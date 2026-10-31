//! Global capture and recording shortcuts managed by the desktop portal.

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::desktop::Session;
use async_channel::Receiver;
use futures_util::StreamExt;

const CAPTURE_SHORTCUT: &str = "capture";
const TOGGLE_RECORDING_SHORTCUT: &str = "toggle-recording";
const PAUSE_RECORDING_SHORTCUT: &str = "pause-recording";
const STOP_RECORDING_SHORTCUT: &str = "stop-recording";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum RecordingStartShortcut {
    #[default]
    Disabled,
    Silent,
    Microphone,
    System,
    SystemAndMicrophone,
}

impl RecordingStartShortcut {
    pub(super) const ALL: [Self; 5] = [
        Self::Disabled,
        Self::Silent,
        Self::Microphone,
        Self::System,
        Self::SystemAndMicrophone,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Disabled => "No start shortcut",
            Self::Silent => "Silent",
            Self::Microphone => "Microphone",
            Self::System => "System audio",
            Self::SystemAndMicrophone => "System + microphone",
        }
    }

    pub(super) fn stored_name(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Silent => "silent",
            Self::Microphone => "microphone",
            Self::System => "system",
            Self::SystemAndMicrophone => "both",
        }
    }

    pub(super) fn from_stored_name(name: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|choice| choice.stored_name() == name)
            .ok_or_else(|| format!("unknown global recording start preference: {name}"))
    }

    pub(super) fn index(self) -> i32 {
        match self {
            Self::Disabled => 0,
            Self::Silent => 1,
            Self::Microphone => 2,
            Self::System => 3,
            Self::SystemAndMicrophone => 4,
        }
    }

    pub(super) fn from_index(index: i32) -> Result<Self, String> {
        usize::try_from(index)
            .ok()
            .and_then(|index| Self::ALL.get(index))
            .copied()
            .ok_or_else(|| "select a recording shortcut audio option".to_owned())
    }

    pub(super) fn audio(self) -> Option<crate::capture_linux::RecordingAudio> {
        use crate::capture_linux::RecordingAudio;

        match self {
            Self::Disabled => None,
            Self::Silent => Some(RecordingAudio::Disabled),
            Self::Microphone => Some(RecordingAudio::Microphone),
            Self::System => Some(RecordingAudio::System),
            Self::SystemAndMicrophone => Some(RecordingAudio::SystemAndMicrophone),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ShortcutOptions {
    pub(super) recording_controls: bool,
    pub(super) recording_start: RecordingStartShortcut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutAction {
    Capture,
    ToggleRecording,
    PauseRecording,
    StopRecording,
}

impl ShortcutAction {
    fn id(self) -> &'static str {
        match self {
            Self::Capture => CAPTURE_SHORTCUT,
            Self::ToggleRecording => TOGGLE_RECORDING_SHORTCUT,
            Self::PauseRecording => PAUSE_RECORDING_SHORTCUT,
            Self::StopRecording => STOP_RECORDING_SHORTCUT,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Capture => "Capture screen or window",
            Self::ToggleRecording => "Start or stop screen recording",
            Self::PauseRecording => "Pause or resume screen recording",
            Self::StopRecording => "Stop and save screen recording",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ShortcutEnd {
    Stopped,
    ClosedByDesktop,
}

enum Next<T> {
    Value(T),
    Stopped,
    Closed(Option<zbus::Message>),
}

pub(super) async fn listen(
    preferred_trigger: String,
    options: ShortcutOptions,
    stop: Receiver<()>,
    on_bound: impl FnOnce(Vec<(ShortcutAction, String)>) -> Result<(), String>,
    mut on_activated: impl FnMut(ShortcutAction) -> Result<(), String>,
) -> Result<ShortcutEnd, String> {
    let portal = GlobalShortcuts::new()
        .await
        .map_err(|error| format!("could not connect to the global shortcuts portal: {error}"))?;
    let session = portal
        .create_session()
        .await
        .map_err(|error| format!("could not create a global shortcut session: {error}"))?;
    let result = listen_in_session(
        &portal,
        &session,
        &preferred_trigger,
        options,
        &stop,
        on_bound,
        &mut on_activated,
    )
    .await;
    let closed = if matches!(result, Ok(ShortcutEnd::ClosedByDesktop)) {
        Ok(())
    } else {
        session
            .close()
            .await
            .map_err(|error| format!("could not close global shortcut session: {error}"))
    };
    crate::portal_linux::combine_session_results(result, closed)
}

async fn listen_in_session(
    portal: &GlobalShortcuts<'_>,
    session: &Session<'_, GlobalShortcuts<'_>>,
    preferred_trigger: &str,
    options: ShortcutOptions,
    stop: &Receiver<()>,
    on_bound: impl FnOnce(Vec<(ShortcutAction, String)>) -> Result<(), String>,
    on_activated: &mut impl FnMut(ShortcutAction) -> Result<(), String>,
) -> Result<ShortcutEnd, String> {
    // ashpd serializes a Session as its object path but does not expose the path directly.
    let session_path: String = serde_json::from_value(
        serde_json::to_value(session)
            .map_err(|error| format!("could not identify the global shortcut session: {error}"))?,
    )
    .map_err(|error| format!("could not identify the global shortcut session: {error}"))?;
    // ashpd 0.11 decodes Closed as an empty signal, but the portal sends details.
    let session_proxy = zbus::Proxy::new(
        portal.connection(),
        "org.freedesktop.portal.Desktop",
        session_path.as_str(),
        "org.freedesktop.portal.Session",
    )
    .await
    .map_err(|error| format!("could not monitor the global shortcut session: {error}"))?;
    let mut closed = session_proxy
        .receive_signal("Closed")
        .await
        .map_err(|error| format!("could not monitor the global shortcut session: {error}"))?;
    let mut activated = portal
        .receive_activated()
        .await
        .map_err(|error| format!("could not listen for global shortcuts: {error}"))?;
    let actions = shortcut_actions(preferred_trigger, options);
    let shortcuts: Vec<_> = actions
        .iter()
        .map(|(action, trigger)| {
            NewShortcut::new(action.id(), action.description()).preferred_trigger(*trigger)
        })
        .collect();
    let request = match futures_lite::future::or(
        async { Next::Value(portal.bind_shortcuts(session, &shortcuts, None).await) },
        futures_lite::future::or(
            async {
                let _ = stop.recv().await;
                Next::Stopped
            },
            async { Next::Closed(closed.next().await) },
        ),
    )
    .await
    {
        Next::Value(request) => {
            request.map_err(|error| format!("could not register global shortcuts: {error}"))?
        }
        Next::Stopped => return Ok(ShortcutEnd::Stopped),
        Next::Closed(Some(_)) => return Ok(ShortcutEnd::ClosedByDesktop),
        Next::Closed(None) => {
            return Err("the global shortcut session monitor ended unexpectedly".to_owned())
        }
    };
    let binding = request
        .response()
        .map_err(|error| format!("global shortcuts were not enabled: {error}"))?;
    let bindings: Vec<_> = actions
        .iter()
        .map(|(action, _)| {
            binding
                .shortcuts()
                .iter()
                .find(|shortcut| shortcut.id() == action.id())
                .map(|shortcut| (*action, shortcut.trigger_description().to_owned()))
                .ok_or_else(|| {
                    format!(
                        "the desktop portal did not bind the {} shortcut",
                        action.description()
                    )
                })
        })
        .collect::<Result<_, _>>()?;
    on_bound(bindings)?;

    loop {
        match futures_lite::future::or(
            async { Next::Value(activated.next().await) },
            futures_lite::future::or(
                async {
                    let _ = stop.recv().await;
                    Next::Stopped
                },
                async { Next::Closed(closed.next().await) },
            ),
        )
        .await
        {
            Next::Value(Some(event)) => {
                if let Some(action) = shortcut_activation(
                    &session_path,
                    event.session_handle().as_str(),
                    event.shortcut_id(),
                    &actions,
                ) {
                    on_activated(action)?;
                }
            }
            Next::Value(None) => {
                return Err("the global shortcut event stream ended unexpectedly".to_owned())
            }
            Next::Stopped => return Ok(ShortcutEnd::Stopped),
            Next::Closed(Some(_)) => return Ok(ShortcutEnd::ClosedByDesktop),
            Next::Closed(None) => {
                return Err("the global shortcut session monitor ended unexpectedly".to_owned())
            }
        }
    }
}

fn shortcut_actions(
    preferred_trigger: &str,
    options: ShortcutOptions,
) -> Vec<(ShortcutAction, &str)> {
    let mut actions = vec![(ShortcutAction::Capture, preferred_trigger)];
    if options.recording_controls {
        if options.recording_start.audio().is_some() {
            actions.push((ShortcutAction::ToggleRecording, "SHIFT+F8"));
        }
        actions.extend([
            (ShortcutAction::PauseRecording, "SHIFT+F9"),
            (ShortcutAction::StopRecording, "SHIFT+F10"),
        ]);
    }
    actions
}

fn shortcut_activation(
    session: &str,
    event_session: &str,
    shortcut: &str,
    actions: &[(ShortcutAction, &str)],
) -> Option<ShortcutAction> {
    if session != event_session {
        return None;
    }
    actions
        .iter()
        .map(|(action, _)| *action)
        .find(|action| action.id() == shortcut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activates_only_requested_shortcuts_for_the_current_session() {
        let session = "/org/freedesktop/portal/desktop/session/test/one";
        let actions = [
            (ShortcutAction::Capture, "CTRL+SHIFT+X"),
            (ShortcutAction::ToggleRecording, "SHIFT+F8"),
            (ShortcutAction::PauseRecording, "SHIFT+F9"),
            (ShortcutAction::StopRecording, "SHIFT+F10"),
        ];
        for action in [
            ShortcutAction::Capture,
            ShortcutAction::ToggleRecording,
            ShortcutAction::PauseRecording,
            ShortcutAction::StopRecording,
        ] {
            assert_eq!(
                shortcut_activation(session, session, action.id(), &actions),
                Some(action)
            );
            assert_eq!(
                shortcut_activation(
                    session,
                    "/org/freedesktop/portal/desktop/session/test/two",
                    action.id(),
                    &actions
                ),
                None
            );
        }
        assert_eq!(
            shortcut_activation(session, session, "record", &actions),
            None
        );
        assert_eq!(
            shortcut_activation(session, session, PAUSE_RECORDING_SHORTCUT, &actions[..1]),
            None,
            "recording controls must remain inactive without explicit opt-in"
        );
    }

    #[test]
    fn recording_start_binding_requires_both_recording_opt_ins() {
        for options in [
            ShortcutOptions::default(),
            ShortcutOptions {
                recording_start: RecordingStartShortcut::Microphone,
                ..ShortcutOptions::default()
            },
            ShortcutOptions {
                recording_controls: true,
                ..ShortcutOptions::default()
            },
        ] {
            assert!(
                !shortcut_actions("CTRL+SHIFT+X", options)
                    .iter()
                    .any(|(action, _)| *action == ShortcutAction::ToggleRecording),
                "start shortcut must not bind without both opt-ins: {options:?}"
            );
        }
        let actions = shortcut_actions(
            "CTRL+SHIFT+X",
            ShortcutOptions {
                recording_controls: true,
                recording_start: RecordingStartShortcut::Microphone,
            },
        );
        assert_eq!(
            actions,
            [
                (ShortcutAction::Capture, "CTRL+SHIFT+X"),
                (ShortcutAction::ToggleRecording, "SHIFT+F8"),
                (ShortcutAction::PauseRecording, "SHIFT+F9"),
                (ShortcutAction::StopRecording, "SHIFT+F10"),
            ]
        );
    }

    #[test]
    fn recording_start_choices_map_to_expected_audio_modes() {
        use crate::capture_linux::RecordingAudio;

        let expected = [
            None,
            Some(RecordingAudio::Disabled),
            Some(RecordingAudio::Microphone),
            Some(RecordingAudio::System),
            Some(RecordingAudio::SystemAndMicrophone),
        ];
        for (choice, audio) in RecordingStartShortcut::ALL.into_iter().zip(expected) {
            assert_eq!(choice.audio(), audio);
            assert_eq!(
                RecordingStartShortcut::from_index(choice.index()),
                Ok(choice)
            );
            assert_eq!(
                RecordingStartShortcut::from_stored_name(choice.stored_name()),
                Ok(choice)
            );
        }
        assert!(RecordingStartShortcut::from_index(-1).is_err());
        assert!(RecordingStartShortcut::from_index(5).is_err());
        assert!(RecordingStartShortcut::from_stored_name("unexpected").is_err());
    }
}
