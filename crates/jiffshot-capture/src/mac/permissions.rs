//! On-demand permission failures. Only the desktop app installs a UI handler;
//! command-line tools retain actionable errors without opening windows.

use std::io;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Permission {
    ScreenRecording,
    Accessibility,
    Camera,
    Microphone,
    Automation,
}

type DeniedHandler = Arc<dyn Fn(Permission) + Send + Sync>;
static DENIED_HANDLER: Mutex<Option<DeniedHandler>> = Mutex::new(None);

/// Installs guidance without querying or requesting any permissions.
pub fn set_denied_handler(handler: Option<DeniedHandler>) {
    *DENIED_HANDLER
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = handler;
}

impl Permission {
    pub fn name(self) -> &'static str {
        match self {
            Self::ScreenRecording => "Screen Recording",
            Self::Accessibility => "Accessibility",
            Self::Camera => "Camera",
            Self::Microphone => "Microphone",
            Self::Automation => "Automation",
        }
    }

    pub fn purpose(self) -> &'static str {
        match self {
            Self::ScreenRecording => "Jiffshot needs access to your screen to take screenshots, record video or system audio, and read screen text and window titles.",
            Self::Accessibility => "Jiffshot needs Accessibility access for automatic scrolling, Step Capture details, and controlling other applications.",
            Self::Camera => "Jiffshot needs Camera access to add your webcam to a recording.",
            Self::Microphone => "Jiffshot needs Microphone access to record your microphone or narration.",
            Self::Automation => "Jiffshot needs permission to control Preview to open its print dialog.",
        }
    }

    pub fn instructions(self) -> &'static str {
        match self {
            Self::ScreenRecording => "Open System Settings > Privacy & Security > Screen & System Audio Recording (Screen Recording on older macOS versions). Enable Jiffshot, then retry your capture. If macOS asks you to quit and reopen Jiffshot, save your work and do so first.",
            Self::Accessibility => "Open System Settings > Privacy & Security > Accessibility and enable Jiffshot. If it is not listed, use + to add Jiffshot from Applications. Then retry the action.",
            Self::Camera => "Open System Settings > Privacy & Security > Camera and enable Jiffshot. Then turn the webcam on again.",
            Self::Microphone => "Open System Settings > Privacy & Security > Microphone and enable Jiffshot. Then turn the microphone on again or retry narration.",
            Self::Automation => "Open System Settings > Privacy & Security > Automation, expand Jiffshot, and enable Preview. Then retry printing.",
        }
    }

    pub fn settings_url(self) -> &'static str {
        match self {
            Self::ScreenRecording => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Self::Camera => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera"
            }
            Self::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
            Self::Automation => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation"
            }
        }
    }

    pub fn denied(self) -> io::Error {
        self.report_denied();
        self.error()
    }

    pub(crate) fn error(self) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {}", self.purpose(), self.instructions()),
        )
    }

    pub(crate) fn report_denied(self) {
        let handler = DENIED_HANDLER
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(handler) = handler {
            handler(self);
        }
    }

    /// Opens only the selected privacy pane, and only on an explicit user action.
    pub fn open_settings(self) -> io::Result<()> {
        let output = std::process::Command::new("/usr/bin/open")
            .arg(self.settings_url())
            .stdin(std::process::Stdio::null())
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "Could not open System Settings ({}): {}. {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
                self.instructions(),
            )))
        }
    }
}

/// Passive checks must not request access or warn about optional capabilities.
pub(crate) fn check_or_request(
    ask: bool,
    granted: impl FnOnce() -> bool,
    request: impl FnOnce() -> bool,
    denied: impl FnOnce(),
) -> bool {
    if granted() {
        return true;
    }
    if !ask {
        return false;
    }
    if request() {
        return true;
    }
    denied();
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn passive_and_granted_checks_never_prompt_or_warn() {
        assert!(!check_or_request(
            false,
            || false,
            || panic!("passive request"),
            || panic!("passive warning")
        ));
        assert!(check_or_request(
            true,
            || true,
            || panic!("already granted"),
            || panic!("already granted")
        ));
    }

    #[test]
    fn required_access_warns_only_after_a_denied_request_and_rechecks_on_retry() {
        let allowed = Cell::new(false);
        let requests = Cell::new(0);
        let warnings = Cell::new(0);
        let check = || {
            check_or_request(
                true,
                || allowed.get(),
                || {
                    requests.set(requests.get() + 1);
                    allowed.get()
                },
                || warnings.set(warnings.get() + 1),
            )
        };
        assert!(!check());
        assert_eq!((requests.get(), warnings.get()), (1, 1));
        allowed.set(true);
        assert!(check());
        assert_eq!((requests.get(), warnings.get()), (1, 1));
        assert!(check_or_request(
            true,
            || false,
            || true,
            || panic!("newly granted")
        ));
    }

    #[test]
    fn every_permission_has_specific_actionable_guidance_and_a_privacy_link() {
        for (permission, pane) in [
            (Permission::ScreenRecording, "ScreenCapture"),
            (Permission::Accessibility, "Accessibility"),
            (Permission::Camera, "Camera"),
            (Permission::Microphone, "Microphone"),
            (Permission::Automation, "Automation"),
        ] {
            assert!(permission
                .settings_url()
                .ends_with(&format!("?Privacy_{pane}")));
            assert!(permission
                .instructions()
                .contains("System Settings > Privacy & Security"));
            assert!(permission.instructions().contains("Jiffshot"));
        }
        assert!(Permission::ScreenRecording
            .instructions()
            .contains("quit and reopen"));
        assert!(Permission::Automation.instructions().contains("Preview"));
    }
}
