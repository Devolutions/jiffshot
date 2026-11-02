//! Jiffshot's local control protocol: how `jiffshot-cli` (or any other
//! client) drives the running Jiffshot.
//!
//! The running Jiffshot (the tray instance) owns capture and recording
//! state and listens on a per-user local transport. Windows uses a named
//! pipe scoped to the signed-in user and session; macOS uses a private Unix
//! socket and verifies the peer user on both ends.
//!
//! Each connection carries one request and one response. Both are framed as
//! a little-endian `u32` byte length followed by that much UTF-8 JSON (at
//! most [`MAX_FRAME`] bytes). A request names a command and its arguments; a
//! response is either a result or a typed [`Error`]. [`PROTOCOL`] is the
//! major version: a server refuses requests of another major version with
//! [`ErrorCode::Unsupported`], so a CLI and a running Jiffshot of different
//! builds fail clearly instead of misreading each other.

#![cfg(any(windows, target_os = "macos"))]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{
    call, connect, pipe_name, send, serve, serve_owned, serve_owned_with_completion, Completion,
    Pipe, Server,
};

/// The protocol's major version.
pub const PROTOCOL: u32 = 1;

/// The largest frame either side accepts.
pub const MAX_FRAME: usize = 16 << 20;

/// A command for the running Jiffshot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub protocol: u32,
    /// Chosen by the client; echoed in the response and the log.
    pub id: String,
    /// `status`, `screenshot`, `record.start`, … (see `docs/cli.md`).
    pub command: String,
    #[serde(default)]
    pub args: Value,
}

impl Request {
    pub fn new(command: &str, args: Value) -> Request {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let pid = std::process::id();
        let id = format!("{pid}-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        Request {
            protocol: PROTOCOL,
            id,
            command: command.to_owned(),
            args,
        }
    }
}

/// The running Jiffshot's answer to one [`Request`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub protocol: u32,
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl Response {
    pub fn ok(id: &str, result: Value) -> Response {
        Response {
            protocol: PROTOCOL,
            id: id.to_owned(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: &str, error: Error) -> Response {
        Response {
            protocol: PROTOCOL,
            id: id.to_owned(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    /// The result, or the error it reports.
    pub fn into_result(self) -> Result<Value, Error> {
        match (self.ok, self.error) {
            (true, _) => Ok(self.result.unwrap_or(Value::Null)),
            (false, Some(error)) => Err(error),
            (false, None) => Err(Error::new(
                ErrorCode::Internal,
                "the response reports a failure without an error",
            )),
        }
    }
}

/// Why a request failed; `code` is stable, `message` is for people.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Error {
        Error {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Bad arguments or an unknown command.
    InvalidRequest,
    /// The window, monitor, text or element asked for is not there.
    NotFound,
    /// The request clashes with what is going on (a recording is already
    /// running, there is none to stop, …).
    Conflict,
    /// Jiffshot is not running or could not be started.
    Unavailable,
    /// A bounded wait ran out.
    Timeout,
    /// The running Jiffshot speaks another protocol version.
    Unsupported,
    /// The pipe broke or carried something unreadable.
    Transport,
    /// Anything else that went wrong while doing the work.
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Transport => "transport",
            ErrorCode::Internal => "internal",
        }
    }

    /// The CLI's exit code for a failure of this kind: 2 for bad usage, 3
    /// when Jiffshot cannot be reached, 4 for timeouts and 1 otherwise.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::InvalidRequest => 2,
            ErrorCode::Unavailable | ErrorCode::Transport | ErrorCode::Unsupported => 3,
            ErrorCode::Timeout => 4,
            _ => 1,
        }
    }
}

/// Writes one frame: the length, then the bytes.
pub fn write_frame(mut to: impl Write, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut buffer = Vec::with_capacity(4 + bytes.len());
    buffer.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buffer.extend_from_slice(bytes);
    to.write_all(&buffer)?;
    to.flush()
}

/// Reads one frame, refusing lengths over [`MAX_FRAME`] before allocating.
pub fn read_frame(mut from: impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    from.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut bytes = vec![0u8; len];
    from.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn response_for(bytes: &[u8], handler: impl FnOnce(Request) -> Response) -> Response {
    match serde_json::from_slice::<Request>(bytes) {
        Err(error) => Response::err("", Error::new(ErrorCode::InvalidRequest, format!("unreadable request: {error}"))),
        Ok(request) if request.protocol != PROTOCOL => Response::err(
            &request.id,
            Error::new(
                ErrorCode::Unsupported,
                format!("this Jiffshot speaks control protocol {PROTOCOL}, the request uses {}; use matching versions", request.protocol),
            ),
        ),
        Ok(request) => {
            let id = request.id.clone();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(request)))
                .unwrap_or_else(|_| Response::err(&id, Error::new(ErrorCode::Internal, "the command crashed")))
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_BROKEN_PIPE, ERROR_PIPE_BUSY,
        ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
        WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
    };

    pub(super) fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A pipe handle, closed on drop.
    pub struct Pipe {
        pub(super) handle: HANDLE,
        pub(super) server: bool,
    }

    // SAFETY: a pipe handle may be used from any thread.
    unsafe impl Send for Pipe {}

    impl Pipe {
        /// The process on the other end of a client connection.
        pub fn server_process_id(&self) -> Option<u32> {
            let mut pid = 0;
            // SAFETY: a live pipe handle and an out-parameter.
            (unsafe { GetNamedPipeServerProcessId(self.handle, &mut pid) } != 0).then_some(pid)
        }
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut read = 0;
            let len = buf.len().min(u32::MAX as usize) as u32;
            // SAFETY: the buffer and its length go together; synchronous I/O.
            if unsafe {
                ReadFile(
                    self.handle,
                    buf.as_mut_ptr(),
                    len,
                    &mut read,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                // SAFETY: plain Win32 call.
                return match unsafe { GetLastError() } {
                    ERROR_BROKEN_PIPE => Ok(0),
                    _ => Err(io::Error::last_os_error()),
                };
            }
            Ok(read as usize)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut written = 0;
            let len = buf.len().min(u32::MAX as usize) as u32;
            // SAFETY: as in `read`.
            if unsafe {
                WriteFile(
                    self.handle,
                    buf.as_ptr(),
                    len,
                    &mut written,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(written as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Pipe {
        fn drop(&mut self) {
            // SAFETY: the handle is owned; a server instance is flushed so the
            // client reads the whole response before it disconnects.
            unsafe {
                if self.server {
                    FlushFileBuffers(self.handle);
                    DisconnectNamedPipe(self.handle);
                }
                CloseHandle(self.handle);
            }
        }
    }

    /// The signed-in user's SID, as a string (`S-1-5-21-…`).
    pub fn user_sid() -> io::Result<String> {
        // SAFETY: the token handle is closed; the buffer is sized by the first
        // call and holds the TOKEN_USER the second call writes; the SID string
        // is freed with LocalFree after being copied.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut size = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
            let mut buffer = vec![0u64; (size as usize).div_ceil(8).max(1)];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size,
                &mut size,
            ) != 0;
            CloseHandle(token);
            if !ok {
                return Err(io::Error::last_os_error());
            }
            let user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Ok(sid)
        }
    }

    /// The Windows session this process runs in.
    pub fn session_id() -> u32 {
        let mut session = 0;
        // SAFETY: plain Win32 call with a valid out pointer.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
        session
    }

    /// The control pipe for this user and session.
    pub fn pipe_name() -> io::Result<String> {
        Ok(format!(
            r"\\.\pipe\Jiffshot.Control.{}.{}",
            user_sid()?,
            session_id()
        ))
    }

    /// A security descriptor granting the current user, and nobody else, full
    /// access (`D:P(A;;GA;;;<SID>)`).
    pub(super) struct OwnerOnly(*mut core::ffi::c_void);

    // SAFETY: the descriptor is immutable once built.
    unsafe impl Send for OwnerOnly {}
    unsafe impl Sync for OwnerOnly {}

    impl OwnerOnly {
        pub(super) fn new() -> io::Result<OwnerOnly> {
            let sddl = wide(&format!("D:P(A;;GA;;;{})", user_sid()?));
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: NUL-terminated SDDL; the descriptor is freed on drop.
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
            Ok(OwnerOnly(descriptor))
        }
    }

    impl Drop for OwnerOnly {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(self.0) };
        }
    }

    pub(super) fn create_instance(
        name: &[u16],
        security: &OwnerOnly,
        first: bool,
    ) -> io::Result<Pipe> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0,
            bInheritHandle: 0,
        };
        let first = if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
        // SAFETY: NUL-terminated name; the attributes outlive the call.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | first,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Pipe {
            handle,
            server: true,
        })
    }

    /// Answers one connection: reads its request, runs `handler` and writes the
    /// response. Requests of another protocol version, unreadable requests and
    /// handler panics get an error response.
    pub(super) fn answer(mut pipe: Pipe, handler: &(dyn Fn(Request) -> Response + Send + Sync)) {
        let Ok(bytes) = read_frame(&mut pipe) else {
            return;
        };
        let response = response_for(&bytes, handler);
        if let Ok(bytes) = serde_json::to_vec(&response) {
            let _ = write_frame(&mut pipe, &bytes);
        }
    }

    /// Listens on the control pipe on a background thread, answering each
    /// connection on a thread of its own. Fails when another process already
    /// owns the pipe.
    pub fn serve(handler: impl Fn(Request) -> Response + Send + Sync + 'static) -> io::Result<()> {
        let name = wide(&pipe_name()?);
        let security = OwnerOnly::new()?;
        // The first instance claims the name, so a second Jiffshot (or anything
        // else) cannot serve it.
        let first = create_instance(&name, &security, true)?;
        let handler: Arc<dyn Fn(Request) -> Response + Send + Sync> = Arc::new(handler);
        std::thread::Builder::new()
            .name("control-pipe".into())
            .spawn(move || {
                let mut next = Some(first);
                loop {
                    let pipe = match next.take() {
                        Some(pipe) => pipe,
                        None => match create_instance(&name, &security, false) {
                            Ok(pipe) => pipe,
                            Err(_) => {
                                std::thread::sleep(Duration::from_millis(250));
                                continue;
                            }
                        },
                    };
                    // SAFETY: a live server instance; blocks until a client connects.
                    let connected = unsafe {
                        ConnectNamedPipe(pipe.handle, std::ptr::null_mut()) != 0
                            || GetLastError() == ERROR_PIPE_CONNECTED
                    };
                    if !connected {
                        continue;
                    }
                    let handler = handler.clone();
                    let _ = std::thread::Builder::new()
                        .name("control-request".into())
                        .spawn(move || answer(pipe, &*handler));
                }
            })?;
        Ok(())
    }

    /// Connects to the running Jiffshot, waiting up to `timeout` while every
    /// pipe instance is busy. [`ErrorCode::Unavailable`] when none is running.
    pub fn connect(timeout: Duration) -> Result<Pipe, Error> {
        let name = pipe_name().map_err(|e| {
            Error::new(
                ErrorCode::Internal,
                format!("cannot name the control pipe: {e}"),
            )
        })?;
        let name = wide(&name);
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: NUL-terminated name; the handle is owned by the Pipe. The
            // server may only identify, not impersonate, this client.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                let pipe = Pipe {
                    handle,
                    server: false,
                };
                // The server must run in this session (the pipe's DACL already
                // limits it to this user).
                if let Some(pid) = pipe.server_process_id() {
                    let mut session = u32::MAX;
                    // SAFETY: plain Win32 call with a valid out pointer.
                    unsafe { ProcessIdToSessionId(pid, &mut session) };
                    if session != session_id() {
                        return Err(Error::new(
                            ErrorCode::Unavailable,
                            "the control pipe belongs to another session",
                        ));
                    }
                }
                return Ok(pipe);
            }
            // SAFETY: plain Win32 call.
            let error = unsafe { GetLastError() };
            let left = deadline.saturating_duration_since(Instant::now());
            if error != ERROR_PIPE_BUSY {
                return Err(Error::new(
                    ErrorCode::Unavailable,
                    "Jiffshot is not running",
                ));
            }
            if left.is_zero() {
                return Err(Error::new(ErrorCode::Timeout, "Jiffshot is busy"));
            }
            // SAFETY: NUL-terminated name.
            unsafe { WaitNamedPipeW(name.as_ptr(), left.as_millis().clamp(1, 1000) as u32) };
        }
    }

    /// Sends `request` and waits up to `timeout` for its response.
    pub fn send(request: &Request, timeout: Duration) -> Result<Response, Error> {
        let started = Instant::now();
        let mut pipe = connect(timeout.min(Duration::from_secs(10)))?;
        let bytes = serde_json::to_vec(request)
            .map_err(|e| Error::new(ErrorCode::Internal, e.to_string()))?;
        write_frame(&mut pipe, &bytes).map_err(|e| {
            Error::new(
                ErrorCode::Transport,
                format!("cannot send the request: {e}"),
            )
        })?;
        // Reads block, so the wait runs on a thread the deadline can abandon.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read_frame(&mut pipe));
        });
        let left = timeout.saturating_sub(started.elapsed());
        let bytes = match rx.recv_timeout(left) {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => {
                return Err(Error::new(
                    ErrorCode::Transport,
                    format!("no response from Jiffshot: {e}"),
                ))
            }
            Err(_) => {
                return Err(Error::new(
                    ErrorCode::Timeout,
                    format!(
                        "no response within {:.0} s (the command may still complete)",
                        timeout.as_secs_f64()
                    ),
                ))
            }
        };
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::Transport, format!("unreadable response: {e}")))
    }

    /// Runs `command` on the running Jiffshot: its result, or the error.
    pub fn call(command: &str, args: Value, timeout: Duration) -> Result<Value, Error> {
        send(&Request::new(command, args), timeout)?.into_result()
    }
}

#[cfg(windows)]
pub use windows::{call, connect, pipe_name, send, serve, session_id, user_sid, Pipe};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, b"{\"a\":1}").unwrap();
        assert_eq!(&buffer[..4], &7u32.to_le_bytes());
        assert_eq!(read_frame(&buffer[..]).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn oversized_frames_are_refused_before_allocating() {
        let header = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert_eq!(
            read_frame(&header[..]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn requests_and_responses_round_trip() {
        let request = Request::new("screenshot", json!({ "screen": "primary" }));
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);
        let error = Response::err("1", Error::new(ErrorCode::NotFound, "no window"));
        let text = serde_json::to_string(&error).unwrap();
        assert!(text.contains("\"not_found\""));
        assert_eq!(
            serde_json::from_str::<Response>(&text)
                .unwrap()
                .into_result()
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(Response::ok("1", json!(3)).into_result().unwrap(), json!(3));
    }

    #[test]
    fn exit_codes_follow_the_error_kind() {
        assert_eq!(ErrorCode::InvalidRequest.exit_code(), 2);
        assert_eq!(ErrorCode::Unavailable.exit_code(), 3);
        assert_eq!(ErrorCode::Timeout.exit_code(), 4);
        assert_eq!(ErrorCode::NotFound.exit_code(), 1);
    }

    #[test]
    #[cfg(windows)]
    fn the_pipe_is_per_user_and_session() {
        let name = pipe_name().unwrap();
        assert!(
            name.starts_with(r"\\.\pipe\Jiffshot.Control.S-1-"),
            "{name}"
        );
        assert!(name.ends_with(&format!(".{}", session_id())));
    }

    #[test]
    #[cfg(windows)]
    fn a_served_pipe_answers_and_refuses_other_versions() {
        use super::windows::{answer, create_instance, wide, OwnerOnly};
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
        use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, WaitNamedPipeW};
        // A private pipe name keeps the test away from a running Jiffshot.
        let name = wide(&format!(
            r"\\.\pipe\Jiffshot.ControlTest.{}",
            std::process::id()
        ));
        let security = OwnerOnly::new().unwrap();
        let server = create_instance(&name, &security, true).unwrap();
        let worker = std::thread::spawn(move || {
            // SAFETY: a live server instance.
            unsafe { ConnectNamedPipe(server.handle, std::ptr::null_mut()) };
            answer(server, &|r: Request| {
                Response::ok(&r.id, json!({ "echo": r.command }))
            });
        });
        // SAFETY: as in `connect`.
        let handle = unsafe {
            WaitNamedPipeW(name.as_ptr(), 5000);
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        let mut pipe = Pipe {
            handle,
            server: false,
        };
        let mut request = Request::new("status", Value::Null);
        request.protocol = PROTOCOL + 1;
        write_frame(&mut pipe, &serde_json::to_vec(&request).unwrap()).unwrap();
        let response: Response = serde_json::from_slice(&read_frame(&mut pipe).unwrap()).unwrap();
        assert_eq!(
            response.into_result().unwrap_err().code,
            ErrorCode::Unsupported
        );
        worker.join().unwrap();
    }
}
