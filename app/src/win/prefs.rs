//! The one Preferences dialog: Capture, Editor, Library, Share and Hotkeys
//! tabs. Each tab is a list of rows, as in the Capture window's panels
//! (`jiffshot_workflow::panel::Row`); the dialog sends back one value per
//! row and tab, and this module applies them: the capture side's tabs
//! through `jiffshot_workflow::prefs`, the editor's through its own actions.

use crate::app::App;
use jiffshot_workflow::panel::{check_row, choice, note, on, string, text, Row};
use jiffshot_workflow::{capture, prefs};
#[cfg(target_os = "macos")]
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

pub const THEMES: [&str; 3] = ["Dark", "Light", "System"];
/// Export formats: label, extension ("" for the one used last).
const FORMATS: [(&str, &str); 7] =
    [("The one used last", ""), ("PNG", "png"), ("JPEG", "jpg"), ("PDF", "pdf"), ("BMP", "bmp"), ("GIF", "gif"), ("WebP", "webp")];

#[derive(Serialize)]
struct Tab {
    id: &'static str,
    title: &'static str,
    rows: Vec<Row>,
}

#[derive(Serialize)]
struct Dialog {
    /// The tab to open on.
    tab: String,
    tabs: Vec<Tab>,
    #[cfg(target_os = "macos")]
    startup_initial: bool,
    #[cfg(target_os = "macos")]
    share_initial: ShareSnapshot,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ShareSnapshot {
    ftp: Option<String>,
    programs: Vec<(String, PathBuf)>,
}

#[cfg(target_os = "macos")]
impl ShareSnapshot {
    fn from_app(app: &App) -> Self {
        Self { ftp: app.settings.ftp.clone(), programs: app.settings.programs.clone() }
    }
}

/// The dialog's JSON, opening on `tab`.
pub fn dialog(app: &App, tab: &str) -> String {
    let capture_settings = jiffshot_workflow::settings_snapshot();
    let s = &app.settings;
    let theme = THEMES.iter().position(|t| Some(*t) == s.theme.as_deref()).unwrap_or(0);
    let format = FORMATS.iter().position(|(_, ext)| *ext == s.export_format.as_deref().unwrap_or("")).unwrap_or(0);
    let mut share = vec![
        text("FTP destination (ftp://user@host/folder)", s.ftp.clone().unwrap_or_default()),
        note(if cfg!(target_os = "macos") {
            "The FTP password is a Keychain generic password with service “Jiffshot FTP” and the FTP username as its account."
        } else {
            "The FTP password is the “Jiffshot FTP” entry in Windows Credential Manager."
        }),
    ];
    if s.programs.is_empty() {
        share.push(note("No programs yet; Share \u{25B8} Add program\u{2026} in the editor adds one."));
    } else {
        share.push(note("Programs captures can be sent to (clear one to remove it):"));
        share.extend(s.programs.iter().map(|(name, _)| check_row(name, true)));
    }
    let startup = capture::startup_status();
    let mut capture_rows = prefs::capture(&capture_settings, startup.requested);
    if let Some(detail) = &startup.detail {
        capture_rows.push(note(detail.clone()));
    }
    let dialog = Dialog {
        tab: tab.to_owned(),
        #[cfg(target_os = "macos")]
        startup_initial: startup.requested,
        #[cfg(target_os = "macos")]
        share_initial: ShareSnapshot::from_app(app),
        tabs: vec![
            Tab { id: "capture", title: "Capture", rows: capture_rows },
            Tab {
                id: "editor",
                title: "Editor",
                rows: vec![
                    choice("Color theme (every Jiffshot window)", &THEMES, theme),
                    check_row("Show tool names in the toolbar", s.layout.labels),
                    check_row("Show the side bar (Quick Styles and properties)", s.layout.side_bar),
                    check_row("Show the Recent Captures tray", s.layout.tray),
                    choice("Export as", &FORMATS.map(|(label, _)| label), format),
                ],
            },
            Tab {
                id: "library",
                title: "Library",
                rows: vec![
                    Row::Folder { label: "Library folder".into(), value: app.library.root.display().to_string() },
                    note("New captures are saved here, and the editor and the tray's Recent captures list them."),
                ],
            },
            Tab { id: "share", title: "Share", rows: share },
            Tab { id: "hotkeys", title: "Hotkeys", rows: prefs::hotkeys(&capture_settings, &crate::capture_window::capture_key()) },
        ],
    };
    serde_json::to_string(&dialog).unwrap_or_default()
}

/// Applies the dialog's values (`{"capture": [...], "editor": [...], …}`).
pub fn apply(app: &mut App, json: &str) {
    let Ok(Value::Object(tabs)) = serde_json::from_str::<Value>(json) else { return };
    let values = |id: &str| -> Vec<Value> { tabs.get(id).and_then(Value::as_array).cloned().unwrap_or_default() };
    let share = values("share");
    #[cfg(target_os = "macos")]
    if !apply_share(app, &share, tabs.get("_share_initial")) {
        return;
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = jiffshot_capture::config::try_validate() {
        app.set_status(format!("Could not apply Preferences: {error}"));
        return;
    }
    #[cfg(target_os = "macos")]
    let startup_initial = tabs.get("_startup_initial").and_then(Value::as_bool).unwrap_or_else(capture::runs_at_startup);
    #[cfg(windows)]
    let startup_initial = capture::runs_at_startup();
    #[cfg(target_os = "macos")]
    if !apply_editor(app, &values("editor"), &values("library")) {
        return;
    }
    apply_capture(&values("capture"), &values("hotkeys"), startup_initial, app);
    #[cfg(windows)]
    apply_editor(app, &values("editor"), &values("library"));
    #[cfg(windows)]
    apply_share(app, &share);
}

fn apply_capture(values: &[Value], hotkeys: &[Value], startup_initial: bool, app: &mut App) {
    #[cfg(target_os = "macos")]
    {
        if !values.is_empty() || !hotkeys.is_empty() {
            crate::capture_window::apply_preferences(values.to_vec(), hotkeys.to_vec(), startup_initial);
        }
        let _ = app;
    }
    #[cfg(windows)]
    {
        let mut s = jiffshot_workflow::settings_snapshot();
        if !values.is_empty() {
            if let Some(on) = prefs::apply_capture(&mut s, values, startup_initial) {
                capture::set_run_at_startup(on);
            }
            capture::apply(s.clone());
        }
        if !hotkeys.is_empty() {
            match prefs::apply_hotkeys(&mut s, hotkeys) {
                Ok(()) => capture::set_hotkeys(s),
                Err(problem) => app.toast("Hotkeys not changed", &problem, true),
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn apply_editor(app: &mut App, editor: &[Value], library: &[Value]) -> bool {
    let mut updated = app.settings.clone();
    let mut changes = serde_json::Map::new();
    if !editor.is_empty() {
        let theme = THEMES[(editor[0].as_u64().unwrap_or(0) as usize).min(2)];
        if updated.theme.as_deref() != Some(theme) {
            updated.theme = Some(theme.to_owned());
            changes.insert("theme".into(), Value::String(theme.to_owned()));
        }
        updated.layout.labels = on(editor, 1);
        updated.layout.side_bar = on(editor, 2);
        updated.layout.tray = on(editor, 3);
        let format = FORMATS[(editor.get(4).and_then(Value::as_u64).unwrap_or(0) as usize).min(FORMATS.len() - 1)].1;
        updated.export_format = (!format.is_empty()).then(|| format.to_owned());
        if updated.layout.labels != app.settings.layout.labels
            || updated.layout.side_bar != app.settings.layout.side_bar
            || updated.layout.tray != app.settings.layout.tray
            || updated.export_format != app.settings.export_format
        {
            match serde_json::to_value(&updated) {
                Ok(value) => { changes.insert("editor".into(), value); }
                Err(error) => {
                    app.set_status(format!("Could not save Editor preferences: {error}"));
                    return false;
                }
            }
        }
    }
    let folder = string(library, 0);
    let new_library = (!folder.is_empty() && std::path::Path::new(&folder) != app.library.root).then(|| PathBuf::from(&folder));
    if let Some(root) = &new_library {
        updated.library = Some(root.clone());
        changes.insert("library".into(), Value::String(folder.clone()));
    }
    if changes.is_empty() {
        return true;
    }
    if let Err(error) = jiffshot_capture::config::try_set_values(changes) {
        app.set_status(format!("Could not save Editor preferences: {error}"));
        return false;
    }
    app.settings = updated;
    if let Some(root) = new_library {
        app.library = jiffshot_capture::library::Library::new(root);
        app.refresh_library();
        app.set_status(format!("Library: {folder}"));
    }
    true
}

#[cfg(windows)]
fn apply_editor(app: &mut App, editor: &[Value], library: &[Value]) {
    if !editor.is_empty() {
        let theme = THEMES[(editor[0].as_u64().unwrap_or(0) as usize).min(2)];
        if app.settings.theme.as_deref() != Some(theme) {
            app.run(&format!("theme:{theme}"));
        }
        let (labels, side_bar, tray) = (on(editor, 1), on(editor, 2), on(editor, 3));
        app.layout(|l| {
            l.labels = labels;
            l.side_bar = side_bar;
            l.tray = tray;
        });
        let format = FORMATS[(editor.get(4).and_then(Value::as_u64).unwrap_or(0) as usize).min(FORMATS.len() - 1)].1;
        if app.settings.export_format.as_deref().unwrap_or("") != format {
            app.run(&format!("export-format:{format}"));
        }
    }
    let folder = string(library, 0);
    if !folder.is_empty() && std::path::Path::new(&folder) != app.library.root {
        app.run(&format!("library-folder:{folder}"));
    }
}

#[cfg(target_os = "macos")]
fn share_changes(initial: &ShareSnapshot, current: &ShareSnapshot, share: &[Value]) -> Result<Option<(Option<String>, Vec<String>)>, &'static str> {
    const INVALID: &str = "the Share preferences are invalid; reopen Preferences";
    let ftp = share.first().and_then(Value::as_str).ok_or(INVALID)?;
    if share.len() != 3 + initial.programs.len() || !share[3..].iter().all(Value::is_boolean) {
        return Err(INVALID);
    }
    let removed: Vec<String> = initial.programs.iter().enumerate()
        .filter(|(index, _)| share[3 + index] == Value::Bool(false))
        .map(|(_, (name, _))| name.clone())
        .collect();
    let ftp_changed = ftp != initial.ftp.as_deref().unwrap_or_default();
    if removed.is_empty() && !ftp_changed {
        return Ok(None);
    }
    if initial != current {
        return Err("Share destinations changed while Preferences was open; reopen Preferences");
    }
    Ok(Some((ftp_changed.then(|| ftp.trim().to_owned()), removed)))
}

#[cfg(target_os = "macos")]
fn apply_share(app: &mut App, share: &[Value], initial: Option<&Value>) -> bool {
    if share.is_empty() {
        return true;
    }
    let initial = initial.and_then(|value| serde_json::from_value::<ShareSnapshot>(value.clone()).ok());
    let changes = match initial {
        Some(initial) => share_changes(&initial, &ShareSnapshot::from_app(app), share),
        None => Err("the Share preferences are missing their original destinations; reopen Preferences"),
    };
    match changes {
        Ok(Some((ftp, removed))) => app.save_share_preferences(ftp.as_deref(), &removed),
        Ok(None) => true,
        Err(error) => {
            app.set_status(format!("Could not save Share preferences: {error}"));
            false
        }
    }
}

#[cfg(windows)]
fn apply_share(app: &mut App, share: &[Value]) -> bool {
    if share.is_empty() {
        return true;
    }
    let ftp = string(share, 0);
    // Rows: FTP, its note, a note, then a check per program.
    let removed: Vec<String> =
        app.settings.programs.iter().enumerate().filter(|(i, _)| share.get(3 + i).is_some_and(|v| v == &Value::Bool(false))).map(|(_, (n, _))| n.clone()).collect();
    if ftp != app.settings.ftp.clone().unwrap_or_default() {
        app.run(&format!("ftp:{ftp}"));
    }
    for name in removed {
        app.run(&format!("program-remove:{name}"));
    }
    true
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot() -> ShareSnapshot {
        ShareSnapshot {
            ftp: Some("ftp://alice@example.test/images".into()),
            programs: vec![("First".into(), "/first.app".into()), ("Second".into(), "/second.app".into())],
        }
    }

    #[test]
    fn share_changes_are_bound_to_the_dialogs_original_destinations() {
        let initial = snapshot();
        let unchanged = vec![json!("ftp://alice@example.test/images"), Value::Null, Value::Null, json!(true), json!(true)];
        let mut current = initial.clone();
        current.programs.remove(0);
        current.programs.push(("New".into(), "/new.app".into()));
        assert_eq!(share_changes(&initial, &current, &unchanged), Ok(None));

        let mut remove_first = unchanged.clone();
        remove_first[3] = json!(false);
        assert_eq!(share_changes(&initial, &initial, &remove_first), Ok(Some((None, vec!["First".into()]))));
        assert!(share_changes(&initial, &current, &remove_first).is_err());
        current = initial.clone();
        current.programs[0].1 = "/replacement.app".into();
        assert!(share_changes(&initial, &current, &remove_first).is_err());
        current = initial.clone();
        current.ftp = Some("ftp://changed.example.test".into());
        assert!(share_changes(&initial, &current, &remove_first).is_err());

        let mut change_ftp = unchanged.clone();
        change_ftp[0] = json!("  ftp://new.example.test/  ");
        assert_eq!(share_changes(&initial, &initial, &change_ftp), Ok(Some((Some("ftp://new.example.test/".into()), Vec::new()))));
        assert!(share_changes(&initial, &current, &change_ftp).is_err());
        change_ftp[0] = json!("  ");
        assert_eq!(share_changes(&initial, &initial, &change_ftp), Ok(Some((Some(String::new()), Vec::new()))));
    }

    #[test]
    fn share_changes_preserve_legacy_ftp_values() {
        let mut initial = snapshot();
        initial.ftp = Some("  ftp://alice@example.test/images  ".into());
        let unchanged = vec![json!("  ftp://alice@example.test/images  "), Value::Null, Value::Null, json!(true), json!(true)];
        let mut current = initial.clone();
        current.programs.push(("New".into(), "/new.app".into()));
        assert_eq!(share_changes(&initial, &current, &unchanged), Ok(None));

        let mut remove_first = unchanged.clone();
        remove_first[3] = json!(false);
        assert_eq!(share_changes(&initial, &initial, &remove_first), Ok(Some((None, vec!["First".into()]))));

        let mut corrected = unchanged.clone();
        corrected[0] = json!("ftp://alice@example.test/images");
        assert_eq!(share_changes(&initial, &initial, &corrected),
            Ok(Some((Some("ftp://alice@example.test/images".into()), Vec::new()))));

        initial.ftp = Some(String::new());
        let mut empty = unchanged;
        empty[0] = json!("");
        assert_eq!(share_changes(&initial, &initial, &empty), Ok(None));
        empty[0] = json!("  ");
        assert_eq!(share_changes(&initial, &initial, &empty), Ok(Some((Some(String::new()), Vec::new()))));
    }

    #[test]
    fn share_changes_reject_missing_or_malformed_rows() {
        let initial = snapshot();
        for values in [
            vec![],
            vec![json!("ftp://alice@example.test/images"), Value::Null, Value::Null, json!(true)],
            vec![json!("ftp://alice@example.test/images"), Value::Null, Value::Null, json!(true), Value::Null],
            vec![Value::Null, Value::Null, Value::Null, json!(true), json!(true)],
        ] {
            assert!(share_changes(&initial, &initial, &values).is_err());
        }
    }
}
