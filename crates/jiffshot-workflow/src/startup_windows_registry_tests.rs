use super::super::{change, initialize, Record, Store};
use super::*;
use std::cell::RefCell;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::System::Registry::{
    RegDeleteTreeW, RegOverridePredefKey, KEY_ALL_ACCESS, REG_CREATED_NEW_KEY, REG_OPTION_VOLATILE,
};

const CHILD: &str = "startup_windows::backend::registry_tests::private_registry_child";
const CHILD_ENV: &str = "JIFFSHOT_PRIVATE_STARTUP_REGISTRY_TEST";
const APPROVAL_RUN: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
const APPROVAL_RUN32: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run32";

fn checked(action: &str, result: u32) -> Result<(), String> {
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "{action}: {}",
            std::io::Error::from_raw_os_error(result as i32)
        ))
    }
}

fn create_key(parent: HKEY, path: &str) -> Result<(RegistryKey, u32), String> {
    let mut handle = std::ptr::null_mut();
    let mut disposition = 0;
    // SAFETY: the path is nul-terminated and both outputs are writable.
    let result = unsafe {
        RegCreateKeyExW(
            parent,
            jiffshot_capture::wide(path).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_VOLATILE,
            KEY_ALL_ACCESS,
            std::ptr::null(),
            &mut handle,
            &mut disposition,
        )
    };
    checked("create scratch registry key", result)?;
    Ok((RegistryKey(handle), disposition))
}

fn set_value(key: HKEY, name: &str, kind: u32, bytes: &[u8]) -> Result<(), String> {
    let size = u32::try_from(bytes.len()).map_err(|_| "scratch value is too large")?;
    // SAFETY: the name is nul-terminated and bytes is readable for size bytes.
    let result = unsafe {
        RegSetValueExW(
            key,
            jiffshot_capture::wide(name).as_ptr(),
            0,
            kind,
            bytes.as_ptr(),
            size,
        )
    };
    checked("write scratch registry value", result)
}

fn matches_probe(key: HKEY, expected: &[u8]) -> Result<bool, String> {
    let mut value = vec![0; expected.len()];
    let mut size = u32::try_from(value.len()).map_err(|_| "scratch probe is too large")?;
    let mut kind = 0;
    // SAFETY: the probe name is nul-terminated and the output buffers are writable.
    let result = unsafe {
        RegGetValueW(
            key,
            std::ptr::null(),
            jiffshot_capture::wide("IsolationProbe").as_ptr(),
            RRF_RT_ANY | RRF_NOEXPAND,
            &mut kind,
            value.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if matches!(result, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
        return Ok(false);
    }
    checked("read scratch registry identity", result)?;
    Ok(kind == REG_BINARY && size as usize == value.len() && value == expected)
}

struct ScratchRegistry {
    software: RegistryKey,
    root: Option<RegistryKey>,
    name: Vec<u16>,
    probe: Vec<u8>,
    mapped: bool,
    created: bool,
}

impl ScratchRegistry {
    fn new() -> Result<Self, String> {
        let mut software = std::ptr::null_mut();
        // SAFETY: the path is nul-terminated and the output handle is writable.
        let result = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                jiffshot_capture::wide("Software").as_ptr(),
                0,
                KEY_ALL_ACCESS,
                &mut software,
            )
        };
        checked("open scratch registry parent", result)?;
        let software = RegistryKey(software);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("read test clock: {error}"))?
            .as_nanos();
        let token = format!("JiffshotStartupTest-{}-{timestamp}", std::process::id());
        let (root, disposition) = create_key(software.0, &token)?;
        if disposition != REG_CREATED_NEW_KEY {
            return Err("scratch registry key already existed; it was not changed".into());
        }
        let mut scratch = Self {
            software,
            root: Some(root),
            name: jiffshot_capture::wide(&token),
            probe: token.into_bytes(),
            mapped: false,
            created: true,
        };
        let root = scratch.root.as_ref().expect("scratch key is open").0;
        set_value(root, "IsolationProbe", REG_BINARY, &scratch.probe)?;
        // SAFETY: the mapping is process-local and root is an open scratch key.
        checked("redirect current-user registry", unsafe {
            RegOverridePredefKey(HKEY_CURRENT_USER, root)
        })?;
        scratch.mapped = true;

        if !matches_probe(HKEY_CURRENT_USER, &scratch.probe)? {
            return Err("current-user registry was not redirected to the scratch key".into());
        }

        // Production opens the existing Run key with non-volatile options.
        // Pre-create volatile test paths so no real startup key is involved.
        for path in [RUN_KEY, APPROVAL_RUN, APPROVAL_RUN32] {
            create_key(root, path)?;
        }
        Ok(scratch)
    }

    fn cleanup(&mut self) -> Result<(), String> {
        if self.mapped {
            // SAFETY: a null replacement restores this process's predefined-key mapping.
            checked("restore current-user registry mapping", unsafe {
                RegOverridePredefKey(HKEY_CURRENT_USER, std::ptr::null_mut())
            })?;
            self.mapped = false;
        }
        if self.created {
            let mut handle = std::ptr::null_mut();
            // SAFETY: software is the original parent and name is nul-terminated.
            let result = unsafe {
                RegOpenKeyExW(
                    self.software.0,
                    self.name.as_ptr(),
                    0,
                    KEY_QUERY_VALUE,
                    &mut handle,
                )
            };
            if matches!(result, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
                self.created = false;
                self.root.take();
                return Ok(());
            }
            checked("open scratch registry for cleanup", result)?;
            let current = RegistryKey(handle);
            if !matches_probe(current.0, &self.probe)? {
                return Err("scratch registry identity changed; it was not removed".into());
            }
            drop(current);
            self.root.take();
            // SAFETY: software is the original parent handle and name identifies
            // only the unique volatile key created by this fixture.
            checked("remove scratch registry key", unsafe {
                RegDeleteTreeW(self.software.0, self.name.as_ptr())
            })?;
            self.created = false;
        }
        Ok(())
    }
}

impl Drop for ScratchRegistry {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("startup registry test cleanup failed: {error}");
        }
    }
}

fn put(path: &str, kind: u32, bytes: &[u8]) {
    let (key, _) = create_key(HKEY_CURRENT_USER, path).expect("open isolated registry key");
    set_value(key.0, jiffshot_capture::paths::NAME, kind, bytes)
        .expect("write isolated registry value");
}

fn put_command(command: &str) {
    let bytes: Vec<u8> = command
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    put(RUN_KEY, REG_SZ, &bytes);
}

fn binary_value(path: &str, length: usize) -> Vec<u8> {
    let (key, _) = create_key(HKEY_CURRENT_USER, path).expect("open isolated registry key");
    let mut value = vec![0; length];
    let mut size = u32::try_from(length).expect("scratch value length fits in u32");
    let mut kind = 0;
    // SAFETY: the name is nul-terminated and the output buffers are writable.
    let result = unsafe {
        RegGetValueW(
            key.0,
            std::ptr::null(),
            jiffshot_capture::wide(jiffshot_capture::paths::NAME).as_ptr(),
            RRF_RT_ANY | RRF_NOEXPAND,
            &mut kind,
            value.as_mut_ptr().cast(),
            &mut size,
        )
    };
    checked("read isolated registry value", result).expect("read isolated value");
    assert_eq!(kind, REG_BINARY);
    assert_eq!(size as usize, length);
    value
}

fn clear(path: &str) {
    let (key, _) = create_key(HKEY_CURRENT_USER, path).expect("open isolated registry key");
    // SAFETY: the value name is nul-terminated and the handle is an isolated key.
    let result = unsafe {
        RegDeleteValueW(
            key.0,
            jiffshot_capture::wide(jiffshot_capture::paths::NAME).as_ptr(),
        )
    };
    assert!(
        result == 0 || result == ERROR_FILE_NOT_FOUND,
        "could not clear isolated registry value: {result}"
    );
}

#[derive(Default)]
struct MemoryStore(RefCell<Option<Record>>);

impl Store for MemoryStore {
    fn load(&self) -> Result<Option<Record>, String> {
        Ok(self.0.borrow().clone())
    }

    fn save(&self, record: &Record) -> Result<(), String> {
        *self.0.borrow_mut() = Some(record.clone());
        Ok(())
    }
}

#[test]
fn startup_registry_backend_runs_in_a_private_child_process() {
    let output = Command::new(std::env::current_exe().expect("find test executable"))
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("start isolated registry test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed"),
        "isolated registry test failed: {stdout}\n{stderr}"
    );
}

#[test]
#[ignore = "run only in the dedicated subprocess started by the parent test"]
fn private_registry_child() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(std::env::var(CHILD_ENV).as_deref(), Ok("1"));
    assert!(
        args.iter().any(|arg| arg == "--exact") && args.iter().any(|arg| arg == CHILD),
        "registry redirection requires an exact single-test subprocess"
    );
    let mut scratch = ScratchRegistry::new().expect("create isolated registry");
    let exe = PathBuf::from("C:\\Program Files\\Jiffshot\\jiffshot.exe");
    let desktop = SystemService {
        backend: BackendKind::Desktop,
        exe: exe.clone(),
    };
    let store = MemoryStore::default();

    assert_eq!(desktop.status().unwrap(), Registration::Disabled);
    assert!(desktop.allow_default().unwrap());
    initialize(&desktop, &store).unwrap();
    assert!(store.0.borrow().as_ref().unwrap().requested);
    assert_eq!(desktop.status().unwrap(), Registration::Enabled);
    let owned = read_run_command().unwrap().unwrap();
    assert_eq!(
        command_arguments(&owned).unwrap(),
        [exe.as_os_str(), OsStr::new("--background")]
    );
    change(&desktop, &store, false).unwrap();
    assert_eq!(desktop.status().unwrap(), Registration::Disabled);
    initialize(&desktop, &store).unwrap();
    assert_eq!(desktop.status().unwrap(), Registration::Disabled);

    put_command("\"C:\\Program Files\\Jiffshot\\jiffshot.exe\"");
    assert_eq!(desktop.status().unwrap(), Registration::EnabledLegacy);
    let migration = initialize(&desktop, &MemoryStore::default()).unwrap();
    assert!(migration.legacy_launch_ambiguous);
    assert_eq!(desktop.status().unwrap(), Registration::Enabled);
    assert_eq!(desktop.disable().unwrap(), Registration::Disabled);

    put_command("\"C:\\Other\\jiffshot.exe\" --background");
    let foreign = read_run_command().unwrap().unwrap();
    assert!(desktop.enable().is_err());
    assert!(desktop.disable().is_err());
    assert!(write_run_command(&exe, Some(&owned)).is_err());
    assert!(delete_run_command(&owned).is_err());
    assert_eq!(read_run_command().unwrap(), Some(foreign));
    clear(RUN_KEY);
    put(RUN_KEY, REG_BINARY, &[1, 2, 3]);
    assert!(desktop.status().is_err());
    assert!(desktop.enable().is_err());
    assert!(desktop.disable().is_err());
    assert_eq!(binary_value(RUN_KEY, 3), [1, 2, 3]);
    clear(RUN_KEY);

    let mut marker = [0u8; 12];
    marker[0] = 3;
    put(APPROVAL_RUN, REG_BINARY, &marker);
    assert!(!desktop.allow_default().unwrap());
    let blocked_store = MemoryStore::default();
    initialize(&desktop, &blocked_store).unwrap();
    assert_eq!(desktop.status().unwrap(), Registration::Disabled);
    assert!(!blocked_store.0.borrow().as_ref().unwrap().requested);
    assert_eq!(binary_value(APPROVAL_RUN, 12), marker);
    marker[0] = 2;
    put(APPROVAL_RUN, REG_BINARY, &marker);
    assert!(desktop.allow_default().unwrap());
    marker[0] = 6;
    put(APPROVAL_RUN32, REG_BINARY, &marker);
    assert!(!desktop.allow_default().unwrap());
    clear(APPROVAL_RUN32);
    put(APPROVAL_RUN, REG_SZ, &[2, 0, 0, 0]);
    assert!(!desktop.allow_default().unwrap());
    put(APPROVAL_RUN, REG_BINARY, &[2; 13]);
    assert!(!desktop.allow_default().unwrap());
    clear(APPROVAL_RUN);

    let token = std::str::from_utf8(&scratch.probe).expect("scratch token is ASCII");
    let package_root = format!("C:\\{token}\\WindowsApps");
    let old_exe =
        format!("{package_root}\\Devolutions.Jiffshot_0.4.0.0_x64__8wekyb3d8bbwe\\jiffshot.exe");
    let packaged = SystemService {
        backend: BackendKind::Packaged,
        exe: PathBuf::from(format!(
            "{package_root}\\Devolutions.Jiffshot_0.7.0.0_x64__8wekyb3d8bbwe\\jiffshot.exe"
        )),
    };
    put_command(&format!("\"{old_exe}\""));
    assert!(packaged.has_unresolved_run_entry().unwrap());
    assert!(packaged.retire_legacy().unwrap());
    assert!(!packaged.has_unresolved_run_entry().unwrap());
    put_command(&format!("\"{old_exe}\" --background"));
    assert!(!packaged.retire_legacy().unwrap());
    assert!(!packaged.has_unresolved_run_entry().unwrap());
    put_command(&format!("\"{old_exe}\" --window"));
    let custom = read_run_command().unwrap();
    assert!(!packaged.retire_legacy().unwrap());
    assert_eq!(read_run_command().unwrap(), custom);
    assert!(packaged.has_unresolved_run_entry().unwrap());

    scratch.cleanup().expect("remove isolated registry");
    let mut handle = std::ptr::null_mut();
    // SAFETY: software is the original parent and name is nul-terminated.
    let result = unsafe {
        RegOpenKeyExW(
            scratch.software.0,
            scratch.name.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut handle,
        )
    };
    if result == 0 {
        drop(RegistryKey(handle));
    }
    assert!(
        matches!(result, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND),
        "scratch registry key still exists or could not be checked: {result}"
    );
}
