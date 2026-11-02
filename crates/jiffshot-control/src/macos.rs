use super::{
    read_frame, response_for, write_frame, Error, ErrorCode, Request, Response, Value, PROTOCOL,
};
use jiffshot_capture::ipc;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const REQUEST_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected local control socket.
pub type Pipe = UnixStream;
pub type Completion = Box<dyn FnOnce() + Send>;
type Handler = dyn Fn(Request) -> (Response, Option<Completion>) + Send + Sync;

fn socket_path() -> io::Result<PathBuf> {
    Ok(ipc::private_dir()?.join("control.sock"))
}

/// The control socket for this user and local-data directory.
pub fn pipe_name() -> io::Result<String> {
    Ok(socket_path()?.display().to_string())
}

fn answer(mut stream: UnixStream, handler: &Handler) -> io::Result<()> {
    ipc::verify_peer(&stream)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(REQUEST_IO_TIMEOUT))?;
    let bytes = read_frame(&mut stream)?;
    let mut completion = None;
    let response = response_for(&bytes, |request| {
        let (response, after) = handler(request);
        completion = after;
        response
    });
    let result = serde_json::to_vec(&response)
        .map_err(io::Error::other)
        .and_then(|bytes| {
            stream.set_write_timeout(Some(REQUEST_IO_TIMEOUT))?;
            write_frame(&mut stream, &bytes)
        });
    if let Some(completion) = completion {
        completion();
    }
    result
}

/// A control listener that drains in-flight requests before releasing its
/// socket and exclusive lock.
pub struct Server {
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl Server {
    fn finish(&mut self) -> io::Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        self.stopping.store(true, Ordering::Release);
        thread
            .join()
            .map_err(|_| io::Error::other("the control listener panicked"))?
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        self.finish()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("control socket shutdown: {error}");
        }
    }
}

/// Listens on a private per-user socket until the returned server is stopped.
/// Only the lock holder may replace an existing socket.
pub fn serve_owned(
    handler: impl Fn(Request) -> Response + Send + Sync + 'static,
) -> io::Result<Server> {
    serve_owned_with_completion(move |request| (handler(request), None))
}

/// Like [`serve_owned`], but a handler may return work to run after its
/// response has been written or the write has failed.
pub fn serve_owned_with_completion(
    handler: impl Fn(Request) -> (Response, Option<Completion>) + Send + Sync + 'static,
) -> io::Result<Server> {
    let guard = ipc::try_lock("control")?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrInUse,
            "another Jiffshot control server is running",
        )
    })?;
    let path = socket_path()?;
    ipc::remove_stale_socket(&path)?;
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let handler: Arc<Handler> = Arc::new(handler);
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let thread = std::thread::Builder::new()
        .name("control-socket".into())
        .spawn(move || {
            let _guard = guard;
            let mut requests: Vec<JoinHandle<()>> = Vec::new();
            let mut request_panicked = false;
            let result = loop {
                if stop.load(Ordering::Acquire) {
                    break Ok(());
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = handler.clone();
                        match std::thread::Builder::new()
                            .name("control-request".into())
                            .spawn(move || {
                                if let Err(error) = answer(stream, &*handler) {
                                    eprintln!("control request: {error}");
                                }
                            }) {
                            Ok(thread) => requests.push(thread),
                            Err(error) => eprintln!("control request thread: {error}"),
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        let mut descriptor = libc::pollfd {
                            fd: listener.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        // SAFETY: the live listener and writable pollfd outlive this call.
                        if unsafe { libc::poll(&mut descriptor, 1, 100) } < 0 {
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::Interrupted {
                                break Err(error);
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => break Err(error),
                }
                let mut index = 0;
                while index < requests.len() {
                    if requests[index].is_finished() {
                        if requests.swap_remove(index).join().is_err() {
                            eprintln!("control request thread panicked");
                            request_panicked = true;
                        }
                    } else {
                        index += 1;
                    }
                }
            };
            drop(listener);
            for request in requests {
                if request.join().is_err() {
                    eprintln!("control request thread panicked");
                    request_panicked = true;
                }
            }
            let cleanup = ipc::remove_stale_socket(&path);
            match (result, cleanup, request_panicked) {
                (Err(error), Err(cleanup), _) => Err(io::Error::other(format!(
                    "{error}; could not remove the control socket: {cleanup}"
                ))),
                (Err(error), _, _) | (_, Err(error), _) => Err(error),
                (_, _, true) => Err(io::Error::other("a control request thread panicked")),
                _ => Ok(()),
            }
        })?;
    Ok(Server {
        stopping,
        thread: Some(thread),
    })
}

/// Listens for the lifetime of the process.
pub fn serve(handler: impl Fn(Request) -> Response + Send + Sync + 'static) -> io::Result<()> {
    let mut server = serve_owned(handler)?;
    drop(server.thread.take());
    Ok(())
}

fn timeout_error(timeout: Duration) -> Error {
    Error::new(
        ErrorCode::Timeout,
        format!(
            "no response within {:.0} s (the command may still complete)",
            timeout.as_secs_f64()
        ),
    )
}

fn remaining(started: Instant, timeout: Duration) -> io::Result<Duration> {
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "control request deadline elapsed",
        ))
    } else {
        Ok(remaining)
    }
}

fn io_error(action: &str, error: io::Error, timeout: Duration) -> Error {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => timeout_error(timeout),
        _ => Error::new(ErrorCode::Transport, format!("{action}: {error}")),
    }
}

struct DeadlineStream {
    stream: UnixStream,
    started: Instant,
    timeout: Duration,
}

impl DeadlineStream {
    fn wait(&self, events: libc::c_short) -> io::Result<()> {
        loop {
            let left = remaining(self.started, self.timeout)?;
            let millis = left.as_nanos().saturating_add(999_999) / 1_000_000;
            let millis = millis.min(libc::c_int::MAX as u128) as libc::c_int;
            let mut descriptor = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: the descriptor refers to the live stream, and poll
            // receives a valid pointer to one writable pollfd.
            let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
            if result > 0 {
                return Ok(());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            self.wait(libc::POLLIN)?;
            match self.stream.read(buffer) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                result => return result,
            }
        }
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            self.wait(libc::POLLOUT)?;
            match self.stream.write(buffer) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn connect_at(path: &std::path::Path, timeout: Duration) -> io::Result<UnixStream> {
    let started = Instant::now();
    remaining(started, timeout)?;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: zero is a valid initial value for sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid or too long control socket path",
        ));
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    address.sun_len = length as u8;
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    // SAFETY: the source and destination do not overlap and the path was
    // checked to fit, including the zeroed trailing NUL.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            address.sun_path.as_mut_ptr().cast::<u8>(),
            bytes.len(),
        )
    };
    // SAFETY: socket returns a new file descriptor or -1 on failure.
    let descriptor = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the new descriptor is uniquely owned by the resulting stream.
    let stream = unsafe { UnixStream::from_raw_fd(descriptor) };
    // SAFETY: fcntl operates on the live descriptor and does not retain it.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let no_sigpipe: libc::c_int = 1;
    // SAFETY: the option points to a c_int for the duration of the call.
    if unsafe {
        libc::setsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&no_sigpipe as *const libc::c_int).cast(),
            std::mem::size_of_val(&no_sigpipe) as libc::socklen_t,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    stream.set_nonblocking(true)?;
    // SAFETY: address is initialized and length covers the path and NUL.
    let result = unsafe {
        libc::connect(
            descriptor,
            (&address as *const libc::sockaddr_un).cast(),
            length as libc::socklen_t,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        let pending = DeadlineStream {
            stream,
            started,
            timeout,
        };
        pending.wait(libc::POLLOUT)?;
        let mut socket_error: libc::c_int = 0;
        let mut option_length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
        // SAFETY: the output buffer and length describe a writable c_int.
        if unsafe {
            libc::getsockopt(
                descriptor,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut socket_error as *mut libc::c_int).cast(),
                &mut option_length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if socket_error != 0 {
            return Err(io::Error::from_raw_os_error(socket_error));
        }
        pending.stream.set_nonblocking(false)?;
        ipc::verify_peer(&pending.stream)?;
        return Ok(pending.stream);
    }
    stream.set_nonblocking(false)?;
    ipc::verify_peer(&stream)?;
    Ok(stream)
}

/// Connects to the running Jiffshot and verifies that the peer is this user.
pub fn connect(timeout: Duration) -> Result<Pipe, Error> {
    let path = socket_path().map_err(|error| {
        Error::new(
            ErrorCode::Internal,
            format!("cannot name the control socket: {error}"),
        )
    })?;
    connect_at(&path, timeout).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            Error::new(ErrorCode::Unavailable, "Jiffshot is not running")
        }
        io::ErrorKind::TimedOut => {
            Error::new(ErrorCode::Timeout, "timed out connecting to Jiffshot")
        }
        _ => Error::new(
            ErrorCode::Transport,
            format!("cannot connect to the control socket: {error}"),
        ),
    })
}

fn send_on(
    stream: UnixStream,
    request: &Request,
    timeout: Duration,
    started: Instant,
) -> Result<Response, Error> {
    stream
        .set_nonblocking(true)
        .map_err(|error| io_error("cannot set the control socket nonblocking", error, timeout))?;
    let mut stream = DeadlineStream {
        stream,
        started,
        timeout,
    };
    let bytes = serde_json::to_vec(request)
        .map_err(|error| Error::new(ErrorCode::Internal, error.to_string()))?;
    write_frame(&mut stream, &bytes)
        .map_err(|error| io_error("cannot send the request", error, timeout))?;
    let bytes = read_frame(&mut stream)
        .map_err(|error| io_error("no response from Jiffshot", error, timeout))?;
    let response: Response = serde_json::from_slice(&bytes).map_err(|error| {
        Error::new(
            ErrorCode::Transport,
            format!("unreadable response: {error}"),
        )
    })?;
    if response.id != request.id {
        return Err(Error::new(
            ErrorCode::Transport,
            "the control response has a different request ID",
        ));
    }
    if response.protocol != PROTOCOL {
        return Err(Error::new(
            ErrorCode::Unsupported,
            format!("Jiffshot uses control protocol {}, this client uses {PROTOCOL}; use matching versions", response.protocol),
        ));
    }
    Ok(response)
}

/// Sends one request, without retrying a request that may have been delivered.
pub fn send(request: &Request, timeout: Duration) -> Result<Response, Error> {
    let started = Instant::now();
    let stream = connect(timeout)?;
    send_on(stream, request, timeout, started)
}

/// Runs `command` on the running Jiffshot.
pub fn call(command: &str, args: Value, timeout: Duration) -> Result<Value, Error> {
    send(&Request::new(command, args), timeout)?.into_result()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::DirBuilderExt;

    fn serve_once(
        handler: impl Fn(Request) -> Response + Send + Sync + 'static,
    ) -> (UnixStream, std::thread::JoinHandle<io::Result<()>>) {
        let (server, client) = UnixStream::pair().unwrap();
        let worker =
            std::thread::spawn(move || answer(server, &move |request| (handler(request), None)));
        (client, worker)
    }

    #[test]
    fn same_user_peers_exchange_framed_requests() {
        let (client, worker) =
            serve_once(|request| Response::ok(&request.id, json!({ "echo": request.command })));
        ipc::verify_peer(&client).unwrap();
        let request = Request::new("status", json!({}));
        let response = send_on(client, &request, Duration::from_secs(1), Instant::now()).unwrap();
        assert_eq!(response.into_result().unwrap(), json!({ "echo": "status" }));
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn accepted_nonblocking_socket_waits_for_a_delayed_request() {
        let (server, client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let (finished, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = answer(server, &|request| {
                (Response::ok(&request.id, Value::Null), None)
            });
            finished.send(result).unwrap();
        });
        assert!(matches!(
            wait.recv_timeout(Duration::from_millis(30)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        let request = Request::new("status", Value::Null);
        send_on(client, &request, Duration::from_secs(1), Instant::now()).unwrap();
        wait.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn another_protocol_is_refused_without_running_the_handler() {
        let (client, worker) =
            serve_once(|_| panic!("unsupported requests must not reach the handler"));
        let mut request = Request::new("status", Value::Null);
        request.protocol += 1;
        let response = send_on(client, &request, Duration::from_secs(1), Instant::now()).unwrap();
        assert_eq!(
            response.into_result().unwrap_err().code,
            ErrorCode::Unsupported
        );
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn mismatched_response_ids_are_refused() {
        let (client, worker) = serve_once(|_| Response::ok("another-request", Value::Null));
        let request = Request::new("status", Value::Null);
        assert_eq!(
            send_on(client, &request, Duration::from_secs(1), Instant::now())
                .unwrap_err()
                .code,
            ErrorCode::Transport
        );
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn another_server_protocol_is_reported_as_unsupported() {
        let (client, worker) = serve_once(|request| {
            let mut response = Response::ok(&request.id, Value::Null);
            response.protocol += 1;
            response
        });
        let request = Request::new("status", Value::Null);
        assert_eq!(
            send_on(client, &request, Duration::from_secs(1), Instant::now())
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn timeout_does_not_retry_a_delivered_request() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handled = calls.clone();
        let (client, worker) = serve_once(move |request| {
            handled.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(100));
            Response::ok(&request.id, Value::Null)
        });
        let request = Request::new("status", Value::Null);
        let error =
            send_on(client, &request, Duration::from_millis(20), Instant::now()).unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout);
        assert!(error.message.contains("may still complete"));
        assert!(worker.join().unwrap().is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn partial_response_reads_share_one_deadline() {
        let (mut server, client) = UnixStream::pair().unwrap();
        let request = Request::new("status", Value::Null);
        let response = serde_json::to_vec(&Response::ok(&request.id, Value::Null)).unwrap();
        let worker = std::thread::spawn(move || {
            read_frame(&mut server).unwrap();
            let mut frame = Vec::new();
            write_frame(&mut frame, &response).unwrap();
            for byte in frame {
                if server.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let started = Instant::now();
        let error = send_on(client, &request, Duration::from_millis(30), started).unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout);
        worker.join().unwrap();
    }

    #[test]
    fn completion_runs_after_the_response_is_written() {
        let (server, client) = UnixStream::pair().unwrap();
        let (received, wait) = std::sync::mpsc::channel();
        let wait = std::sync::Mutex::new(Some(wait));
        let worker = std::thread::spawn(move || {
            answer(server, &move |request| {
                let wait = wait.lock().unwrap().take().unwrap();
                let completion: Completion =
                    Box::new(move || wait.recv_timeout(Duration::from_secs(1)).unwrap());
                (Response::ok(&request.id, Value::Null), Some(completion))
            })
        });
        let request = Request::new("quit", Value::Null);
        send_on(client, &request, Duration::from_secs(1), Instant::now()).unwrap();
        received.send(()).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn completion_runs_even_if_the_reply_cannot_be_written() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let completed = Arc::new(AtomicBool::new(false));
        let observed = completed.clone();
        let (handled, wait_until_handled) = std::sync::mpsc::channel();
        let (release, wait_until_closed) = std::sync::mpsc::channel();
        let wait_until_closed = std::sync::Mutex::new(Some(wait_until_closed));
        let worker = std::thread::spawn(move || {
            answer(server, &move |request| {
                handled.send(()).unwrap();
                wait_until_closed
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .recv()
                    .unwrap();
                let observed = observed.clone();
                let completion: Completion =
                    Box::new(move || observed.store(true, Ordering::Release));
                (
                    Response::ok(&request.id, json!("x".repeat(1 << 20))),
                    Some(completion),
                )
            })
        });
        let request = Request::new("quit", Value::Null);
        write_frame(&mut client, &serde_json::to_vec(&request).unwrap()).unwrap();
        wait_until_handled
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        drop(client);
        release.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn connects_to_a_real_private_socket_and_checks_the_peer() {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let directory = PathBuf::from(format!(
            "/tmp/jiffshot-control-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            answer(stream, &|request| {
                (
                    Response::ok(&request.id, json!({ "echo": request.command })),
                    None,
                )
            })
            .unwrap();
        });
        let stream = connect_at(&path, Duration::from_secs(1)).unwrap();
        let request = Request::new("status", Value::Null);
        let response = send_on(stream, &request, Duration::from_secs(1), Instant::now()).unwrap();
        assert_eq!(response.into_result().unwrap(), json!({ "echo": "status" }));
        worker.join().unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn connection_rejects_bad_paths_and_zero_deadlines() {
        let too_long = PathBuf::from(format!("/tmp/{}", "a".repeat(104)));
        assert_eq!(
            connect_at(&too_long, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            connect_at(std::path::Path::new("/tmp/control.sock"), Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    #[ignore]
    fn isolated_server_child() {
        use std::os::unix::fs::MetadataExt;

        let (started, wait) = std::sync::mpsc::channel();
        let server = serve_owned(move |request| {
            if request.command == "slow" {
                started.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(100));
            }
            Response::ok(&request.id, json!({ "echo": request.command }))
        })
        .unwrap();
        assert_eq!(
            serve_owned(|request| Response::ok(&request.id, Value::Null))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::AddrInUse
        );
        assert_eq!(
            fs::metadata(socket_path().unwrap()).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            call("status", Value::Null, Duration::from_secs(1)).unwrap(),
            json!({ "echo": "status" })
        );
        let client = std::thread::spawn(|| call("slow", Value::Null, Duration::from_secs(1)));
        wait.recv_timeout(Duration::from_secs(1)).unwrap();
        server.shutdown().unwrap();
        assert_eq!(client.join().unwrap().unwrap(), json!({ "echo": "slow" }));
        assert!(!socket_path().unwrap().exists());
        assert!(ipc::try_lock("control").unwrap().is_some());
    }

    #[test]
    fn server_owns_a_private_socket_and_exclusive_lock() {
        let directory = PathBuf::from(format!(
            "/tmp/jiffshot-control-server-{}",
            std::process::id()
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "macos::tests::isolated_server_child",
                "--ignored",
                "--nocapture",
            ])
            .env("JIFFSHOT_LOCAL_DIR", &directory)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        fs::remove_file(directory.join("ipc/control.lock")).unwrap();
        fs::remove_dir(directory.join("ipc")).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
