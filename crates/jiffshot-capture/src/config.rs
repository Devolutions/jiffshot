//! Jiffshot's one settings file, `%APPDATA%\Jiffshot\jiffshot.json`:
//!
//! ```json
//! { "library": "D:\\Captures", "theme": "Dark",
//!   "capture": { … }, "editor": { … } }
//! ```
//!
//! The capture side and the editor each own their section; the library
//! folder and the color theme are shared at the top. Reads and writes go
//! through one lock, so the two sides (on different threads) never undo
//! each other's changes.

use serde_json::{Map, Value};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const FILE: &str = "jiffshot.json";

/// Serializes every read-modify-write of the file in this process.
static LOCK: Mutex<()> = Mutex::new(());

fn read_object(path: &Path) -> Map<String, Value> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| match v {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// The settings in `dir` (empty when there are none yet).
fn read(dir: &Path) -> Map<String, Value> {
    read_object(&dir.join(FILE))
}

fn write_checked(dir: &Path, root: &Map<String, Value>) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(root).map_err(io::Error::other)?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{FILE}.{}-{sequence}.tmp", std::process::id()));
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let staging = StagingFile(tmp);
        let written = file.write_all(&json).and_then(|()| file.sync_all());
        drop(file);
        written?;
        return std::fs::rename(&staging.0, dir.join(FILE));
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not reserve a settings staging file"))
}

struct StagingFile(PathBuf);

impl Drop for StagingFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("Could not remove settings staging file {}: {error}", self.0.display());
            }
        }
    }
}

fn read_checked(path: &Path) -> io::Result<Map<String, Value>> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        {
            Value::Object(root) => Ok(root),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the settings file is not a JSON object",
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error),
    }
}

fn try_update(dir: &Path, update: impl FnOnce(&mut Map<String, Value>)) -> io::Result<()> {
    let mut root = read_checked(&dir.join(FILE))?;
    update(&mut root);
    write_checked(dir, &root)
}

fn try_replace_section(dir: &Path, name: &str, value: Value) -> io::Result<()> {
    try_update(dir, |root| {
        root.insert(name.to_owned(), value);
    })
}

fn with_dir<R>(f: impl FnOnce(&Path) -> R) -> Option<R> {
    let dir = crate::paths::config_dir()?;
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    Some(f(&dir))
}

/// One side's section (`"capture"` or `"editor"`), `Null` when there is none.
pub fn section(name: &str) -> Value {
    with_dir(|dir| read(dir).remove(name))
        .flatten()
        .unwrap_or(Value::Null)
}

/// Replaces one side's section, keeping everything else.
pub fn set_section(name: &str, value: Value) {
    if let Err(error) = try_set_section(name, value) {
        eprintln!("Could not save {name} settings: {error}");
    }
}

/// Replaces a section without discarding unreadable settings or hiding write failures.
pub fn try_set_section(name: &str, value: Value) -> io::Result<()> {
    let dir = crate::paths::config_dir()
        .ok_or_else(|| io::Error::other("the settings directory is unavailable"))?;
    let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    try_replace_section(&dir, name, value)
}

/// Checks that existing settings can be read without changing them.
#[cfg(unix)]
pub fn try_validate() -> io::Result<()> {
    let dir = crate::paths::config_dir()
        .ok_or_else(|| io::Error::other("the settings directory is unavailable"))?;
    let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    read_checked(&dir.join(FILE)).map(|_| ())
}

/// Replaces several top-level values together without discarding unreadable settings.
#[cfg(unix)]
pub fn try_set_values(values: Map<String, Value>) -> io::Result<()> {
    let dir = crate::paths::config_dir()
        .ok_or_else(|| io::Error::other("the settings directory is unavailable"))?;
    let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    try_update(&dir, |root| root.extend(values))
}

fn top(key: &str) -> Option<String> {
    with_dir(|dir| {
        read(dir)
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
    .flatten()
}

fn set_top(key: &str, value: Option<&str>) {
    let result = with_dir(|dir| {
        try_update(dir, |root| {
            match value {
                Some(v) => root.insert(key.to_owned(), Value::String(v.to_owned())),
                None => root.remove(key),
            };
        })
    })
    .unwrap_or_else(|| Err(io::Error::other("the settings directory is unavailable")));
    if let Err(error) = result {
        eprintln!("Could not save {key} setting: {error}");
    }
}

/// The library folder chosen in Preferences, if any. `JIFFSHOT_LIBRARY`
/// overrides it (for tests and portable use).
pub fn library() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("JIFFSHOT_LIBRARY").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    top("library").map(PathBuf::from)
}

/// Where captures are saved and listed: [`library`], or `Pictures\Jiffshot`.
pub fn library_root() -> PathBuf {
    library().unwrap_or_else(crate::library::Library::default_root)
}

pub fn set_library(folder: Option<&Path>) {
    set_top("library", folder.map(|f| f.to_string_lossy()).as_deref());
}

/// "Dark", "Light" or "System"; for every window.
pub fn theme() -> Option<String> {
    top("theme")
}

pub fn set_theme(theme: &str) {
    set_top("theme", Some(theme));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("jiffshot-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_section_write_keeps_the_other_section() {
        let dir = temp("sections");
        let mut root = read(&dir);
        root.insert("editor".into(), serde_json::json!({ "recent": ["a.png"] }));
        write_checked(&dir, &root).unwrap();
        let mut root = read(&dir);
        root.insert("capture".into(), serde_json::json!({ "countdown": 1 }));
        write_checked(&dir, &root).unwrap();
        let root = read(&dir);
        assert_eq!(root["editor"]["recent"][0], "a.png");
        assert_eq!(root["capture"]["countdown"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_write_preserves_an_existing_temporary_file() {
        let dir = temp("temporary-file");
        let other = dir.join(format!("{FILE}.tmp"));
        std::fs::write(&other, b"another writer's data").unwrap();
        try_replace_section(&dir, "capture", serde_json::json!({"countdown": 1})).unwrap();
        assert_eq!(std::fs::read(&other).unwrap(), b"another writer's data");
        assert_eq!(read(&dir)["capture"]["countdown"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checked_section_writes_preserve_other_settings_and_report_failures() {
        let dir = temp("checked-sections");
        let mut root = Map::new();
        root.insert("editor".into(), serde_json::json!({ "recent": ["a.png"] }));
        write_checked(&dir, &root).unwrap();
        try_replace_section(&dir, "capture", serde_json::json!({ "countdown": 1 })).unwrap();
        let root = read(&dir);
        assert_eq!(root["editor"]["recent"][0], "a.png");
        assert_eq!(root["capture"]["countdown"], 1);
        try_update(&dir, |root| {
            root.insert("theme".into(), serde_json::json!("Light"));
            root.insert("editor".into(), serde_json::json!({ "recent": ["b.png"] }));
        })
        .unwrap();
        let root = read(&dir);
        assert_eq!(root["capture"]["countdown"], 1);
        assert_eq!(root["editor"]["recent"][0], "b.png");
        assert_eq!(root["theme"], "Light");

        let settings = dir.join(FILE);
        std::fs::write(&settings, b"{invalid").unwrap();
        assert_eq!(
            try_replace_section(&dir, "capture", Value::Null)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            try_update(&dir, |root| {
                root.insert("theme".into(), serde_json::json!("Dark"));
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&settings).unwrap(), b"{invalid");
        std::fs::write(&settings, b"[]").unwrap();
        assert_eq!(
            try_replace_section(&dir, "capture", Value::Null)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&settings).unwrap(), b"[]");

        let blocked = dir.join("not-a-directory");
        std::fs::write(&blocked, b"keep").unwrap();
        assert!(try_replace_section(&blocked, "capture", Value::Null).is_err());
        assert_eq!(std::fs::read(&blocked).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
