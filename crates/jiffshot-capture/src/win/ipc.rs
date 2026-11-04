//! Single-instance hand-off: the running Jiffshot listens on a per-user,
//! per-session named pipe, and a second launch sends it the file to open.

use crate::wide;
use std::io;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_WRITE, FlushFileBuffers,
    OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    sid: String,
    session: u32,
}

fn identities_match(expected: &Identity, actual: &Identity) -> bool {
    expected == actual
}

fn pipe_name(identity: &Identity) -> String {
    format!(
        r"\\.\pipe\Jiffshot.Editor.{}.{}",
        identity.sid, identity.session
    )
}

fn owner_only_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns a valid Windows handle.
        unsafe { CloseHandle(self.0) };
    }
}

fn token_sid(token: HANDLE) -> io::Result<String> {
    // SAFETY: the first call only obtains the required size; the second writes
    // into an aligned buffer of that size. The returned SID is copied and freed.
    unsafe {
        let mut size = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0_u64; (size as usize).div_ceil(std::mem::size_of::<u64>())];
        if GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = (0..256).take_while(|&index| *text.add(index) != 0).count();
        if length == 256 {
            LocalFree(text.cast());
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned an invalid user SID",
            ));
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        Ok(sid)
    }
}

fn process_identity(process_id: u32) -> io::Result<Identity> {
    // SAFETY: OpenProcess returns an owned handle or null on failure.
    let process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) });
    if process.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut token = std::ptr::null_mut();
    // SAFETY: process is valid and token is a writable output pointer.
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let sid = token_sid(token.0)?;
    let mut session = 0;
    // SAFETY: session points to writable storage.
    if unsafe { ProcessIdToSessionId(process_id, &mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Identity { sid, session })
}

fn current_identity() -> io::Result<Identity> {
    // SAFETY: this parameterless Win32 call returns the current process ID.
    process_identity(unsafe { GetCurrentProcessId() })
}

struct OwnerOnly(*mut core::ffi::c_void);

impl OwnerOnly {
    fn new(sid: &str) -> io::Result<Self> {
        let sddl = wide(&owner_only_sddl(sid));
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: SDDL is NUL-terminated; the allocated descriptor is freed on drop.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
}

impl Drop for OwnerOnly {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

struct OwnedPipe(isize);

impl Drop for OwnedPipe {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the pipe handle for the server thread.
        unsafe { CloseHandle(self.0 as HANDLE) };
    }
}

/// Sends `path` to a running editor. Returns false if no trusted editor listens.
/// The editor may take the foreground because this process received the hotkey.
pub fn send_to_editor(path: &Path) -> bool {
    let Ok(identity) = current_identity() else {
        return false;
    };
    send_on(&pipe_name(&identity), &identity, path)
}

fn send_on(name: &str, identity: &Identity, path: &Path) -> bool {
    let name = wide(name);
    // SAFETY: the handle is checked and closed; the buffer outlives WriteFile.
    unsafe {
        let pipe = CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if pipe == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut server = 0;
        let trusted_server = GetNamedPipeServerProcessId(pipe, &mut server) != 0
            && process_identity(server)
                .is_ok_and(|server_identity| identities_match(identity, &server_identity));
        if !trusted_server {
            CloseHandle(pipe);
            return false;
        }
        AllowSetForegroundWindow(server);
        let message = path.to_string_lossy().into_owned().into_bytes();
        let Ok(message_len) = u32::try_from(message.len()) else {
            CloseHandle(pipe);
            return false;
        };
        let mut written = 0;
        let ok = WriteFile(
            pipe,
            message.as_ptr(),
            message_len,
            &mut written,
            std::ptr::null_mut(),
        ) != 0
            && written == message_len;
        FlushFileBuffers(pipe);
        CloseHandle(pipe);
        ok
    }
}

/// Listens for paths on a background thread and passes each to `handler`.
/// Fails if another editor already owns the pipe.
pub fn serve(handler: impl Fn(PathBuf) + Send + 'static) -> io::Result<()> {
    let identity = current_identity()?;
    let name = pipe_name(&identity);
    serve_on(&name, identity, handler)
}

fn serve_on(
    name: &str,
    identity: Identity,
    handler: impl Fn(PathBuf) + Send + 'static,
) -> io::Result<()> {
    let name = wide(name);
    let security = OwnerOnly::new(&identity.sid)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
    // SAFETY: NUL-terminated name and security attributes live through the call.
    let pipe = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            64 * 1024,
            0,
            &attributes,
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let pipe = OwnedPipe(pipe as isize);
    let expected_identity = identity;
    std::thread::Builder::new()
        .name("editor-ipc".into())
        .spawn(move || {
            // Own the whole guard: capturing only `pipe.0` would drop it, and
            // close the handle, as soon as `serve` returns.
            let pipe = pipe;
            loop {
                let pipe_handle = pipe.0 as HANDLE;
                // SAFETY: `pipe` owns a live server handle for this thread.
                unsafe {
                    let connected = ConnectNamedPipe(pipe_handle, std::ptr::null_mut()) != 0
                        || GetLastError() == ERROR_PIPE_CONNECTED;
                    if connected {
                        let mut client = 0;
                        let trusted_client = GetNamedPipeClientProcessId(pipe_handle, &mut client)
                            != 0
                            && process_identity(client).is_ok_and(|client_identity| {
                                identities_match(&expected_identity, &client_identity)
                            });
                        if trusted_client {
                            let mut buf = vec![0u8; 64 * 1024];
                            let mut read = 0;
                            if ReadFile(
                                pipe_handle,
                                buf.as_mut_ptr(),
                                buf.len() as u32,
                                &mut read,
                                std::ptr::null_mut(),
                            ) != 0
                                && read > 0
                            {
                                buf.truncate(read as usize);
                                handler(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()));
                            }
                        }
                    }
                    DisconnectNamedPipe(pipe_handle);
                }
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{BOOL, GENERIC_ALL, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION, AclSizeInformation, EqualSid, GetAce,
        GetAclInformation, GetSecurityDescriptorDacl,
    };

    #[test]
    fn editor_pipe_is_scoped_to_sid_and_session() {
        let identity = Identity {
            sid: "S-1-5-21-1-2-3-1000".into(),
            session: 7,
        };

        assert_eq!(
            pipe_name(&identity),
            r"\\.\pipe\Jiffshot.Editor.S-1-5-21-1-2-3-1000.7"
        );
        assert_eq!(
            owner_only_sddl(&identity.sid),
            "D:P(A;;GA;;;S-1-5-21-1-2-3-1000)"
        );
    }

    #[test]
    fn editor_pipe_peer_must_match_sid_and_session() {
        let expected = Identity {
            sid: "S-1-5-21-1-2-3-1000".into(),
            session: 7,
        };
        assert!(identities_match(&expected, &expected));
        assert!(!identities_match(
            &expected,
            &Identity {
                sid: "S-1-5-21-1-2-3-2000".into(),
                session: 7,
            }
        ));
        assert!(!identities_match(
            &expected,
            &Identity {
                sid: expected.sid.clone(),
                session: 8,
            }
        ));
    }

    #[test]
    fn editor_pipe_outlives_serve_and_delivers_paths() {
        let identity = current_identity().expect("read current process identity");
        let name = format!(
            r"\\.\pipe\Jiffshot.Editor.Test.{}.{}",
            std::process::id(),
            identity.session
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        serve_on(&name, current_identity().unwrap(), move |path| {
            let _ = sender.send(path);
        })
        .expect("start editor pipe server");

        let path = Path::new(r"C:\captures\shot.png");
        let delivered = (0..50).any(|_| {
            send_on(&name, &identity, path) || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                false
            }
        });
        assert!(
            delivered,
            "the pipe must accept clients after serve returns"
        );
        assert_eq!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("handler receives the path"),
            path
        );
    }

    #[test]
    fn editor_pipe_descriptor_grants_access_only_to_the_current_user() {
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

        let identity = current_identity().expect("read current process identity");
        let descriptor = OwnerOnly::new(&identity.sid).expect("build owner-only descriptor");
        let mut present: BOOL = 0;
        let mut defaulted: BOOL = 0;
        let mut acl = std::ptr::null_mut();
        // SAFETY: descriptor owns a valid security descriptor and all output pointers live.
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
            },
            0
        );
        assert_ne!(present, 0, "the descriptor must contain a DACL");
        assert!(!acl.is_null(), "the DACL must not be null");

        let mut size = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: `acl` came from the live descriptor and `size` is writable.
        assert_ne!(
            unsafe {
                GetAclInformation(
                    acl,
                    (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            },
            0
        );
        assert_eq!(size.AceCount, 1, "the DACL should contain one access rule");

        let mut ace = std::ptr::null_mut();
        // SAFETY: the ACL reports exactly one ACE and `ace` is writable.
        assert_ne!(unsafe { GetAce(acl, 0, &mut ace) }, 0);
        assert!(!ace.is_null(), "the sole access rule must exist");
        // SAFETY: the SDDL creates one allowed ACE, as checked by its type.
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        assert_eq!(allowed.Header.AceType, ACCESS_ALLOWED_ACE_TYPE);
        assert_eq!(allowed.Mask & GENERIC_ALL, GENERIC_ALL);

        let sid = wide(&identity.sid);
        let mut expected_sid = std::ptr::null_mut();
        // SAFETY: `sid` is NUL-terminated and the allocated SID is freed below.
        assert_ne!(
            unsafe { ConvertStringSidToSidW(sid.as_ptr(), &mut expected_sid) },
            0
        );
        // SAFETY: `SidStart` begins the SID in a valid ACCESS_ALLOWED_ACE.
        let allowed_sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast();
        // SAFETY: both SID pointers refer to valid SIDs.
        let matches_current_user = unsafe { EqualSid(allowed_sid, expected_sid) } != 0;
        // SAFETY: allocated by ConvertStringSidToSidW.
        unsafe { LocalFree(expected_sid) };
        assert!(
            matches_current_user,
            "the allowed SID must be the current user"
        );
    }
}
