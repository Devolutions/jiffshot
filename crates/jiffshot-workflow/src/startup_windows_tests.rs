use super::*;
use std::cell::{Cell, RefCell};

struct FakeService {
    backend: BackendKind,
    state: Cell<Registration>,
    enable_result: Cell<Option<Registration>>,
    enables: Cell<usize>,
    disables: Cell<usize>,
    fail_enable: Cell<bool>,
    fail_disable: Cell<bool>,
    fail_status: Cell<bool>,
    legacy: Cell<bool>,
    retired: Cell<usize>,
    default_allowed: Cell<bool>,
    fail_approval: Cell<bool>,
    fail_legacy_probe: Cell<bool>,
    fail_retire: Cell<bool>,
}

impl FakeService {
    fn new(backend: BackendKind, state: Registration) -> Self {
        Self {
            backend,
            state: Cell::new(state),
            enable_result: Cell::new(None),
            enables: Cell::new(0),
            disables: Cell::new(0),
            fail_enable: Cell::new(false),
            fail_disable: Cell::new(false),
            fail_status: Cell::new(false),
            legacy: Cell::new(false),
            retired: Cell::new(0),
            default_allowed: Cell::new(true),
            fail_approval: Cell::new(false),
            fail_legacy_probe: Cell::new(false),
            fail_retire: Cell::new(false),
        }
    }
}

impl Service for FakeService {
    fn backend(&self) -> BackendKind {
        self.backend
    }

    fn status(&self) -> Result<Registration, String> {
        if self.fail_status.get() {
            Err("status unavailable".into())
        } else {
            Ok(self.state.get())
        }
    }

    fn enable(&self) -> Result<Registration, String> {
        self.enables.set(self.enables.get() + 1);
        if self.fail_enable.get() {
            return Err("registration failed".into());
        }
        let state = self
            .enable_result
            .get()
            .unwrap_or_else(|| match self.state.get() {
                Registration::DisabledByUser
                | Registration::DisabledByPolicy
                | Registration::EnabledByPolicy => self.state.get(),
                _ => Registration::Enabled,
            });
        self.state.set(state);
        Ok(state)
    }

    fn disable(&self) -> Result<Registration, String> {
        self.disables.set(self.disables.get() + 1);
        if self.fail_disable.get() {
            return Err("could not disable startup".into());
        }
        let state = if self.state.get() == Registration::EnabledByPolicy {
            Registration::EnabledByPolicy
        } else {
            Registration::Disabled
        };
        self.state.set(state);
        Ok(state)
    }

    fn retire_legacy(&self) -> Result<bool, String> {
        self.retired.set(self.retired.get() + 1);
        if self.fail_retire.get() {
            return Err("legacy removal failed".into());
        }
        Ok(self.legacy.replace(false))
    }

    fn has_unresolved_run_entry(&self) -> Result<bool, String> {
        if self.fail_legacy_probe.get() {
            Err("legacy status unavailable".into())
        } else {
            Ok(self.legacy.get())
        }
    }

    fn allow_default(&self) -> Result<bool, String> {
        if self.fail_approval.get() {
            Err("approval status unavailable".into())
        } else {
            Ok(self.default_allowed.get())
        }
    }
}

#[derive(Default)]
struct MemoryStore {
    record: RefCell<Option<Record>>,
    fail_write: Cell<bool>,
}

impl Store for MemoryStore {
    fn load(&self) -> Result<Option<Record>, String> {
        Ok(self.record.borrow().clone())
    }

    fn save(&self, record: &Record) -> Result<(), String> {
        if self.fail_write.get() {
            return Err("settings are not writable".into());
        }
        *self.record.borrow_mut() = Some(record.clone());
        Ok(())
    }
}

#[test]
fn first_launch_enables_once_and_does_not_reverse_an_external_disable() {
    let service = FakeService::new(BackendKind::Desktop, Registration::Disabled);
    let store = MemoryStore::default();

    let first = initialize(&service, &store).unwrap();
    assert!(!first.legacy_launch_ambiguous);
    assert_eq!(service.state.get(), Registration::Enabled);
    assert_eq!(service.enables.get(), 1);
    assert!(store.record.borrow().as_ref().unwrap().requested);

    service.state.set(Registration::Disabled);
    initialize(&service, &store).unwrap();
    assert_eq!(service.enables.get(), 1);
    assert!(!store.record.borrow().as_ref().unwrap().requested);
}

#[test]
fn unconfirmed_default_registration_is_reported_without_retrying_a_disable() {
    for (backend, initial) in [
        (BackendKind::Packaged, Registration::Disabled),
        (BackendKind::Desktop, Registration::EnabledLegacy),
    ] {
        let service = FakeService::new(backend, initial);
        service.enable_result.set(Some(initial));
        let store = MemoryStore::default();

        assert!(initialize(&service, &store)
            .unwrap_err()
            .contains("did not confirm"));
        assert_eq!(service.enables.get(), 1);
        assert_eq!(service.retired.get(), 0);
        assert_eq!(
            store.record.borrow().as_ref().unwrap().requested,
            initial.enabled()
        );
        if initial == Registration::Disabled {
            initialize(&service, &store).unwrap();
            assert_eq!(service.enables.get(), 1);
        }
    }
}

#[test]
fn windows_disabling_during_default_registration_is_respected() {
    for blocked in [Registration::DisabledByUser, Registration::DisabledByPolicy] {
        let service = FakeService::new(BackendKind::Packaged, Registration::Disabled);
        service.enable_result.set(Some(blocked));
        let store = MemoryStore::default();

        initialize(&service, &store).unwrap();
        assert_eq!(service.state.get(), blocked);
        assert!(!store.record.borrow().as_ref().unwrap().requested);
        assert_eq!(service.enables.get(), 1);
    }
}

#[test]
fn unconfirmed_registration_and_settings_failure_are_both_reported() {
    let service = FakeService::new(BackendKind::Packaged, Registration::Disabled);
    service.enable_result.set(Some(Registration::Disabled));
    let store = MemoryStore::default();
    store.fail_write.set(true);

    let error = initialize(&service, &store).unwrap_err();
    assert!(error.contains("did not confirm"));
    assert!(error.contains("settings are not writable"));
    assert!(store.record.borrow().is_none());
}

#[test]
fn a_legacy_run_command_is_migrated_and_its_first_launch_is_ambiguous() {
    let service = FakeService::new(BackendKind::Desktop, Registration::EnabledLegacy);
    let store = MemoryStore::default();

    let first = initialize(&service, &store).unwrap();
    assert!(first.legacy_launch_ambiguous);
    assert_eq!(service.state.get(), Registration::Enabled);
    assert_eq!(service.enables.get(), 1);

    assert!(
        !initialize(&service, &store)
            .unwrap()
            .legacy_launch_ambiguous
    );
    assert_eq!(service.enables.get(), 1);
}

#[test]
fn opt_out_survives_restart_and_a_packaging_change() {
    let desktop = FakeService::new(BackendKind::Desktop, Registration::Disabled);
    let store = MemoryStore::default();
    initialize(&desktop, &store).unwrap();
    change(&desktop, &store, false).unwrap();

    initialize(&desktop, &store).unwrap();
    assert_eq!(desktop.enables.get(), 1);
    assert_eq!(desktop.state.get(), Registration::Disabled);

    let packaged = FakeService::new(BackendKind::Packaged, Registration::Disabled);
    initialize(&packaged, &store).unwrap();
    assert_eq!(packaged.enables.get(), 0);
    assert_eq!(
        store.record.borrow().as_ref().unwrap().backend,
        BackendKind::Packaged
    );
    assert!(!store.record.borrow().as_ref().unwrap().requested);
}

#[test]
fn enabled_intent_is_migrated_when_the_installation_mechanism_changes() {
    let store = MemoryStore::default();
    *store.record.borrow_mut() = Some(Record {
        backend: BackendKind::Desktop,
        requested: true,
    });
    let packaged = FakeService::new(BackendKind::Packaged, Registration::Disabled);

    initialize(&packaged, &store).unwrap();
    assert_eq!(packaged.enables.get(), 1);
    assert_eq!(packaged.state.get(), Registration::Enabled);
    assert_eq!(
        store.record.borrow().as_ref().unwrap().backend,
        BackendKind::Packaged
    );
}

#[test]
fn packaged_migration_retires_owned_legacy_commands_only_once() {
    let store = MemoryStore::default();
    let packaged = FakeService::new(BackendKind::Packaged, Registration::Disabled);
    packaged.legacy.set(true);

    assert!(
        initialize(&packaged, &store)
            .unwrap()
            .legacy_launch_ambiguous
    );
    assert_eq!(packaged.retired.get(), 1);
    assert!(
        !initialize(&packaged, &store)
            .unwrap()
            .legacy_launch_ambiguous
    );
    assert_eq!(packaged.retired.get(), 1);
}

#[test]
fn user_and_policy_disables_are_never_overridden_during_initialization() {
    for state in [Registration::DisabledByUser, Registration::DisabledByPolicy] {
        let service = FakeService::new(BackendKind::Packaged, state);
        let store = MemoryStore::default();
        initialize(&service, &store).unwrap();
        assert_eq!(service.enables.get(), 0);
        assert_eq!(service.state.get(), state);
        assert!(!store.record.borrow().as_ref().unwrap().requested);
        assert!(change(&service, &store, true).is_err());
    }
}

#[test]
fn packaged_windows_disables_do_not_leave_an_owned_legacy_entry_enabled() {
    for state in [Registration::DisabledByUser, Registration::DisabledByPolicy] {
        let service = FakeService::new(BackendKind::Packaged, state);
        service.legacy.set(true);
        let store = MemoryStore::default();

        assert!(
            initialize(&service, &store)
                .unwrap()
                .legacy_launch_ambiguous
        );
        assert_eq!(service.state.get(), state);
        assert_eq!(service.enables.get(), 0);
        assert!(!service.legacy.get());
        assert!(!store.record.borrow().as_ref().unwrap().requested);
    }
}

#[test]
fn changing_packaged_startup_cleans_up_an_owned_legacy_run_entry() {
    let service = FakeService::new(BackendKind::Packaged, Registration::Enabled);
    service.legacy.set(true);
    service.default_allowed.set(false);
    let store = MemoryStore::default();

    change(&service, &store, false).unwrap();
    assert_eq!(service.state.get(), Registration::Disabled);
    assert!(!service.legacy.get());
    assert!(!store.record.borrow().as_ref().unwrap().requested);

    service.legacy.set(true);
    change(&service, &store, true).unwrap();
    assert_eq!(service.state.get(), Registration::Enabled);
    assert!(!service.legacy.get());
    assert!(store.record.borrow().as_ref().unwrap().requested);
}

#[test]
fn packaged_opt_out_reports_legacy_cleanup_failures() {
    let service = FakeService::new(BackendKind::Packaged, Registration::Enabled);
    service.legacy.set(true);
    service.fail_retire.set(true);
    let store = MemoryStore::default();

    assert!(change(&service, &store, false)
        .unwrap_err()
        .contains("legacy removal failed"));
    assert_eq!(service.state.get(), Registration::Disabled);
    assert!(service.legacy.get());
    assert!(!store.record.borrow().as_ref().unwrap().requested);
}

#[test]
fn an_older_windows_disable_is_not_bypassed_by_a_packaged_task() {
    let service = FakeService::new(BackendKind::Packaged, Registration::Disabled);
    service.legacy.set(true);
    service.default_allowed.set(false);
    let store = MemoryStore::default();

    initialize(&service, &store).unwrap();
    assert_eq!(service.enables.get(), 0);
    assert_eq!(service.retired.get(), 0);
    assert!(service.legacy.get());
    assert!(!store.record.borrow().as_ref().unwrap().requested);

    change(&service, &store, true).unwrap();
    assert_eq!(service.enables.get(), 1);
    assert!(!service.legacy.get());
    assert_eq!(service.retired.get(), 1);
    assert!(store.record.borrow().as_ref().unwrap().requested);
}

#[test]
fn desktop_registration_reports_that_windows_may_have_disabled_it() {
    use crate::StartupNotice;

    let service = FakeService::new(BackendKind::Desktop, Registration::Enabled);
    service.default_allowed.set(false);

    let status = read_status(&service).unwrap();
    assert!(status.requested);
    assert_eq!(status.notice, StartupNotice::StatusUnknown);
    assert!(status
        .detail
        .unwrap()
        .contains("Windows may also have disabled"));

    service.state.set(Registration::EnabledLegacy);
    let status = read_status(&service).unwrap();
    assert!(status.requested);
    assert_eq!(status.notice, StartupNotice::Legacy);
    let detail = status.detail.unwrap();
    assert!(detail.contains("older Jiffshot startup command"));
    assert!(detail.contains("Windows may also have disabled"));
}

#[test]
fn packaged_status_reports_unresolved_entries_and_probe_errors() {
    use crate::StartupNotice;

    let service = FakeService::new(BackendKind::Packaged, Registration::Enabled);
    service.legacy.set(true);
    let status = read_status(&service).unwrap();
    assert!(status.requested);
    assert_eq!(status.notice, StartupNotice::Legacy);
    assert!(status.detail.unwrap().contains("Run entry remains"));

    service.fail_legacy_probe.set(true);
    let status = read_status(&service).unwrap();
    assert!(status.requested);
    assert_eq!(status.notice, StartupNotice::StatusUnknown);
    assert!(status.detail.unwrap().contains("legacy status unavailable"));
}

#[test]
fn packaged_status_keeps_windows_restrictions_and_warns_about_legacy_entries() {
    use crate::StartupNotice;

    let service = FakeService::new(BackendKind::Packaged, Registration::DisabledByUser);
    service.legacy.set(true);

    let status = read_status(&service).unwrap();
    assert!(!status.requested);
    assert_eq!(status.notice, StartupNotice::NeedsApproval);
    let detail = status.detail.unwrap();
    assert!(detail.contains("disabled in Windows Startup settings"));
    assert!(detail.contains("Run entry remains"));

    service.state.set(Registration::EnabledByPolicy);
    let status = read_status(&service).unwrap();
    assert!(status.requested);
    assert_eq!(status.notice, StartupNotice::Legacy);
    let detail = status.detail.unwrap();
    assert!(detail.contains("Windows policy requires"));
    assert!(detail.contains("Run entry remains"));
}

#[test]
fn status_reports_failure_to_check_windows_approval() {
    use crate::StartupNotice;

    let service = FakeService::new(BackendKind::Desktop, Registration::Enabled);
    service.fail_approval.set(true);

    let status = read_status(&service).unwrap();
    assert!(status.requested);
    assert_eq!(status.notice, StartupNotice::StatusUnknown);
    assert!(status
        .detail
        .unwrap()
        .contains("approval status unavailable"));
}

#[test]
fn a_legacy_entry_is_migrated_after_windows_reenables_it() {
    let service = FakeService::new(BackendKind::Desktop, Registration::DisabledByUser);
    let store = MemoryStore::default();
    initialize(&service, &store).unwrap();
    assert_eq!(service.enables.get(), 0);

    service.state.set(Registration::EnabledLegacy);
    assert!(
        initialize(&service, &store)
            .unwrap()
            .legacy_launch_ambiguous
    );
    assert_eq!(service.state.get(), Registration::Enabled);
}

#[test]
fn registration_and_settings_failures_are_reported_without_claiming_success() {
    let service = FakeService::new(BackendKind::Desktop, Registration::Disabled);
    let store = MemoryStore::default();
    service.fail_enable.set(true);
    assert!(initialize(&service, &store)
        .unwrap_err()
        .contains("registration failed"));
    assert!(store.record.borrow().is_none());
    assert_eq!(service.state.get(), Registration::Disabled);

    service.fail_enable.set(false);
    initialize(&service, &store).unwrap();
    store.fail_write.set(true);
    assert!(change(&service, &store, false)
        .unwrap_err()
        .contains("settings are not writable"));
    assert_eq!(service.disables.get(), 0);
    assert_eq!(service.state.get(), Registration::Enabled);
}

#[test]
fn failed_status_and_disable_are_reported_without_hiding_the_actual_state() {
    let service = FakeService::new(BackendKind::Desktop, Registration::Disabled);
    let store = MemoryStore::default();
    service.fail_status.set(true);
    assert!(initialize(&service, &store)
        .unwrap_err()
        .contains("status unavailable"));
    assert_eq!(service.enables.get(), 0);
    assert!(store.record.borrow().is_none());

    service.fail_status.set(false);
    initialize(&service, &store).unwrap();
    service.fail_disable.set(true);
    assert!(change(&service, &store, false)
        .unwrap_err()
        .contains("could not disable startup"));
    assert_eq!(service.state.get(), Registration::Enabled);
    initialize(&service, &store).unwrap();
    assert!(store.record.borrow().as_ref().unwrap().requested);
    assert_eq!(service.enables.get(), 1);
}

#[test]
fn background_is_an_application_mode_not_a_command_line_utility() {
    for args in [
        Vec::<String>::new(),
        vec!["--background".into()],
        vec!["--window".into()],
        vec!["--capture".into()],
        vec!["image.png".into()],
    ] {
        assert!(crate::run_command(&args).is_none(), "{args:?}");
    }
}
