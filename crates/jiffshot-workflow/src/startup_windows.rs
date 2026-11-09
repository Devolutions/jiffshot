use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[path = "startup_windows_backend.rs"]
mod backend;

const SETTINGS_SECTION: &str = "windows_startup";
static INITIALIZATION_ERROR: Mutex<Option<String>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BackendKind {
    Desktop,
    Packaged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Registration {
    Disabled,
    Enabled,
    EnabledLegacy,
    DisabledByUser,
    DisabledByPolicy,
    EnabledByPolicy,
}

impl Registration {
    fn enabled(self) -> bool {
        matches!(
            self,
            Self::Enabled | Self::EnabledLegacy | Self::EnabledByPolicy
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Record {
    backend: BackendKind,
    requested: bool,
}

trait Service {
    fn backend(&self) -> BackendKind;
    fn status(&self) -> Result<Registration, String>;
    fn enable(&self) -> Result<Registration, String>;
    fn disable(&self) -> Result<Registration, String>;
    fn retire_legacy(&self) -> Result<bool, String> {
        Ok(false)
    }
    fn has_unresolved_run_entry(&self) -> Result<bool, String> {
        Ok(false)
    }
    fn allow_default(&self) -> Result<bool, String> {
        Ok(true)
    }
}

trait Store {
    fn load(&self) -> Result<Option<Record>, String>;
    fn save(&self, record: &Record) -> Result<(), String>;
}

struct SettingsStore;

impl Store for SettingsStore {
    fn load(&self) -> Result<Option<Record>, String> {
        jiffshot_capture::config::try_section(SETTINGS_SECTION)
            .map_err(|error| format!("Could not read startup settings: {error}"))?
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(|error| format!("Invalid startup settings: {error}"))
            })
            .transpose()
    }

    fn save(&self, record: &Record) -> Result<(), String> {
        let value = serde_json::to_value(record)
            .map_err(|error| format!("Could not encode startup settings: {error}"))?;
        jiffshot_capture::config::try_set_section(SETTINGS_SECTION, value)
            .map_err(|error| format!("Could not save startup settings: {error}"))
    }
}

#[derive(Debug)]
struct Initialization {
    legacy_launch_ambiguous: bool,
}

fn initialize(service: &impl Service, store: &impl Store) -> Result<Initialization, String> {
    let previous = store.load()?;
    let initial = service.status()?;
    let first = previous.is_none();
    let changed_backend = previous
        .as_ref()
        .is_some_and(|record| record.backend != service.backend());
    let should_enable =
        first || (changed_backend && previous.as_ref().is_some_and(|record| record.requested));
    let mut legacy_launch_ambiguous = initial == Registration::EnabledLegacy;
    let should_check_approval = (should_enable && initial == Registration::Disabled)
        || initial == Registration::EnabledLegacy
        || (service.backend() == BackendKind::Packaged && (first || changed_backend));
    let allow_default = !should_check_approval || service.allow_default()?;
    let needs_enable = allow_default
        && ((initial == Registration::Disabled && should_enable)
            || initial == Registration::EnabledLegacy);
    let current = if needs_enable {
        service.enable()?
    } else {
        initial
    };
    let unconfirmed = needs_enable
        && matches!(
            current,
            Registration::Disabled | Registration::EnabledLegacy
        );
    if !unconfirmed
        && service.backend() == BackendKind::Packaged
        && (first || changed_backend)
        && allow_default
    {
        legacy_launch_ambiguous |= service.retire_legacy()?;
    }
    let record = Record {
        backend: service.backend(),
        requested: current.enabled(),
    };
    const UNCONFIRMED: &str =
        "Windows did not confirm the requested startup registration. Check Windows Startup settings.";
    if previous.as_ref() != Some(&record) {
        store.save(&record).map_err(|error| {
            if unconfirmed {
                format!("{UNCONFIRMED} {error}")
            } else {
                error
            }
        })?;
    }
    if unconfirmed {
        return Err(UNCONFIRMED.into());
    }
    Ok(Initialization {
        legacy_launch_ambiguous,
    })
}

fn change(service: &impl Service, store: &impl Store, on: bool) -> Result<(), String> {
    let current = service.status()?;
    store.load()?;
    if on {
        match current {
            Registration::DisabledByUser => {
                return Err(
                    "Jiffshot was disabled in Windows Startup settings. Enable it there first."
                        .into(),
                );
            }
            Registration::DisabledByPolicy => {
                return Err("Windows policy prevents Jiffshot from starting automatically.".into());
            }
            _ => {}
        }
        let updated = service.enable()?;
        if !updated.enabled() {
            return Err(
                "Windows did not enable Jiffshot's startup task. Check Windows Startup settings."
                    .into(),
            );
        }
        if service.backend() == BackendKind::Packaged {
            service.retire_legacy()?;
        }
        store.save(&Record {
            backend: service.backend(),
            requested: true,
        })?;
    } else {
        if current == Registration::EnabledByPolicy {
            return Err("Windows policy requires Jiffshot to start automatically.".into());
        }
        store.save(&Record {
            backend: service.backend(),
            requested: false,
        })?;
        if service.disable()?.enabled() {
            return Err("Windows did not disable Jiffshot's startup task.".into());
        }
        if service.backend() == BackendKind::Packaged {
            service.retire_legacy()?;
        }
    }
    Ok(())
}

pub(crate) fn prepare() -> Result<bool, String> {
    if std::env::var_os("JIFFSHOT_UITEST").is_some() {
        return Err("Windows startup registration is disabled during UI self-tests.".into());
    }
    let result = backend::SystemService::new()
        .and_then(|service| initialize(&service, &SettingsStore))
        .map(|outcome| outcome.legacy_launch_ambiguous);
    *INITIALIZATION_ERROR
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = result.as_ref().err().cloned();
    result
}

pub(crate) fn is_startup_activation() -> Result<bool, String> {
    backend::is_startup_activation()
}

fn read_status(service: &impl Service) -> Result<crate::StartupStatus, String> {
    use crate::{StartupNotice, StartupStatus};

    let current = service.status()?;
    let (mut notice, mut detail) = match current {
        Registration::DisabledByUser => (
            StartupNotice::NeedsApproval,
            Some("Jiffshot is disabled in Windows Startup settings. Enable it there to start at sign-in.".into()),
        ),
        Registration::DisabledByPolicy => (
            StartupNotice::Unavailable,
            Some("Windows policy prevents Jiffshot from starting automatically.".into()),
        ),
        Registration::EnabledByPolicy => (
            StartupNotice::None,
            Some("Windows policy requires Jiffshot to start automatically.".into()),
        ),
        Registration::EnabledLegacy => (
            StartupNotice::Legacy,
            Some("An older Jiffshot startup command needs to be updated.".into()),
        ),
        _ if service.backend() == BackendKind::Desktop => (
            StartupNotice::None,
            Some("Windows can also disable Jiffshot in Settings > Apps > Startup; that choice is not reflected here.".into()),
        ),
        _ => (StartupNotice::None, None),
    };
    if service.backend() == BackendKind::Packaged {
        let warning = match service.has_unresolved_run_entry() {
            Ok(true) => Some((
                StartupNotice::Legacy,
                "Another Jiffshot Run entry remains and may also start the app. Review Windows Startup settings.".to_owned(),
            )),
            Err(error) => Some((
                StartupNotice::StatusUnknown,
                format!("Could not check for an older startup entry: {error}"),
            )),
            Ok(false) => None,
        };
        if let Some((warning_notice, warning)) = warning {
            if notice == StartupNotice::None {
                notice = warning_notice;
            }
            detail = Some(match detail {
                Some(existing) => format!("{existing} {warning}"),
                None => warning,
            });
        }
    }
    if notice == StartupNotice::None || notice == StartupNotice::Legacy {
        let warning = match service.allow_default() {
            Ok(false) if service.backend() == BackendKind::Desktop => Some(
                "Windows may also have disabled this registration. Check Settings > Apps > Startup; the Jiffshot switch controls only the Run entry.".to_owned(),
            ),
            Ok(false) if !current.enabled() => Some(
                "A previous Windows Startup choice could not be safely migrated. Review Windows Startup settings, then use the Jiffshot switch to enable startup if desired.".to_owned(),
            ),
            Err(error) => Some(format!("Could not check the earlier Windows Startup choice: {error}")),
            _ => None,
        };
        if let Some(warning) = warning {
            if notice == StartupNotice::None {
                notice = StartupNotice::StatusUnknown;
            }
            detail = Some(match detail {
                Some(existing) => format!("{existing} {warning}"),
                None => warning,
            });
        }
    }
    Ok(StartupStatus {
        requested: current.enabled(),
        notice,
        detail,
    })
}

pub(crate) fn status() -> crate::StartupStatus {
    use crate::{StartupNotice, StartupStatus};

    let mut status = match backend::SystemService::new().and_then(|service| read_status(&service)) {
        Ok(status) => status,
        Err(error) => {
            return StartupStatus {
                notice: StartupNotice::Unavailable,
                detail: Some(error),
                ..Default::default()
            };
        }
    };
    let error = INITIALIZATION_ERROR
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if let Some(error) = error {
        status.notice = StartupNotice::Unavailable;
        status.detail = Some(error);
    }
    status
}

pub(crate) fn set_run_at_startup(on: bool) -> Result<(), String> {
    if std::env::var_os("JIFFSHOT_UITEST").is_some() {
        return Err("Windows startup cannot be changed during UI self-tests.".into());
    }
    let service = backend::SystemService::new()?;
    change(&service, &SettingsStore, on)?;
    *INITIALIZATION_ERROR
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
    Ok(())
}

#[cfg(test)]
#[path = "startup_windows_tests.rs"]
mod tests;
