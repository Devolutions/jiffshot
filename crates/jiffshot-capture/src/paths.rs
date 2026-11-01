//! Filesystem paths and where Jiffshot keeps its settings, logs and caches.
//!
//! `JIFFSHOT_CONFIG_DIR` and `JIFFSHOT_LOCAL_DIR` override both folders on
//! every platform, so tests never touch a user's real settings.

use std::path::PathBuf;

/// The product's name, for folders and registry values.
pub const NAME: &str = "Jiffshot";

/// Decodes the path portion of a file URI without losing non-UTF-8 bytes.
pub fn decode_uri_path(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = if bytes[i] == b'%' {
            let digits = bytes
                .get(i + 1..i + 3)
                .ok_or("incomplete percent escape in the file URI")?;
            let hex = |byte: u8| char::from(byte).to_digit(16);
            let value = hex(digits[0])
                .zip(hex(digits[1]))
                .ok_or("invalid percent escape in the file URI")?;
            i += 3;
            (value.0 * 16 + value.1) as u8
        } else {
            let byte = bytes[i];
            i += 1;
            byte
        };
        if byte == 0 {
            return Err("the file URI contains a NUL byte".into());
        }
        out.push(byte);
    }
    Ok(out)
}

fn overridden(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Settings, presets and stamps: `%APPDATA%\Jiffshot` on Windows,
/// `~/Library/Application Support/Jiffshot` on macOS.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = overridden("JIFFSHOT_CONFIG_DIR") {
        return Some(dir);
    }
    #[cfg(windows)]
    {
        Some(PathBuf::from(std::env::var_os("APPDATA")?).join(NAME))
    }
    #[cfg(target_os = "macos")]
    {
        Some(
            crate::os::home_dir()?
                .join("Library/Application Support")
                .join(NAME),
        )
    }
}

/// Logs and caches, which start fresh: `%LOCALAPPDATA%\Jiffshot` on
/// Windows, `~/Library/Caches/Jiffshot` on macOS (the temp folder without
/// either).
pub fn local_dir() -> PathBuf {
    if let Some(dir) = overridden("JIFFSHOT_LOCAL_DIR") {
        return dir;
    }

    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = crate::os::home_dir().map(|home| home.join("Library/Caches"));
    base.unwrap_or_else(std::env::temp_dir).join(NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_paths_preserve_bytes_and_reject_invalid_escapes() {
        assert_eq!(
            decode_uri_path("/tmp/a%20b%FF%23.png").unwrap(),
            b"/tmp/a b\xff#.png"
        );
        for invalid in ["/tmp/%", "/tmp/%2", "/tmp/%xy", "/tmp/%00"] {
            assert!(decode_uri_path(invalid).is_err(), "{invalid}");
        }
    }
}
