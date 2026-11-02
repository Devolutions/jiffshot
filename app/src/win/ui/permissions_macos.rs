//! Persistent, on-demand guidance, including when only the tray is visible.

use super::capture_view::parts::{self, Subscriptions};
use jiffshot_capture::macos::permissions::{self, Permission};
use rustolonia::{
    AnyEl, AppScope, Button, HorizontalAlignment, NodeRef, Orientation, Result, StackPanel,
    TextBlock, TextWrapping, Window, WindowStartupLocation,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

struct Guidance {
    window: Window,
    subscriptions: Subscriptions,
}

thread_local! {
    static WINDOWS: RefCell<HashMap<Permission, Guidance>> = RefCell::new(HashMap::new());
}

pub(super) fn install(scope: &AppScope) {
    let scope = scope.clone();
    permissions::set_denied_handler(Some(Arc::new(move |permission| {
        let ui = scope.clone();
        if let Err(error) = scope.post(move || {
            if let Err(error) = show(&ui, permission) {
                report_failure(
                    permission,
                    &format!("Could not show permission guidance: {error}"),
                );
            }
        }) {
            report_failure(
                permission,
                &format!("Could not queue permission guidance: {error}"),
            );
        }
    })));
}

fn report_failure(permission: Permission, error: &str) {
    let message = format!("{error}. {}", permission.instructions());
    jiffshot_workflow::log(&message);
    crate::app::post(move |app| app.toast("Permission required", &message, true));
}

fn show(scope: &AppScope, permission: Permission) -> Result<()> {
    let existing = WINDOWS.with(|windows| {
        windows
            .borrow()
            .get(&permission)
            .map(|guidance| guidance.window.clone())
    });
    let window = match existing {
        Some(window) => window,
        None => {
            let guidance = build(scope, permission)?;
            let window = guidance.window.clone();
            WINDOWS.with(|windows| windows.borrow_mut().insert(permission, guidance));
            window
        }
    };
    // Repeated failures (for example an interval capture) must not open more
    // windows or steal focus from System Settings while the user enables access.
    if !window.get_is_visible()? {
        window.show()?;
        if let Some(handle) = parts::hwnd(scope, &window) {
            crate::winutil::bring_forward(handle as isize);
        }
    }
    Ok(())
}

fn build(scope: &AppScope, permission: Permission) -> Result<Guidance> {
    let subscriptions = Subscriptions::default();
    let window_ref = NodeRef::<Window>::new();
    let status = TextBlock::new()?
        .text_wrapping(TextWrapping::Wrap)?
        .max_width(460.0)?;
    let error_status = status.clone();
    let settings = parts::on_click(
        Button::el()
            .content("Open System Settings")
            .default(true)
            .min_width(160.0),
        &subscriptions,
        move |_| {
            let text = match permission.open_settings() {
                Ok(()) => {
                    "After enabling access, return to Jiffshot and retry the action.".to_owned()
                }
                Err(error) => {
                    let message = error.to_string();
                    jiffshot_workflow::log(&message);
                    message
                }
            };
            if let Err(error) = error_status.set_text(text) {
                report_failure(
                    permission,
                    &format!("Could not update permission guidance: {error}"),
                );
            }
        },
    );
    let close = parts::on_click(
        Button::el().content("Not now").cancel(true).min_width(90.0),
        &subscriptions,
        move |_| {
            if let Some(window) = window_ref.get() {
                if let Err(error) = window.hide() {
                    report_failure(
                        permission,
                        &format!("Could not dismiss permission guidance: {error}"),
                    );
                }
            }
        },
    );
    let closing_scope = scope.clone();
    let window = Window::el()
        .title(format!("Jiffshot needs {} access", permission.name()))
        .icon(super::icon_file())
        .size_to_content(parts::SIZE_TO_CONTENT)
        .can_resize(false)
        .window_startup_location(WindowStartupLocation::CenterScreen)
        .node_ref(window_ref)
        .on_closing(move |event| {
            if !parts::closing_for_good(event.close_reason) {
                event.cancel = true;
                parts::post(&closing_scope, "Permission guidance", move || {
                    match window_ref.get() {
                        Some(window) => window.hide(),
                        None => Ok(()),
                    }
                });
            }
        })
        .content(
            StackPanel::el().margin(24.0).spacing(16.0).max_width(508.0)
                .child(TextBlock::el().text(permission.purpose()).text_wrapping(TextWrapping::Wrap).max_width(460.0))
                .child(TextBlock::el().text(permission.instructions()).text_wrapping(TextWrapping::Wrap).max_width(460.0))
                .child(TextBlock::el().text("If access is restricted by your organization or parental controls, ask your administrator. Permissions belong to Jiffshot itself, not Terminal.").text_wrapping(TextWrapping::Wrap).max_width(460.0))
                .child(AnyEl::from_control(status))
                .child(StackPanel::el().orientation(Orientation::Horizontal).spacing(8.0)
                    .horizontal_alignment(HorizontalAlignment::Right).child(close).child(settings)),
        )
        .build(scope)?;
    Ok(Guidance {
        window,
        subscriptions,
    })
}

pub(super) fn after_run() {
    permissions::set_denied_handler(None);
    WINDOWS.with(|windows| {
        for (_, guidance) in windows.borrow_mut().drain() {
            guidance.subscriptions.clear();
        }
    });
}

pub(super) fn windows_for_selftest() -> Vec<(Permission, Window)> {
    assert!(std::env::var_os("JIFFSHOT_UITEST_PERMISSIONS").is_some());
    WINDOWS.with(|windows| {
        windows
            .borrow()
            .iter()
            .map(|(permission, guidance)| (*permission, guidance.window.clone()))
            .collect()
    })
}
