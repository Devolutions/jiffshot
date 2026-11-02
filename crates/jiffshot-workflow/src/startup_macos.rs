//! macOS login-item registration for the signed main application.

use super::{StartupNotice, StartupStatus};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_foundation::{NSBundle, NSError, NSURL};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

const BUNDLE_ID: &str = "com.devolutions.jiffshot";
const APPROVAL: &str = "Jiffshot is registered but macOS has not allowed it to open at login. Enable it in System Settings > General > Login Items & Extensions.";
const BUNDLE_REQUIRED: &str = "Open a signed Jiffshot.app to configure its login item.";
const STATUS_UNKNOWN: &str = "macOS could not find the login-item record. Try enabling it, or check System Settings > General > Login Items & Extensions.";
const LEGACY: &str = "An older Jiffshot LaunchAgent exists in ~/Library/LaunchAgents/com.devolutions.jiffshot.plist. Review and remove it before changing this setting; Jiffshot has not modified it.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServiceStatus {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
}

impl ServiceStatus {
    fn from_raw(value: isize) -> Result<Self, String> {
        match value {
            0 => Ok(Self::NotRegistered),
            1 => Ok(Self::Enabled),
            2 => Ok(Self::RequiresApproval),
            3 => Ok(Self::NotFound),
            _ => Err(format!("macOS returned an unknown login-item status ({value}).")),
        }
    }

    fn requested(self) -> bool {
        matches!(self, Self::Enabled | Self::RequiresApproval)
    }
}

trait Service {
    fn status(&self) -> Result<ServiceStatus, String>;
    fn register(&self) -> Result<(), String>;
    fn unregister(&self) -> Result<(), String>;
}

struct NativeService(Retained<AnyObject>);

impl NativeService {
    fn class() -> Result<&'static AnyClass, String> {
        AnyClass::get(c"SMAppService").ok_or_else(|| "macOS login-item service is unavailable.".to_owned())
    }

    fn main_app() -> Result<Self, String> {
        let identifier = NSBundle::mainBundle().bundleIdentifier().map(|id| id.to_string());
        if identifier.as_deref() != Some(BUNDLE_ID) {
            return Err(BUNDLE_REQUIRED.to_owned());
        }
        Self::unchecked_main_app()
    }

    fn unchecked_main_app() -> Result<Self, String> {
        // SAFETY: SMAppService is provided by ServiceManagement on macOS 13+;
        // mainAppService returns an Objective-C object retained by objc2.
        let service: Option<Retained<AnyObject>> = unsafe { msg_send![Self::class()?, mainAppService] };
        service.map(Self).ok_or_else(|| "macOS could not create the login-item service.".to_owned())
    }

    fn perform(&self, registering: bool) -> Result<(), String> {
        let mut error: *mut NSError = std::ptr::null_mut();
        // SAFETY: the live service supports both documented selectors. The
        // error pointer is read inside the surrounding autorelease pool.
        let success: bool = unsafe {
            if registering {
                msg_send![&*self.0, registerAndReturnError: &mut error]
            } else {
                msg_send![&*self.0, unregisterAndReturnError: &mut error]
            }
        };
        if success {
            return Ok(());
        }
        let detail = unsafe { error.as_ref() }
            .map(|error| error.localizedDescription().to_string())
            .unwrap_or_else(|| "macOS did not provide an error.".to_owned());
        let action = if registering { "register" } else { "unregister" };
        Err(format!("Could not {action} the login item: {detail}"))
    }
}

impl Service for NativeService {
    fn status(&self) -> Result<ServiceStatus, String> {
        // SAFETY: status is a documented NSInteger property of the live service.
        ServiceStatus::from_raw(unsafe { msg_send![&*self.0, status] })
    }

    fn register(&self) -> Result<(), String> {
        self.perform(true)
    }

    fn unregister(&self) -> Result<(), String> {
        self.perform(false)
    }
}

fn legacy_path() -> Result<Option<PathBuf>, String> {
    let home = jiffshot_capture::os::home_dir().ok_or("Could not locate the home folder to check for an older login item.")?;
    let path = home.join("Library/LaunchAgents/com.devolutions.jiffshot.plist");
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not check the older login item: {error}")),
    }
}

fn legacy_status(path: &Path) -> Result<ServiceStatus, String> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| "The older login-item path contains a NUL byte.")?;
    let ptr = NonNull::new(path.as_ptr().cast_mut()).ok_or("The older login-item path is unavailable.")?;
    // SAFETY: path is a NUL-terminated filesystem path and remains live
    // until NSURL has copied it.
    let url = unsafe { NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(ptr, false, None) };
    // SAFETY: this documented class method accepts a file URL to a legacy plist.
    ServiceStatus::from_raw(unsafe { msg_send![NativeService::class()?, statusForLegacyURL: &*url] })
}

fn describe(main: ServiceStatus, legacy: Option<ServiceStatus>) -> StartupStatus {
    let requested = main.requested() || legacy.is_some_and(ServiceStatus::requested);
    if let Some(legacy) = legacy {
        let detail = if legacy == ServiceStatus::NotFound {
            format!("{LEGACY} macOS could not determine the older item's status.")
        } else {
            LEGACY.to_owned()
        };
        StartupStatus { requested, notice: StartupNotice::Legacy, detail: Some(detail) }
    } else {
        match main {
            ServiceStatus::RequiresApproval => StartupStatus { requested, notice: StartupNotice::NeedsApproval, detail: Some(APPROVAL.to_owned()) },
            ServiceStatus::NotFound => StartupStatus { requested, notice: StartupNotice::StatusUnknown, detail: Some(STATUS_UNKNOWN.to_owned()) },
            _ => StartupStatus { requested, ..Default::default() },
        }
    }
}

pub(crate) fn status() -> StartupStatus {
    let result = autoreleasepool(|_| {
        let service = NativeService::main_app()?;
        let main = service.status()?;
        let legacy = legacy_path()?.map(|path| legacy_status(&path)).transpose()?;
        Ok::<_, String>(describe(main, legacy))
    });
    result.unwrap_or_else(|error| StartupStatus { notice: StartupNotice::Unavailable, detail: Some(error), ..Default::default() })
}

fn change(service: &impl Service, has_legacy: bool, on: bool) -> Result<Option<String>, String> {
    if has_legacy {
        return Err(LEGACY.to_owned());
    }
    let before = service.status()?;
    match (on, before) {
        (false, ServiceStatus::NotFound) => return Err(STATUS_UNKNOWN.to_owned()),
        (true, ServiceStatus::Enabled) | (false, ServiceStatus::NotRegistered) => return Ok(None),
        (true, ServiceStatus::RequiresApproval) => return Ok(Some(APPROVAL.to_owned())),
        (true, ServiceStatus::NotRegistered | ServiceStatus::NotFound) => service.register()?,
        (false, ServiceStatus::Enabled | ServiceStatus::RequiresApproval) => service.unregister()?,
    }
    match (on, service.status()?) {
        (true, ServiceStatus::Enabled) | (false, ServiceStatus::NotRegistered) => Ok(None),
        (true, ServiceStatus::RequiresApproval) => Ok(Some(APPROVAL.to_owned())),
        _ => Err("macOS did not confirm the requested login-item change. Check System Settings > General > Login Items & Extensions.".to_owned()),
    }
}

pub(crate) fn set_run_at_startup(on: bool) -> Result<Option<String>, String> {
    autoreleasepool(|_| {
        let service = NativeService::main_app()?;
        change(&service, legacy_path()?.is_some(), on)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Fake {
        status: Cell<ServiceStatus>,
        after_register: ServiceStatus,
        after_unregister: ServiceStatus,
        registers: Cell<u32>,
        unregisters: Cell<u32>,
        fail: bool,
    }

    impl Fake {
        fn new(status: ServiceStatus) -> Self {
            Self {
                status: Cell::new(status),
                after_register: ServiceStatus::Enabled,
                after_unregister: ServiceStatus::NotRegistered,
                registers: Cell::new(0),
                unregisters: Cell::new(0),
                fail: false,
            }
        }
    }

    impl Service for Fake {
        fn status(&self) -> Result<ServiceStatus, String> {
            Ok(self.status.get())
        }
        fn register(&self) -> Result<(), String> {
            self.registers.set(self.registers.get() + 1);
            if self.fail { return Err("registration failed".to_owned()); }
            self.status.set(self.after_register);
            Ok(())
        }
        fn unregister(&self) -> Result<(), String> {
            self.unregisters.set(self.unregisters.get() + 1);
            if self.fail { return Err("unregistration failed".to_owned()); }
            self.status.set(self.after_unregister);
            Ok(())
        }
    }

    #[test]
    fn status_distinguishes_approval_legacy_and_unavailable() {
        assert_eq!(describe(ServiceStatus::NotRegistered, None), StartupStatus::default());
        assert!(describe(ServiceStatus::Enabled, None).requested);
        let approval = describe(ServiceStatus::RequiresApproval, None);
        assert!(approval.requested);
        assert_eq!(approval.notice, StartupNotice::NeedsApproval);
        assert!(approval.detail.unwrap().contains("System Settings"));
        let legacy = describe(ServiceStatus::NotRegistered, Some(ServiceStatus::RequiresApproval));
        assert!(legacy.requested);
        assert_eq!(legacy.notice, StartupNotice::Legacy);
        assert_eq!(describe(ServiceStatus::NotFound, None).notice, StartupNotice::StatusUnknown);
        assert!(ServiceStatus::from_raw(4).is_err());
    }

    #[test]
    fn changing_login_item_confirms_the_os_state_and_respects_approval() {
        let service = Fake::new(ServiceStatus::NotRegistered);
        assert_eq!(change(&service, false, true), Ok(None));
        assert_eq!(service.registers.get(), 1);
        assert_eq!(change(&service, false, true), Ok(None));
        assert_eq!(service.registers.get(), 1);
        service.status.set(ServiceStatus::RequiresApproval);
        assert!(change(&service, false, true).unwrap().unwrap().contains("System Settings"));
        assert_eq!(service.registers.get(), 1);
        assert_eq!(change(&service, false, false), Ok(None));
        assert_eq!(service.unregisters.get(), 1);
    }

    #[test]
    fn failures_and_legacy_items_never_report_success() {
        let service = Fake::new(ServiceStatus::NotRegistered);
        assert!(change(&service, true, true).unwrap_err().contains("older Jiffshot LaunchAgent"));
        assert_eq!(service.registers.get(), 0);
        service.status.set(ServiceStatus::Enabled);
        assert!(change(&service, true, false).unwrap_err().contains("older Jiffshot LaunchAgent"));
        assert_eq!(service.unregisters.get(), 0);
        let failed = Fake { fail: true, ..Fake::new(ServiceStatus::Enabled) };
        assert_eq!(change(&failed, false, false).unwrap_err(), "unregistration failed");
        let unconfirmed = Fake { after_register: ServiceStatus::NotRegistered, ..Fake::new(ServiceStatus::NotRegistered) };
        assert!(change(&unconfirmed, false, true).unwrap_err().contains("did not confirm"));
        let pending = Fake { after_register: ServiceStatus::RequiresApproval, ..Fake::new(ServiceStatus::NotRegistered) };
        assert!(change(&pending, false, true).unwrap().unwrap().contains("not allowed"));
        let not_found = Fake::new(ServiceStatus::NotFound);
        assert_eq!(change(&not_found, false, true), Ok(None));
        assert_eq!(not_found.registers.get(), 1);
    }

    #[test]
    #[ignore = "registers and removes a unique signed test bundle's login item"]
    fn native_login_item_round_trip_in_an_isolated_bundle() {
        autoreleasepool(|_| {
            let service = isolated_test_service();
            let initial = service.status().unwrap();
            assert!(matches!(initial, ServiceStatus::NotRegistered | ServiceStatus::NotFound), "the fixture must not have a prior login item");
            println!("Initial isolated login-item status: {initial:?}");

            struct Cleanup<'a>(&'a NativeService);
            impl Drop for Cleanup<'_> {
                fn drop(&mut self) {
                    if !matches!(self.0.status(), Ok(ServiceStatus::NotRegistered | ServiceStatus::NotFound)) {
                        if let Err(error) = self.0.unregister() {
                            eprintln!("Could not remove the isolated test login item: {error}");
                        }
                    }
                }
            }
            let cleanup = Cleanup(&service);
            let warning = change(&service, false, true).expect("register the isolated login item");
            let registered = service.status().unwrap();
            assert!(registered.requested(), "registration was not reflected in macOS: {registered:?}");
            assert_eq!(warning.is_some(), registered == ServiceStatus::RequiresApproval);
            println!("Registered isolated login item: {registered:?}");
            assert_eq!(change(&service, false, false), Ok(None));
            assert_eq!(service.status().unwrap(), ServiceStatus::NotRegistered);
            drop(cleanup);
        });
    }

    fn isolated_test_service() -> NativeService {
        let expected = std::env::var("JIFFSHOT_LOGIN_SELFTEST_BUNDLE_ID").expect("the fixture must supply its unique bundle ID");
        assert!(expected.starts_with("com.devolutions.jiffshot.login-selftest."));
        let actual = NSBundle::mainBundle().bundleIdentifier().expect("the test must run inside a signed app bundle").to_string();
        assert_eq!(actual, expected);
        NativeService::unchecked_main_app().expect("login-item service")
    }

    #[test]
    #[ignore = "cleans up a unique signed test bundle's login item"]
    fn native_login_item_cleanup_in_an_isolated_bundle() {
        autoreleasepool(|_| {
            let service = isolated_test_service();
            if !matches!(service.status(), Ok(ServiceStatus::NotRegistered | ServiceStatus::NotFound)) {
                service.unregister().expect("remove the isolated test login item");
            }
            assert!(matches!(service.status().unwrap(), ServiceStatus::NotRegistered | ServiceStatus::NotFound));
        });
    }
}
