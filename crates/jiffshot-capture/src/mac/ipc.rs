//! Per-user Unix-socket hand-off for the running editor and capture process.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_PATH_BYTES: usize = 64 * 1024;
const SOCKET_PATH_LIMIT: usize = 104;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn secure_directory(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: this is a plain query of the current process's effective user.
    if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("IPC directory is not owned by this user: {}", path.display())));
    }
    if metadata.mode() & 0o777 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ipc_dir_for(base: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(base)?;
    let base = fs::canonicalize(base)?;
    let metadata = fs::metadata(&base)?;
    // SAFETY: this is a plain query of the current process's effective user.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("Jiffshot's local directory is not private to this user: {}", base.display())));
    }
    let candidate = base.join("ipc");
    let longest_socket = candidate.join("JiffshotCaptureTray.sock");
    let directory = if longest_socket.as_os_str().as_bytes().len() < SOCKET_PATH_LIMIT {
        candidate
    } else {
        // Darwin's sockaddr_un has a short path limit. Use a stable, private
        // per-user directory when the configured cache path is too long.
        let mut hash = 0xcbf29ce484222325u64;
        for byte in base.as_os_str().as_bytes() {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
        PathBuf::from(format!("/tmp/jiffshot-ipc-{uid}-{hash:016x}"))
    };
    secure_directory(&directory)?;
    Ok(directory)
}

/// A private IPC directory, stable for this user and local-data directory.
pub fn private_dir() -> io::Result<PathBuf> {
    ipc_dir_for(&crate::paths::local_dir())
}

fn lock_at(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: this is a plain query of the current process's effective user.
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("IPC lock is not owned by this user: {}", path.display())));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    // SAFETY: the live descriptor is held for as long as the returned file.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(Some(file))
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock { Ok(None) } else { Err(error) }
    }
}

/// Acquires an exclusive process-lifetime lock; `None` means it is held.
pub fn try_lock(name: &str) -> io::Result<Option<File>> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid IPC lock name"));
    }
    lock_at(&private_dir()?.join(format!("{name}.lock")))
}

/// Removes only an owned socket after the caller has acquired its lock.
pub fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // SAFETY: this is a plain query of the current process's effective user.
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("refusing to replace an unowned or non-socket IPC path: {}", path.display())));
    }
    fs::remove_file(path)
}

/// Verifies that a connected Unix-socket peer belongs to the current user.
pub fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: the descriptor is live and both output pointers are writable.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this is a plain query of the current process's effective user.
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "IPC peer belongs to another user"));
    }
    Ok(())
}

/// Waits for a listener to appear, then verifies its user before sending.
/// Once connected, callers must not retry a possibly delivered message.
pub fn connect_when_ready(path: &Path, wait: Duration) -> io::Result<UnixStream> {
    let started = Instant::now();
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => {
                verify_peer(&stream)?;
                return Ok(stream);
            }
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::Interrupted)
                && started.elapsed() < wait =>
            {
                std::thread::sleep(Duration::from_millis(50).min(wait.saturating_sub(started.elapsed())));
            }
            Err(error) => return Err(error),
        }
    }
}

fn socket_path() -> io::Result<PathBuf> {
    Ok(private_dir()?.join("editor.sock"))
}

fn send_at(socket: &Path, path: &Path, wait: Duration) -> io::Result<()> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_PATH_BYTES || bytes.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the file path is empty, invalid, or too long to forward"));
    }
    let mut stream = connect_when_ready(socket, wait)?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    let delivery_error = |error: io::Error| {
        io::Error::new(error.kind(), format!("the file may have been forwarded, but delivery could not be confirmed: {error}"))
    };
    stream.write_all(&(bytes.len() as u32).to_le_bytes()).map_err(delivery_error)?;
    stream.write_all(bytes).map_err(delivery_error)?;
    stream.shutdown(std::net::Shutdown::Write).map_err(delivery_error)?;
    let mut ack = [0];
    stream.read_exact(&mut ack).map_err(|error| {
        io::Error::new(error.kind(), format!("the file may have been forwarded, but the running editor did not acknowledge it: {error}"))
    })?;
    if ack != [1] {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the running editor rejected the file"));
    }
    Ok(())
}

/// Sends a path to an already-listening editor.
pub fn send_to_editor(path: &Path) -> bool {
    socket_path().and_then(|socket| send_at(&socket, path, Duration::ZERO)).is_ok()
}

/// Waits for an already-claimed editor to start its listener, then forwards
/// the path exactly once.
pub fn send_to_editor_when_ready(path: &Path, wait: Duration) -> io::Result<()> {
    send_at(&socket_path()?, path, wait)
}

fn receive(mut stream: UnixStream, handler: &impl Fn(PathBuf) -> io::Result<()>) -> io::Result<()> {
    verify_peer(&stream)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_PATH_BYTES {
        stream.write_all(&[0])?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid or oversized editor IPC message"));
    }
    let mut buf = vec![0; length];
    stream.read_exact(&mut buf)?;
    let mut extra = [0];
    if buf.contains(&0) || stream.read(&mut extra)? != 0 {
        stream.write_all(&[0])?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid or oversized editor IPC message"));
    }
    if let Err(error) = handler(PathBuf::from(std::ffi::OsString::from_vec(buf))) {
        stream.write_all(&[0])?;
        return Err(error);
    }
    stream.write_all(&[1])
}

/// Listens for paths on a background thread. Fails if another editor owns
/// the lock, and only removes stale sockets while holding that lock.
/// A successful handler result means that the path was queued for the UI.
pub fn serve(handler: impl Fn(PathBuf) -> io::Result<()> + Send + 'static) -> io::Result<()> {
    let guard = try_lock("editor")?.ok_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "another Jiffshot editor is running"))?;
    let path = socket_path()?;
    remove_stale_socket(&path)?;
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new().name("editor-ipc".into()).spawn(move || {
        let _guard = guard;
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    if let Err(error) = receive(stream, &handler) {
                        eprintln!("editor IPC: {error}");
                    }
                }
                Err(error) => eprintln!("editor IPC listener: {error}"),
            }
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn test_directory() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = PathBuf::from(format!("/tmp/jiffshot-ipc-test-{}-{id}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        directory
    }

    #[test]
    fn locks_are_exclusive_and_stale_sockets_are_safely_removed() {
        let dir = test_directory();
        let lock = dir.join("editor.lock");
        let guard = lock_at(&lock).unwrap().unwrap();
        assert!(lock_at(&lock).unwrap().is_none());
        drop(guard);
        assert!(lock_at(&lock).unwrap().is_some());

        let socket = dir.join("editor.sock");
        drop(UnixListener::bind(&socket).unwrap());
        remove_stale_socket(&socket).unwrap();
        assert!(!socket.exists());
        fs::write(&socket, b"preserve").unwrap();
        assert_eq!(remove_stale_socket(&socket).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&socket).unwrap(), b"preserve");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn forwards_paths_once_and_rejects_oversized_messages() {
        let dir = test_directory();
        let socket = dir.join("editor.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (tx, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let result = receive(stream, &|path| {
                    tx.send(path).unwrap();
                    Ok(())
                });
                if let Err(error) = result {
                    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                }
            }
        });
        let original = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/a \xff.png".to_vec()));
        send_at(&socket, &original, Duration::ZERO).unwrap();
        assert_eq!(rx.recv_timeout(IO_TIMEOUT).unwrap(), original);
        let mut oversized = UnixStream::connect(&socket).unwrap();
        oversized.write_all(&((MAX_PATH_BYTES + 1) as u32).to_le_bytes()).unwrap();
        let mut ack = [1];
        oversized.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [0]);
        server.join().unwrap();
        assert!(rx.try_recv().is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn waits_for_a_delayed_listener_without_replaying_the_request() {
        let dir = test_directory();
        let socket = dir.join("editor.sock");
        let server_path = socket.clone();
        let (tx, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            let listener = UnixListener::bind(server_path).unwrap();
            let (stream, _) = listener.accept().unwrap();
            receive(stream, &|path| {
                tx.send(path).unwrap();
                Ok(())
            }).unwrap();
        });
        send_at(&socket, Path::new("/tmp/a b.png"), Duration::from_secs(2)).unwrap();
        assert_eq!(rx.recv_timeout(IO_TIMEOUT).unwrap(), PathBuf::from("/tmp/a b.png"));
        server.join().unwrap();
        assert!(rx.try_recv().is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn incomplete_path_is_not_forwarded() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            receive(server, &|path| {
                tx.send(path).unwrap();
                Ok(())
            })
        });
        client.write_all(&12u32.to_le_bytes()).unwrap();
        client.write_all(b"/tmp/a").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn missing_acknowledgement_is_reported_without_replaying() {
        let dir = test_directory();
        let socket = dir.join("editor.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0; 4];
            stream.read_exact(&mut length).unwrap();
            let mut bytes = vec![0; u32::from_le_bytes(length) as usize];
            stream.read_exact(&mut bytes).unwrap();
            let mut extra = [0];
            assert_eq!(stream.read(&mut extra).unwrap(), 0);
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            (listener, bytes)
        });
        let error = send_at(&socket, Path::new("/tmp/test.png"), Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("may have been forwarded"));
        let (listener, bytes) = server.join().unwrap();
        assert_eq!(bytes, b"/tmp/test.png");
        assert_eq!(listener.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        drop(listener);
        fs::remove_file(socket).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn editor_rejects_a_path_that_cannot_be_queued() {
        let dir = test_directory();
        let socket = dir.join("editor.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            receive(stream, &|_| Err(io::Error::other("dispatcher stopped"))).unwrap_err()
        });
        let error = send_at(&socket, Path::new("/tmp/test.png"), Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("rejected"));
        assert!(server.join().unwrap().to_string().contains("dispatcher stopped"));
        fs::remove_file(socket).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn long_local_paths_use_a_private_short_socket_directory() {
        let dir = test_directory();
        let base = dir.join("a".repeat(90));
        fs::create_dir(&base).unwrap();
        let private = ipc_dir_for(&base).unwrap();
        assert!(private.join("JiffshotCaptureTray.sock").as_os_str().as_bytes().len() < SOCKET_PATH_LIMIT);
        assert_eq!(fs::metadata(&private).unwrap().mode() & 0o777, 0o700);
        assert_eq!(ipc_dir_for(&base).unwrap(), private);
        fs::remove_dir(private).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
