//! Local share destinations on Linux: other programs, the default mail
//! client (`xdg-email`), the printer (`lp`) and folders. `owner` (a window
//! on Windows) is unused here.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Creates a private per-operation folder for a temporary share export.
pub fn temporary_export_path(stem: &str, ext: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;

    if stem.is_empty()
        || matches!(stem, "." | "..")
        || stem.contains(['/', '\0'])
        || ext.is_empty()
        || ext.contains(['/', '\0'])
    {
        return Err("invalid temporary sharing filename".into());
    }
    static NEXT_EXPORT: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("could not prepare a temporary share: {error}"))?
        .as_nanos();
    for _ in 0..32 {
        let sequence = NEXT_EXPORT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "jiffshot-share-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir.join(format!("{stem}.{ext}"))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not prepare a temporary sharing folder: {error}"
                ))
            }
        }
    }
    Err("could not create a unique temporary sharing folder".into())
}

fn spawn(program: &str, args: &[&std::ffi::OsStr]) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Opens `file` in a program: an executable name or path.
pub fn open_in(_owner: isize, program: &str, file: &Path) -> Result<(), String> {
    spawn(program, &[file.as_os_str()])
        .map_err(|error| format!("{program} is not installed or could not start: {error}"))
}

/// Opens the file in its default application.
pub fn open_with(_owner: isize, file: &Path) -> Result<bool, String> {
    open_with_in(file, None)
}

pub fn open_with_in(file: &Path, _folder: Option<&Path>) -> Result<bool, String> {
    spawn("xdg-open", &[file.as_os_str()])
        .map_err(|error| format!("could not open the capture: {error}"))?;
    Ok(true)
}

/// Sends the file to the default printer.
pub fn print(_owner: isize, file: &Path) -> Result<bool, String> {
    let status = Command::new("lp")
        .arg(file)
        .stdin(Stdio::null())
        .status()
        .map_err(|error| format!("could not print the capture (is CUPS installed?): {error}"))?;
    if status.success() {
        Ok(true)
    } else {
        Err(format!("could not print the capture: lp {status}"))
    }
}

/// Copies the file into `folder`, never overwriting.
pub fn to_folder(file: &Path, folder: &Path) -> Result<PathBuf, String> {
    let metadata = std::fs::metadata(file)
        .map_err(|error| format!("could not inspect {}: {error}", file.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "the share source is not a regular file: {}",
            file.display()
        ));
    }
    let name = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Capture".into());
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "png".into());
    let mut source = std::fs::File::open(file)
        .map_err(|error| format!("could not open {}: {error}", file.display()))?;
    for n in 1u64.. {
        let target = if n == 1 {
            folder.join(format!("{name}.{ext}"))
        } else {
            folder.join(format!("{name} ({n}).{ext}"))
        };
        let mut output = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create {}: {error}", target.display())),
        };
        io::copy(&mut source, &mut output)
            .map_err(|error| format!("could not copy to {}: {error}", target.display()))?;
        return Ok(target);
    }
    unreachable!()
}

/// Opens a new message in the default mail client with `file` attached.
pub fn email(_owner: isize, file: &Path, subject: &str) -> Result<bool, String> {
    spawn(
        "xdg-email",
        &[
            "--subject".as_ref(),
            subject.as_ref(),
            "--attach".as_ref(),
            file.as_os_str(),
        ],
    )
    .map_err(|error| format!("could not open a new message (is xdg-email installed?): {error}"))?;
    Ok(true)
}
