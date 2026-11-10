use super::{BackendKind, Registration, Service};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows::ApplicationModel::{
    Activation::ActivationKind, AppInstance, StartupTask, StartupTaskState,
};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows_sys::Win32::Foundation::{
    APPMODEL_ERROR_NO_PACKAGE, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
    ERROR_PATH_NOT_FOUND,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_BINARY, REG_SZ, RRF_NOEXPAND,
    RRF_RT_ANY,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const TASK_ID: &str = "JiffshotStartup";

pub(super) struct SystemService {
    backend: BackendKind,
    exe: PathBuf,
}

impl SystemService {
    pub(super) fn new() -> Result<Self, String> {
        Ok(Self {
            backend: if has_package_identity()? {
                BackendKind::Packaged
            } else {
                BackendKind::Desktop
            },
            exe: std::env::current_exe()
                .map_err(|error| format!("Could not locate Jiffshot's executable: {error}"))?,
        })
    }
}

impl Service for SystemService {
    fn backend(&self) -> BackendKind {
        self.backend
    }

    fn status(&self) -> Result<Registration, String> {
        match self.backend {
            BackendKind::Desktop => desktop_status(&self.exe),
            BackendKind::Packaged => packaged_status(),
        }
    }

    fn enable(&self) -> Result<Registration, String> {
        match self.backend {
            BackendKind::Desktop => {
                let existing = read_run_command()?;
                if let Some(command) = &existing {
                    command_registration(command, &self.exe)?;
                }
                write_run_command(&self.exe, existing.as_deref())?;
                desktop_status(&self.exe)
            }
            BackendKind::Packaged => packaged_enable(),
        }
    }

    fn disable(&self) -> Result<Registration, String> {
        match self.backend {
            BackendKind::Desktop => {
                if let Some(command) = read_run_command()? {
                    command_registration(&command, &self.exe)?;
                    delete_run_command(&command)?;
                }
                desktop_status(&self.exe)
            }
            BackendKind::Packaged => packaged_disable(),
        }
    }

    fn retire_legacy(&self) -> Result<bool, String> {
        if self.backend != BackendKind::Packaged {
            return Ok(false);
        }
        let Some(command) = read_run_command()? else {
            return Ok(false);
        };
        let Some(legacy) = packaged_legacy_command(&command, &self.exe)? else {
            return Ok(false);
        };
        delete_run_command(&command)?;
        Ok(legacy)
    }

    fn has_unresolved_run_entry(&self) -> Result<bool, String> {
        if self.backend == BackendKind::Packaged {
            Ok(read_run_command()?.is_some())
        } else {
            Ok(false)
        }
    }

    fn allow_default(&self) -> Result<bool, String> {
        legacy_approval_allows_default()
    }
}

fn has_package_identity() -> Result<bool, String> {
    use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;

    let mut length = 0;
    // SAFETY: this is the documented size query; the output buffer is null.
    let result = unsafe { GetCurrentPackageFullName(&mut length, std::ptr::null_mut()) };
    match result {
        APPMODEL_ERROR_NO_PACKAGE => Ok(false),
        ERROR_INSUFFICIENT_BUFFER => Ok(true),
        error => Err(format!(
            "Could not determine Jiffshot's package identity: {}",
            std::io::Error::from_raw_os_error(error as i32)
        )),
    }
}

fn ensure_mta_usage() -> Result<(), String> {
    use std::sync::Mutex;
    use windows::Win32::System::Com::CoIncrementMTAUsage;

    static RETAINED: Mutex<bool> = Mutex::new(false);
    let mut retained = RETAINED.lock().expect("MTA usage lock poisoned");
    if !*retained {
        // SAFETY: retains one process-wide MTA usage cookie for the life of the process so
        // per-worker RoUninitialize calls never drop the process MTA refcount to zero or
        // unload proxy-stub DLLs (such as OneCoreCommonProxyStub.dll) behind agile factories
        // cached in process-wide statics (`windows` FactoryCache, `execmodelclient`).
        let _cookie = unsafe { CoIncrementMTAUsage() }
            .map_err(|error| format!("Could not initialize Windows COM services: {error}"))?;
        *retained = true;
    }
    Ok(())
}

fn with_winrt<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    ensure_mta_usage()?;
    // SAFETY: the successful call is paired with RoUninitialize on this thread.
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
        .map_err(|error| format!("Could not initialize Windows startup services: {error}"))?;
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: paired with the successful RoInitialize on this thread.
            unsafe { RoUninitialize() };
        }
    }
    let _apartment = Apartment;
    work()
}

fn on_winrt<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    std::thread::Builder::new()
        .name("jiffshot-startup".into())
        .spawn(move || with_winrt(work))
        .map_err(|error| format!("Could not start the Windows startup service: {error}"))?
        .join()
        .map_err(|_| "The Windows startup service stopped unexpectedly.".to_owned())?
}

fn task() -> Result<StartupTask, String> {
    StartupTask::GetAsync(&TASK_ID.into())
        .and_then(|operation| operation.join())
        .map_err(|error| format!("Could not access Jiffshot's Windows startup task: {error}"))
}

fn registration(state: StartupTaskState) -> Result<Registration, String> {
    match state {
        StartupTaskState::Disabled => Ok(Registration::Disabled),
        StartupTaskState::Enabled => Ok(Registration::Enabled),
        StartupTaskState::DisabledByUser => Ok(Registration::DisabledByUser),
        StartupTaskState::DisabledByPolicy => Ok(Registration::DisabledByPolicy),
        StartupTaskState::EnabledByPolicy => Ok(Registration::EnabledByPolicy),
        _ => Err(format!(
            "Windows returned an unknown startup-task state: {}",
            state.0
        )),
    }
}

fn packaged_status() -> Result<Registration, String> {
    on_winrt(|| {
        let state = task()?
            .State()
            .map_err(|error| format!("Could not read the Windows startup task: {error}"))?;
        registration(state)
    })
}

fn packaged_enable() -> Result<Registration, String> {
    on_winrt(|| {
        let state = task()?
            .RequestEnableAsync()
            .and_then(|operation| operation.join())
            .map_err(|error| format!("Could not enable the Windows startup task: {error}"))?;
        registration(state)
    })
}

fn packaged_disable() -> Result<Registration, String> {
    on_winrt(|| {
        let task = task()?;
        task.Disable()
            .map_err(|error| format!("Could not disable the Windows startup task: {error}"))?;
        registration(
            task.State()
                .map_err(|error| format!("Could not read the Windows startup task: {error}"))?,
        )
    })
}

fn classify_activated_args<T>(
    result: windows::core::Result<T>,
    kind_of: impl FnOnce(T) -> windows::core::Result<ActivationKind>,
) -> Result<bool, String> {
    let args = match result {
        Ok(args) => args,
        // A packaged desktop process started directly without UWP contract activation returns
        // S_OK with a null IActivatedEventArgs pointer, which `windows` maps to HRESULT(0).
        Err(error) if error.code().is_ok() => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Could not read Windows activation details: {error}"
            ));
        }
    };
    let kind = kind_of(args)
        .map_err(|error| format!("Could not read the Windows activation kind: {error}"))?;
    Ok(kind == ActivationKind::StartupTask)
}

pub(super) fn is_startup_activation() -> Result<bool, String> {
    if !has_package_identity()? {
        return Ok(false);
    }
    on_winrt(|| classify_activated_args(AppInstance::GetActivatedEventArgs(), |args| args.Kind()))
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        // SAFETY: this handle came from a successful RegOpenKeyExW or RegCreateKeyExW.
        unsafe { RegCloseKey(self.0) };
    }
}

fn registry_error(action: &str, code: u32) -> String {
    format!(
        "Could not {action} the Windows startup entry: {}",
        std::io::Error::from_raw_os_error(code as i32)
    )
}

fn approval_allows_default(kind: u32, bytes: &[u8]) -> bool {
    // Explorer's marker is not a stable API. Only a known enabled marker is
    // safe to migrate automatically; never write Windows' approval values.
    kind == REG_BINARY && bytes.len() == 12 && bytes[..4] == [2, 0, 0, 0]
}

fn legacy_approval_allows_default() -> Result<bool, String> {
    for location in ["Run", "Run32"] {
        let path = jiffshot_capture::wide(&format!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\{location}"
        ));
        let name = jiffshot_capture::wide(jiffshot_capture::paths::NAME);
        let mut bytes = [0u8; 12];
        let mut size = bytes.len() as u32;
        let mut kind = 0;
        // SAFETY: this reads one value into a fixed-size buffer and never writes it.
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                name.as_ptr(),
                RRF_RT_ANY | RRF_NOEXPAND,
                &mut kind,
                bytes.as_mut_ptr().cast(),
                &mut size,
            )
        };
        match result {
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => {}
            ERROR_MORE_DATA => return Ok(false),
            0 if (size as usize) <= bytes.len()
                && approval_allows_default(kind, &bytes[..size as usize]) => {}
            0 => return Ok(false),
            error => return Err(registry_error("read Windows' approval for", error)),
        }
    }
    Ok(true)
}

fn read_run_command() -> Result<Option<Vec<u16>>, String> {
    let path = jiffshot_capture::wide(RUN_KEY);
    let name = jiffshot_capture::wide(jiffshot_capture::paths::NAME);
    let mut kind = 0;
    let mut size = 0;
    // SAFETY: the first call queries the size and type of one HKCU value.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_ANY | RRF_NOEXPAND,
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if matches!(result, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
        return Ok(None);
    }
    if result != 0 {
        return Err(registry_error("read", result));
    }
    if kind != REG_SZ || size == 0 || !size.is_multiple_of(2) {
        return Err(
            "The Jiffshot Windows startup entry has an unexpected value type or size.".into(),
        );
    }
    let mut command = vec![0u16; size as usize / 2 + 1];
    let mut capacity = (command.len() * 2) as u32;
    // SAFETY: the buffer has `capacity` bytes and remains writable for the call.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_ANY | RRF_NOEXPAND,
            &mut kind,
            command.as_mut_ptr().cast(),
            &mut capacity,
        )
    };
    if result != 0 {
        return Err(registry_error("read", result));
    }
    if kind != REG_SZ || capacity == 0 || !capacity.is_multiple_of(2) {
        return Err("The Jiffshot Windows startup entry changed while it was being read.".into());
    }
    command.truncate(capacity as usize / 2);
    if command.last() == Some(&0) {
        command.pop();
    }
    if command.contains(&0) {
        return Err("The Jiffshot Windows startup entry contains an invalid command.".into());
    }
    Ok(Some(command))
}

fn command_arguments(command: &[u16]) -> Result<Vec<OsString>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    if command.is_empty() || command.contains(&0) {
        return Err("The Windows startup entry contains an empty or invalid command.".into());
    }
    let mut terminated = command.to_vec();
    terminated.push(0);
    let mut count = 0;
    // SAFETY: the command is nul-terminated and count is writable.
    let argv = unsafe { CommandLineToArgvW(terminated.as_ptr(), &mut count) };
    if argv.is_null() {
        return Err(format!(
            "Could not parse the Windows startup command: {}",
            std::io::Error::last_os_error()
        ));
    }
    struct Arguments(*mut *mut u16);
    impl Drop for Arguments {
        fn drop(&mut self) {
            // SAFETY: CommandLineToArgvW allocates the argument array with LocalAlloc.
            unsafe { LocalFree(self.0.cast()) };
        }
    }
    let _arguments = Arguments(argv);
    let mut result = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: the returned array has `count` nul-terminated UTF-16 strings.
        let arg = unsafe { *argv.add(index as usize) };
        let mut length = 0;
        // SAFETY: each returned string is nul-terminated.
        while unsafe { *arg.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: the string has the measured number of UTF-16 units.
        result.push(OsString::from_wide(unsafe {
            std::slice::from_raw_parts(arg, length)
        }));
    }
    Ok(result)
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    same_text(left.as_os_str(), right.as_os_str())
}

fn same_text(left: &OsStr, right: &OsStr) -> bool {
    use windows_sys::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};

    let left: Vec<u16> = left.encode_wide().collect();
    let right: Vec<u16> = right.encode_wide().collect();
    let (Ok(left_len), Ok(right_len)) = (i32::try_from(left.len()), i32::try_from(right.len()))
    else {
        return false;
    };
    // SAFETY: both slices have the lengths supplied and are valid for the duration of the call.
    unsafe {
        CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) == CSTR_EQUAL
    }
}

fn package_family(full_name: &OsStr) -> Result<Option<OsString>, String> {
    use windows_sys::Win32::Storage::Packaging::Appx::PackageFamilyNameFromFullName;

    let full_name: Vec<u16> = full_name.encode_wide().chain(Some(0)).collect();
    let mut length = 0;
    // SAFETY: this is a size query for a nul-terminated package full name.
    let result = unsafe {
        PackageFamilyNameFromFullName(full_name.as_ptr(), &mut length, std::ptr::null_mut())
    };
    if result != ERROR_INSUFFICIENT_BUFFER {
        return Ok(None);
    }
    let mut family = vec![0u16; length as usize];
    // SAFETY: family is writable for the requested number of UTF-16 units.
    let result = unsafe {
        PackageFamilyNameFromFullName(full_name.as_ptr(), &mut length, family.as_mut_ptr())
    };
    if result != 0 {
        return Err(format!(
            "Could not read the package identity in the startup entry: {}",
            std::io::Error::from_raw_os_error(result as i32)
        ));
    }
    family.truncate(length as usize);
    if family.last() == Some(&0) {
        family.pop();
    }
    Ok(Some(OsString::from_wide(&family)))
}

fn packaged_legacy_command(command: &[u16], exe: &Path) -> Result<Option<bool>, String> {
    let arguments = command_arguments(command)?;
    let Some(candidate) = arguments.first().map(Path::new) else {
        return Ok(None);
    };
    let Some((candidate_name, current_name)) = candidate.file_name().zip(exe.file_name()) else {
        return Ok(None);
    };
    if !same_text(candidate_name, current_name) {
        return Ok(None);
    }
    let Some((candidate_dir, current_dir)) = candidate.parent().zip(exe.parent()) else {
        return Ok(None);
    };
    let Some((candidate_root, current_root)) = candidate_dir.parent().zip(current_dir.parent())
    else {
        return Ok(None);
    };
    if !same_path(candidate_root, current_root) {
        return Ok(None);
    }
    let Some((candidate_package, current_package)) =
        candidate_dir.file_name().zip(current_dir.file_name())
    else {
        return Ok(None);
    };
    let current_family = package_family(current_package)?
        .ok_or_else(|| "Could not identify Jiffshot's current Windows package.".to_owned())?;
    let Some(candidate_family) = package_family(candidate_package)? else {
        return Ok(None);
    };
    if !same_text(&candidate_family, &current_family) {
        return Ok(None);
    }
    match arguments.as_slice() {
        [_] => Ok(Some(true)),
        [_, flag] if flag == OsStr::new("--background") => Ok(Some(false)),
        _ => Ok(None),
    }
}

fn desktop_status(exe: &Path) -> Result<Registration, String> {
    let Some(command) = read_run_command()? else {
        return Ok(Registration::Disabled);
    };
    command_registration(&command, exe)
}

fn command_registration(command: &[u16], exe: &Path) -> Result<Registration, String> {
    let arguments = command_arguments(command)?;
    if arguments
        .first()
        .is_none_or(|value| !same_path(Path::new(value), exe))
    {
        return Err(
            "The Jiffshot startup entry belongs to a different executable; it was not changed."
                .into(),
        );
    }
    match arguments.as_slice() {
        [_] => Ok(Registration::EnabledLegacy),
        [_, flag] if flag == OsStr::new("--background") => Ok(Registration::Enabled),
        _ => Err("The Jiffshot startup entry has custom arguments; it was not changed.".into()),
    }
}

fn run_command(exe: &Path) -> Result<Vec<u16>, String> {
    let path: Vec<u16> = exe.as_os_str().encode_wide().collect();
    if path.is_empty() || path.contains(&0) || path.contains(&(b'"' as u16)) {
        return Err(
            "Jiffshot's executable path cannot be used in a Windows startup command.".into(),
        );
    }
    let mut command = vec![b'"' as u16];
    command.extend(path);
    command.extend("\" --background".encode_utf16());
    command.push(0);
    Ok(command)
}

fn write_run_command(exe: &Path, expected: Option<&[u16]>) -> Result<(), String> {
    let value = run_command(exe)?;
    let mut handle = std::ptr::null_mut();
    // SAFETY: opens or creates the current user's Run key with query and write access.
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            jiffshot_capture::wide(RUN_KEY).as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            std::ptr::null(),
            &mut handle,
            std::ptr::null_mut(),
        )
    };
    if result != 0 {
        return Err(registry_error("open", result));
    }
    let key = RegistryKey(handle);
    if read_run_command()?.as_deref() != expected {
        return Err(
            "The Windows startup entry changed before it could be updated; it was not overwritten."
                .into(),
        );
    }
    let bytes = u32::try_from(value.len() * 2)
        .map_err(|_| "Jiffshot's startup command is too long.".to_owned())?;
    // SAFETY: value is a nul-terminated UTF-16 buffer of `bytes` bytes.
    let result = unsafe {
        RegSetValueExW(
            key.0,
            jiffshot_capture::wide(jiffshot_capture::paths::NAME).as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            bytes,
        )
    };
    if result != 0 {
        return Err(registry_error("write", result));
    }
    Ok(())
}

fn delete_run_command(expected: &[u16]) -> Result<(), String> {
    let mut handle = std::ptr::null_mut();
    // SAFETY: opens the current user's Run key for deletion without creating it.
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            jiffshot_capture::wide(RUN_KEY).as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut handle,
        )
    };
    if matches!(result, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
        return Ok(());
    }
    if result != 0 {
        return Err(registry_error("open", result));
    }
    let key = RegistryKey(handle);
    if read_run_command()?.as_deref() != Some(expected) {
        return Err(
            "The Windows startup entry changed before it could be removed; it was not deleted."
                .into(),
        );
    }
    // SAFETY: removes only Jiffshot's named value from the open Run key.
    let result = unsafe {
        RegDeleteValueW(
            key.0,
            jiffshot_capture::wide(jiffshot_capture::paths::NAME).as_ptr(),
        )
    };
    if result != 0 && result != ERROR_FILE_NOT_FOUND {
        return Err(registry_error("remove", result));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn startup_task_operations_use_a_fresh_mta_from_a_single_threaded_caller() {
        std::thread::spawn(|| {
            use windows::Win32::System::WinRT::RO_INIT_SINGLETHREADED;
            use windows_sys::Win32::System::Com::{CoGetApartmentType, APTTYPE_MTA};

            // SAFETY: this new thread's successful initialization is balanced below.
            unsafe { RoInitialize(RO_INIT_SINGLETHREADED) }
                .expect("initialize the caller's single-threaded apartment");
            struct Apartment;
            impl Drop for Apartment {
                fn drop(&mut self) {
                    // SAFETY: this thread successfully called RoInitialize above.
                    unsafe { RoUninitialize() };
                }
            }
            let _apartment = Apartment;
            let caller = std::thread::current().id();
            let (worker, kind) = on_winrt(|| {
                let mut kind = 0;
                let mut qualifier = 0;
                // SAFETY: both output pointers refer to writable apartment values.
                let result = unsafe { CoGetApartmentType(&mut kind, &mut qualifier) };
                if result < 0 {
                    return Err(format!("could not read the worker apartment: {result:#x}"));
                }
                Ok((std::thread::current().id(), kind))
            })
            .expect("start the Windows Runtime worker");
            assert_ne!(worker, caller);
            assert_eq!(kind, APTTYPE_MTA);
        })
        .join()
        .expect("the single-threaded caller completed");
    }

    #[test]
    fn startup_task_worker_propagates_operation_errors() {
        let error = on_winrt(|| Err::<(), _>("startup task failed".to_owned()))
            .expect_err("the operation error must not be hidden");
        assert_eq!(error, "startup task failed");
    }

    #[test]
    fn startup_task_states_preserve_windows_user_and_policy_restrictions() {
        for (state, expected) in [
            (StartupTaskState::Disabled, Registration::Disabled),
            (StartupTaskState::Enabled, Registration::Enabled),
            (
                StartupTaskState::DisabledByUser,
                Registration::DisabledByUser,
            ),
            (
                StartupTaskState::DisabledByPolicy,
                Registration::DisabledByPolicy,
            ),
            (
                StartupTaskState::EnabledByPolicy,
                Registration::EnabledByPolicy,
            ),
        ] {
            assert_eq!(registration(state).expect("known task state"), expected);
        }
    }

    #[test]
    fn unknown_startup_task_state_is_reported() {
        let error = registration(StartupTaskState(i32::MAX))
            .expect_err("an unknown task state must not be treated as enabled or disabled");
        assert!(error.contains(&i32::MAX.to_string()));
    }

    #[test]
    fn unpackaged_process_is_not_a_windows_startup_task_activation() {
        assert!(
            !has_package_identity().expect("query the test process package identity"),
            "the cargo test executable must be unpackaged"
        );
        assert!(
            !is_startup_activation().expect("classify the unpackaged test process"),
            "an unpackaged process cannot have MSIX startup-task activation"
        );
    }

    #[test]
    fn direct_packaged_launch_without_activated_args_is_not_startup_activation() {
        use windows::core::{Error, HRESULT};

        assert_eq!(
            classify_activated_args(Ok(()), |()| Ok(ActivationKind::StartupTask)),
            Ok(true)
        );
        assert_eq!(
            classify_activated_args(Ok(()), |()| Ok(ActivationKind::Launch)),
            Ok(false)
        );
        assert_eq!(
            classify_activated_args::<()>(Err(Error::empty()), |()| {
                unreachable!("null IActivatedEventArgs returns early")
            }),
            Ok(false)
        );
        let read_err = classify_activated_args::<()>(
            Err(Error::from_hresult(HRESULT(0x8000_4005u32 as i32))),
            |()| unreachable!("failed activation lookup returns early"),
        )
        .expect_err("real activation HRESULT failure must be reported");
        assert!(read_err.contains("Could not read Windows activation details"));
        let kind_err = classify_activated_args(Ok(()), |()| {
            Err(Error::from_hresult(HRESULT(0x8000_4005u32 as i32)))
        })
        .expect_err("failed Kind() lookup must be reported");
        assert!(kind_err.contains("Could not read the Windows activation kind"));
    }

    #[test]
    fn desktop_command_quotes_spaces_and_starts_in_background() {
        let exe = Path::new("C:\\Program Files\\Jiffshot\\jiffshot.exe");
        let mut command = run_command(exe).unwrap();
        assert_eq!(command.pop(), Some(0));
        assert_eq!(
            command_arguments(&command).unwrap(),
            [exe.as_os_str(), OsStr::new("--background")]
        );
        assert_eq!(
            command_registration(&command, exe).unwrap(),
            Registration::Enabled
        );
    }

    #[test]
    fn desktop_command_migrates_only_our_legacy_entry() {
        let exe = Path::new("C:\\Jiffshot\\jiffshot.exe");
        assert!(command_registration(&[], exe).is_err());
        assert_eq!(
            command_registration(&utf16("\"c:\\JIFFSHOT\\jiffshot.exe\""), exe).unwrap(),
            Registration::EnabledLegacy
        );
        assert!(
            command_registration(&utf16("\"C:\\Other\\jiffshot.exe\" --background"), exe).is_err()
        );
        assert!(
            command_registration(&utf16("\"C:\\Jiffshot\\jiffshot.exe\" --window"), exe).is_err()
        );
    }

    #[test]
    fn desktop_command_preserves_unpaired_utf16() {
        let exe = PathBuf::from(OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
            b'\\' as u16,
            b'j' as u16,
            b'.' as u16,
            b'e' as u16,
            b'x' as u16,
            b'e' as u16,
        ]));
        let mut command = run_command(&exe).unwrap();
        assert_eq!(command.pop(), Some(0));
        assert_eq!(
            command_arguments(&command).unwrap(),
            [exe.as_os_str(), OsStr::new("--background")]
        );
    }

    #[test]
    fn packaged_migration_only_retires_the_same_package_family() {
        let exe = Path::new(
            "Z:\\WindowsApps\\Devolutions.Jiffshot_0.7.0.0_x64__8wekyb3d8bbwe\\jiffshot.exe",
        );
        let old = utf16(
            "\"Z:\\WindowsApps\\Devolutions.Jiffshot_0.4.0.0_x64__8wekyb3d8bbwe\\jiffshot.exe\"",
        );
        assert_eq!(packaged_legacy_command(&old, exe).unwrap(), Some(true));
        let other =
            utf16("\"Z:\\WindowsApps\\Other.Jiffshot_0.4.0.0_x64__8wekyb3d8bbwe\\jiffshot.exe\"");
        assert_eq!(packaged_legacy_command(&other, exe).unwrap(), None);
        let custom = utf16(
            "\"Z:\\WindowsApps\\Devolutions.Jiffshot_0.4.0.0_x64__8wekyb3d8bbwe\\jiffshot.exe\" --window",
        );
        assert_eq!(packaged_legacy_command(&custom, exe).unwrap(), None);
    }

    #[test]
    fn only_a_known_enabled_windows_approval_marker_allows_automatic_migration() {
        let mut marker = [0u8; 12];
        marker[0] = 2;
        assert!(approval_allows_default(REG_BINARY, &marker));
        marker[0] = 3;
        assert!(!approval_allows_default(REG_BINARY, &marker));
        marker[0] = 6;
        assert!(!approval_allows_default(REG_BINARY, &marker));
        marker[0] = 2;
        assert!(!approval_allows_default(REG_SZ, &marker));
        assert!(!approval_allows_default(REG_BINARY, &marker[..4]));
    }

    const PROXY_CHILD: &str =
        "startup_windows::backend::tests::cached_startup_factory_survives_across_workers_child";
    const PROXY_CHILD_ENV: &str = "JIFFSHOT_PRIVATE_STARTUP_PROXY_TEST";

    #[test]
    fn cached_startup_factory_survives_across_workers_and_preserves_main_sta() {
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", PROXY_CHILD, "--ignored", "--nocapture"])
            .env(PROXY_CHILD_ENV, "1")
            .output()
            .expect("start isolated startup proxy test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "isolated startup proxy test failed (status {:?}):\n{stdout}\n{stderr}",
            output.status.code()
        );
    }

    #[test]
    #[ignore = "run only in the dedicated subprocess started by the parent test"]
    fn cached_startup_factory_survives_across_workers_child() {
        use std::ffi::c_void;
        use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
        use windows::core::{Interface, GUID, HRESULT, HSTRING};
        use windows::ApplicationModel::{IStartupTaskStatics, IStartupTaskStatics_Vtbl};
        use windows::Win32::System::Com::{
            CoFreeUnusedLibrariesEx, CoGetApartmentType, CoInitializeEx, CoUninitialize,
            IAgileObject, APTTYPE_MAINSTA, APTTYPE_STA, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_QUIT,
        };

        #[link(name = "ole32")]
        extern "system" {
            fn CoMarshalInterThreadInterfaceInStream(
                riid: *const GUID,
                p_unk: *mut c_void,
                pp_stm: *mut *mut c_void,
            ) -> HRESULT;
            fn CoGetInterfaceAndReleaseStream(
                p_stm: *mut c_void,
                riid: *const GUID,
                ppv: *mut *mut c_void,
            ) -> HRESULT;
        }

        assert_eq!(std::env::var(PROXY_CHILD_ENV).as_deref(), Ok("1"));

        const SENTINEL_HR: HRESULT = HRESULT(0x8004_0201u32 as i32);

        #[repr(C)]
        struct StubFactory {
            vtbl: *const IStartupTaskStatics_Vtbl,
            refs: AtomicU32,
            calls: AtomicU32,
        }

        unsafe extern "system" fn query_interface(
            this: *mut c_void,
            iid: *const GUID,
            interface: *mut *mut c_void,
        ) -> HRESULT {
            if iid.is_null() || interface.is_null() {
                return HRESULT(0x8000_4003u32 as i32);
            }
            // SAFETY: caller supplied valid non-null pointers for a COM QueryInterface call.
            let requested = unsafe { *iid };
            if requested == windows::core::IUnknown::IID
                || requested == windows::core::IInspectable::IID
                || requested == IAgileObject::IID
                || requested == IStartupTaskStatics::IID
            {
                // SAFETY: `this` is a valid `StubFactory` instance.
                unsafe {
                    add_ref(this);
                    *interface = this;
                }
                HRESULT(0)
            } else {
                // SAFETY: `interface` was checked for null above.
                unsafe { *interface = std::ptr::null_mut() };
                HRESULT(0x8000_4002u32 as i32)
            }
        }

        unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
            // SAFETY: `this` points to a heap-allocated `StubFactory`.
            let stub = unsafe { &*(this as *const StubFactory) };
            stub.refs.fetch_add(1, Ordering::Relaxed) + 1
        }

        unsafe extern "system" fn release(this: *mut c_void) -> u32 {
            // SAFETY: `this` points to a heap-allocated `StubFactory` with balanced refcounts.
            let stub = unsafe { &*(this as *const StubFactory) };
            let remaining = stub.refs.fetch_sub(1, Ordering::Release) - 1;
            if remaining == 0 {
                std::sync::atomic::fence(Ordering::Acquire);
                // SAFETY: the last reference was just released.
                unsafe { drop(Box::from_raw(this as *mut StubFactory)) };
            }
            remaining
        }

        unsafe extern "system" fn get_iids(
            _this: *mut c_void,
            count: *mut u32,
            iids: *mut *mut GUID,
        ) -> HRESULT {
            // SAFETY: WinRT metadata callers pass valid out-pointers.
            unsafe {
                if !count.is_null() {
                    *count = 0;
                }
                if !iids.is_null() {
                    *iids = std::ptr::null_mut();
                }
            }
            HRESULT(0)
        }

        unsafe extern "system" fn get_runtime_class_name(
            _this: *mut c_void,
            name: *mut *mut c_void,
        ) -> HRESULT {
            // SAFETY: out-pointer is valid when non-null.
            unsafe {
                if !name.is_null() {
                    *name = std::ptr::null_mut();
                }
            }
            HRESULT(0)
        }

        unsafe extern "system" fn get_trust_level(_this: *mut c_void, level: *mut i32) -> HRESULT {
            // SAFETY: out-pointer is valid when non-null.
            unsafe {
                if !level.is_null() {
                    *level = 0;
                }
            }
            HRESULT(0)
        }

        unsafe extern "system" fn get_for_current_package_async(
            _this: *mut c_void,
            result: *mut *mut c_void,
        ) -> HRESULT {
            // SAFETY: out-pointer is valid when non-null.
            unsafe {
                if !result.is_null() {
                    *result = std::ptr::null_mut();
                }
            }
            SENTINEL_HR
        }

        unsafe extern "system" fn get_async(
            this: *mut c_void,
            _task_id: *mut c_void,
            result: *mut *mut c_void,
        ) -> HRESULT {
            // SAFETY: `this` points to our `StubFactory` and `result` is the COM out-pointer.
            let stub = unsafe { &*(this as *const StubFactory) };
            stub.calls.fetch_add(1, Ordering::SeqCst);
            // SAFETY: out-pointer is valid when non-null.
            unsafe {
                if !result.is_null() {
                    *result = std::ptr::null_mut();
                }
            }
            SENTINEL_HR
        }

        static VTBL: IStartupTaskStatics_Vtbl = IStartupTaskStatics_Vtbl {
            base__: windows::core::IInspectable_Vtbl {
                base: windows::core::IUnknown_Vtbl {
                    QueryInterface: query_interface,
                    AddRef: add_ref,
                    Release: release,
                },
                GetIids: get_iids,
                GetRuntimeClassName: get_runtime_class_name,
                GetTrustLevel: get_trust_level,
            },
            GetForCurrentPackageAsync: get_for_current_package_async,
            GetAsync: get_async,
        };

        let (tx, rx) = std::sync::mpsc::channel::<(usize, u32)>();
        let server = std::thread::spawn(move || {
            // SAFETY: balanced with CoUninitialize before this dedicated STA thread exits.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
                .ok()
                .expect("initialize STA server");
            // SAFETY: obtains this STA thread's Win32 thread identifier for WM_QUIT.
            let tid = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
            let raw = Box::into_raw(Box::new(StubFactory {
                vtbl: &VTBL,
                refs: AtomicU32::new(1),
                calls: AtomicU32::new(0),
            }));
            // SAFETY: `raw` is a valid COM object with refcount 1; `from_raw` adopts ownership.
            let factory: IStartupTaskStatics = unsafe { IStartupTaskStatics::from_raw(raw.cast()) };
            let mut stream = std::ptr::null_mut();
            // SAFETY: marshals the STA interface into a cross-apartment stream so MTA resolves CStdProxyBuffer.
            unsafe {
                CoMarshalInterThreadInterfaceInStream(
                    &IStartupTaskStatics::IID,
                    factory.as_raw(),
                    &mut stream,
                )
            }
            .ok()
            .expect("marshal IStartupTaskStatics from STA");
            tx.send((stream as usize, tid))
                .expect("send marshaled stream");

            let mut msg: MSG = unsafe { std::mem::zeroed() };
            // SAFETY: standard STA message loop until the test posts WM_QUIT.
            while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                // SAFETY: translates and dispatches the retrieved message on this STA thread.
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            let calls = factory.as_raw() as *const StubFactory;
            // SAFETY: `factory` is still alive until dropped below.
            let count = unsafe { (*calls).calls.load(Ordering::SeqCst) };
            drop(factory);
            // SAFETY: balances CoInitializeEx on this STA thread.
            unsafe { CoUninitialize() };
            count
        });

        let (stream_raw, server_tid) = rx.recv().expect("receive marshaled stream");
        static CACHED_FACTORY: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

        // First worker (`packaged_status`): unmarshals the `OneCoreCommonProxyStub.dll`
        // proxy in the MTA, caches it in a process-wide static just like `windows_core::imp::FactoryCache`,
        // and invokes `GetAsync` before `on_winrt` exits.
        let first = on_winrt(move || {
            let mut raw_proxy = std::ptr::null_mut();
            // SAFETY: consumes the single-use stream and creates the MTA proxy for IStartupTaskStatics.
            unsafe {
                CoGetInterfaceAndReleaseStream(
                    stream_raw as *mut c_void,
                    &IStartupTaskStatics::IID,
                    &mut raw_proxy,
                )
            }
            .ok()
            .map_err(|e| e.to_string())?;
            // SAFETY: `raw_proxy` is a valid newly-unmarshaled `IStartupTaskStatics` pointer.
            let proxy: IStartupTaskStatics = unsafe { IStartupTaskStatics::from_raw(raw_proxy) };
            proxy
                .cast::<IAgileObject>()
                .expect("standard COM proxy is agile, matching FactoryCache::call");
            CACHED_FACTORY.store(proxy.as_raw(), Ordering::SeqCst);
            let task_id = HSTRING::from(TASK_ID);
            let mut op = std::ptr::null_mut();
            // SAFETY: invokes GetAsync on the freshly unmarshaled proxy.
            let hr = unsafe {
                (Interface::vtable(&proxy).GetAsync)(
                    proxy.as_raw(),
                    task_id.as_ptr().cast_mut().cast(),
                    &mut op,
                )
            };
            std::mem::forget(proxy);
            Ok(hr)
        })
        .expect("first on_winrt call");
        assert_eq!(first, SENTINEL_HR);

        // Force immediate unload of any COM proxy-stub DLLs eligible after MTA teardown
        // (deterministic equivalent of combase's MTA shutdown / background unload).
        // SAFETY: standard COM API with zero delay.
        unsafe { CoFreeUnusedLibrariesEx(0, Some(0)) };

        // Second worker (`packaged_enable`): reuses the cached proxy pointer from `CACHED_FACTORY`,
        // exactly as `FactoryCache` does on the second `StartupTask::GetAsync` call.
        let second = on_winrt(|| {
            let ptr = CACHED_FACTORY.load(Ordering::SeqCst);
            assert!(!ptr.is_null());
            // SAFETY: borrows the leaked `IStartupTaskStatics` pointer cached in `CACHED_FACTORY`.
            let proxy = unsafe { std::mem::transmute::<&*mut c_void, &IStartupTaskStatics>(&ptr) };
            let task_id = HSTRING::from(TASK_ID);
            let mut op = std::ptr::null_mut();
            // SAFETY: invokes GetAsync on the cached proxy across `ObjectStublessClient`.
            let hr = unsafe {
                (Interface::vtable(proxy).GetAsync)(
                    proxy.as_raw(),
                    task_id.as_ptr().cast_mut().cast(),
                    &mut op,
                )
            };
            Ok(hr)
        })
        .expect("second on_winrt call");
        assert_eq!(second, SENTINEL_HR);

        // Verify the main thread can still enter STA afterwards, as Rustolonia requires (`OleInitialize`).
        // SAFETY: balanced with OleUninitialize below on this main thread.
        unsafe { OleInitialize(None) }.expect("main thread enters OLE STA after startup workers");
        let mut apt_type = windows::Win32::System::Com::APTTYPE::default();
        let mut apt_qualifier = windows::Win32::System::Com::APTTYPEQUALIFIER::default();
        // SAFETY: queries the current thread's COM apartment.
        unsafe { CoGetApartmentType(&mut apt_type, &mut apt_qualifier) }
            .expect("query main thread apartment");
        assert!(
            apt_type == APTTYPE_MAINSTA || apt_type == APTTYPE_STA,
            "unexpected main-thread apartment: {apt_type:?}"
        );
        // SAFETY: balances OleInitialize above.
        unsafe { OleUninitialize() };

        // Clean up the STA server thread and ensure that even when the remote server exits and
        // unused COM libraries are flushed (as with an out-of-process broker), the client proxy's
        // retained MTA keeps OneCoreCommonProxyStub.dll mapped so calls return a clean HRESULT
        // instead of faulting in combase!ObjectStublessClient.
        // SAFETY: posts WM_QUIT to our own STA server thread.
        unsafe { PostThreadMessageW(server_tid, WM_QUIT, 0, 0) };
        let total_calls = server.join().expect("join STA server");
        assert_eq!(total_calls, 2);

        // SAFETY: flushes any COM DLLs that lost their last reference.
        unsafe { CoFreeUnusedLibrariesEx(0, Some(0)) };
        let after_server_exit = on_winrt(|| {
            let ptr = CACHED_FACTORY.load(Ordering::SeqCst);
            // SAFETY: borrows the leaked `IStartupTaskStatics` pointer cached in `CACHED_FACTORY`.
            let proxy = unsafe { std::mem::transmute::<&*mut c_void, &IStartupTaskStatics>(&ptr) };
            let task_id = HSTRING::from(TASK_ID);
            let mut op = std::ptr::null_mut();
            // SAFETY: invokes GetAsync on the cached proxy after the server exited.
            let hr = unsafe {
                (Interface::vtable(proxy).GetAsync)(
                    proxy.as_raw(),
                    task_id.as_ptr().cast_mut().cast(),
                    &mut op,
                )
            };
            Ok(hr)
        })
        .expect("third on_winrt call after server exit");
        assert!(
            after_server_exit.is_err(),
            "disconnected server should return a COM error HRESULT, got {after_server_exit:?}"
        );
    }
}

#[cfg(test)]
#[path = "startup_windows_registry_tests.rs"]
mod registry_tests;
