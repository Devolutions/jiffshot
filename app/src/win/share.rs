//! Share destinations: other programs, mail, Office apps, folders,
//! PDF and the printer. Each takes the flattened image (written to a
//! temporary PNG named after the capture) unless it needs another format.

use crate::winutil;
use jiffshot_capture::share;
use std::path::Path;

#[cfg(target_os = "macos")]
pub(crate) use jiffshot_capture::share::open_with_in;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use jiffshot_capture::share::temporary_export_path;

/// Destinations offered in the Share panel, before the user's programs.
#[cfg(target_os = "linux")]
pub const BUILT_IN: &[&str] = &["Open with…", "Email", "Folder…", "Print"];
#[cfg(windows)]
pub const BUILT_IN: &[&str] = &["Open with…", "Email", "Word", "PowerPoint", "Excel", "OneNote", "Paint", "Folder…", "Print"];
#[cfg(target_os = "macos")]
pub const BUILT_IN: &[&str] = &["Open with…", "Email", "Word", "PowerPoint", "Excel", "OneNote", "Preview", "Folder…", "Print"];

/// Opens `file` in a program found on the App Paths list (`winword.exe`…).
pub fn open_in(program: &str, file: &Path) -> Result<(), String> {
    share::open_in(winutil::main_hwnd(), program, file)
}

/// Asks which program to open the file with.
pub fn open_with(file: &Path) -> Result<bool, String> {
    share::open_with(winutil::main_hwnd(), file)
}

pub fn print(file: &Path) -> Result<bool, String> {
    share::print(winutil::main_hwnd(), file)
}

pub use share::to_folder;

/// Opens a new message in the default mail program with `file` attached.
pub fn email(file: &Path, subject: &str) -> Result<bool, String> {
    share::email(winutil::main_hwnd(), file, subject)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct ScratchFile(std::path::PathBuf);

    impl ScratchFile {
        fn new(stem: &str, contents: &[u8]) -> Self {
            let path = temporary_export_path(stem, "png").unwrap();
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }
    }

    impl Drop for ScratchFile {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_file(&self.0) {
                eprintln!("could not remove the temporary export test file: {error}");
            }
            if let Some(parent) = self.0.parent() {
                if let Err(error) = std::fs::remove_dir(parent) {
                    eprintln!("could not remove the temporary export test folder: {error}");
                }
            }
        }
    }

    #[test]
    fn temporary_exports_are_private_and_do_not_overwrite_each_other() {
        let first = temporary_export_path("Capture", "png").unwrap();
        let second = temporary_export_path("Capture", "png").unwrap();
        assert_ne!(first, second);
        assert_eq!(first.file_name(), second.file_name());

        for path in [&first, &second] {
            let dir = path.parent().unwrap();
            assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o077, 0);
        }
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");

        for path in [&first, &second] {
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn temporary_exports_reject_path_components() {
        assert!(temporary_export_path("../capture", "png").is_err());
        assert!(temporary_export_path("capture", "../png").is_err());
        assert!(temporary_export_path("..", "png").is_err());
    }
}
