//! Opt-in XDG desktop autostart for the current user.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const AUTOSTART_FILE: &str = "com.devolutions.Jiffshot.desktop";
const OWNERSHIP_MARKER: &str = "X-Jiffshot-Autostart=true";
const PACKAGED_LAUNCHER_ENV: &str = "JIFFSHOT_LAUNCHER";

pub(super) fn enable() -> Result<(), String> {
    let launcher = match std::env::var_os(PACKAGED_LAUNCHER_ENV) {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()
            .map_err(|error| format!("could not locate the Jiffshot executable: {error}"))?,
    };
    enable_in(&autostart_directory()?, &launcher)
}

pub(super) fn disable() -> Result<(), String> {
    disable_in(&autostart_directory()?)
}

fn autostart_directory() -> Result<PathBuf, String> {
    dirs::config_dir()
        .map(|directory| directory.join("autostart"))
        .ok_or_else(|| "could not find the user's configuration folder".to_owned())
}

fn enable_in(directory: &Path, launcher: &Path) -> Result<(), String> {
    if !launcher.is_absolute() {
        return Err("the Jiffshot startup executable must be an absolute path".to_owned());
    }
    let metadata = std::fs::metadata(launcher)
        .map_err(|error| format!("could not inspect {}: {error}", launcher.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!("{} is not an executable file", launcher.display()));
    }
    let executable = quote_executable(launcher)?;
    // GLib rejects percent field codes in the executable position, even escaped `%%`.
    write_entry(
        directory,
        &format!(
            "[Desktop Entry]\nType=Application\nName=Jiffshot\nExec=/usr/bin/env {executable} --background\n\
             Terminal=false\nNoDisplay=true\n{OWNERSHIP_MARKER}\n"
        ),
    )
}

fn disable_in(directory: &Path) -> Result<(), String> {
    // GLib validates Exec even for hidden entries; portable installs need no PATH entry.
    write_entry(
        directory,
        &format!(
            "[Desktop Entry]\nType=Application\nName=Jiffshot\nExec=/usr/bin/true\n\
             Hidden=true\n{OWNERSHIP_MARKER}\n"
        ),
    )
}

fn quote_executable(path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or_else(|| "the startup executable path is not valid UTF-8".to_owned())?;
    if path
        .chars()
        .any(|character| character.is_control() || character == '=')
    {
        return Err("the startup executable path contains an unsupported character".to_owned());
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for character in path.chars() {
        match character {
            '\\' => quoted.push_str("\\\\\\\\"),
            '"' | '$' | '`' => {
                quoted.push_str("\\\\");
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

fn write_entry(directory: &Path, contents: &str) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let path = directory.join(AUTOSTART_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!("{} is not a regular file", path.display()));
        }
        Ok(_) => {
            let existing = std::fs::read_to_string(&path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
            if !existing.lines().any(|line| line == OWNERSHIP_MARKER) {
                return Err(format!(
                    "{} is not managed by Jiffshot; it was not changed",
                    path.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
    }
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .map_err(|error| format!("could not create a startup entry: {error}"))?;
    temporary
        .write_all(contents.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("could not write the startup entry: {error}"))?;
    temporary
        .persist(&path)
        .map_err(|error| format!("could not save {}: {}", path.display(), error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn executable(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("test launcher should be written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("test launcher should be executable");
        path
    }

    #[test]
    fn enabling_and_disabling_writes_a_managed_desktop_override() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let launcher = executable(directory.path(), "jiffshot launcher");
        let autostart = directory.path().join("autostart");
        enable_in(&autostart, &launcher).expect("startup should be enabled");
        let path = autostart.join(AUTOSTART_FILE);
        let enabled = std::fs::read_to_string(&path).expect("startup entry should be readable");
        assert!(enabled.contains(&format!(
            "Exec=/usr/bin/env \"{}\" --background\n",
            launcher.display()
        )));
        assert!(enabled.contains(OWNERSHIP_MARKER));
        assert!(!enabled.contains("Hidden=true"));

        disable_in(&autostart).expect("startup should be disabled");
        let disabled = std::fs::read_to_string(&path).expect("disabled entry should be readable");
        assert!(disabled.contains("Hidden=true\n"));
        assert!(disabled.contains("Exec=/usr/bin/true\n"));
        assert!(disabled.contains(OWNERSHIP_MARKER));
        enable_in(&autostart, &launcher).expect("startup should be re-enabled");
        assert!(!std::fs::read_to_string(path)
            .unwrap()
            .contains("Hidden=true"));
    }

    #[test]
    fn desktop_exec_escaping_preserves_reserved_characters() {
        let path = Path::new("/tmp/a b\\c\"d$e`f%g");
        assert_eq!(
            quote_executable(path).expect("valid path should be quoted"),
            "\"/tmp/a b\\\\\\\\c\\\\\"d\\\\$e\\\\`f%%g\""
        );
        for invalid in ["/tmp/new\nline", "/tmp/equal=sign"] {
            assert!(quote_executable(Path::new(invalid)).is_err(), "{invalid:?}");
        }
        let non_utf8 = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/\xff".to_vec()));
        assert!(quote_executable(&non_utf8).is_err());
    }

    #[test]
    fn unmanaged_and_symbolic_link_entries_are_never_overwritten() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let launcher = executable(directory.path(), "jiffshot");
        let path = directory.path().join(AUTOSTART_FILE);
        std::fs::write(&path, "custom settings").expect("foreign entry should be written");
        assert!(enable_in(directory.path(), &launcher).is_err());
        assert!(disable_in(directory.path()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "custom settings");

        std::fs::remove_file(&path).expect("foreign fixture should be removed");
        let target = directory.path().join("target");
        std::fs::write(&target, OWNERSHIP_MARKER).expect("link target should be written");
        std::os::unix::fs::symlink(&target, &path).expect("symbolic link should be created");
        assert!(enable_in(directory.path(), &launcher).is_err());
        assert!(disable_in(directory.path()).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), OWNERSHIP_MARKER);
    }

    #[test]
    fn startup_rejects_missing_relative_and_non_executable_launchers() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        assert!(enable_in(directory.path(), Path::new("jiffshot")).is_err());
        assert!(enable_in(directory.path(), &directory.path().join("missing")).is_err());
        let path = directory.path().join("not-executable");
        std::fs::write(&path, "data").expect("fixture should be written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("fixture permissions should be set");
        assert!(enable_in(directory.path(), &path).is_err());
        assert!(!directory.path().join(AUTOSTART_FILE).exists());
    }
}
