//! Share destinations: other programs, mail, Office apps, folders, FTP,
//! PDF and the printer. Each takes the flattened image (written to a
//! temporary PNG named after the capture) unless it needs another format.

use crate::winutil;
use jiffshot_capture::share;
#[cfg(windows)]
use jiffshot_capture::wide;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::path::Path;

#[cfg(target_os = "macos")]
pub(crate) use jiffshot_capture::share::{open_with_in, temporary_export_path};

/// Destinations offered in the Share panel, before the user's programs.
#[cfg(windows)]
pub const BUILT_IN: &[&str] = &["Open with…", "Email", "Word", "PowerPoint", "Excel", "OneNote", "Paint", "Folder…", "FTP", "Print"];
#[cfg(target_os = "macos")]
pub const BUILT_IN: &[&str] = &["Open with…", "Email", "Word", "PowerPoint", "Excel", "OneNote", "Preview", "Folder…", "FTP", "Print"];

/// Opens `file` in a program found on the App Paths list (`winword.exe`…).
pub fn open_in(program: &str, file: &Path) -> Result<(), String> {
    share::open_in(winutil::main_hwnd(), program, file)
}

/// Asks which program to open the file with.
pub fn open_with(file: &Path) -> Result<bool, String> {
    share::open_with(winutil::main_hwnd(), file)
}

pub fn print(file: &Path) -> Result<bool, String> {
    share::print(winutil::main_hwnd(), file)
}

pub use share::to_folder;

/// Opens a new message in the default mail program with `file` attached.
pub fn email(file: &Path, subject: &str) -> Result<bool, String> {
    share::email(winutil::main_hwnd(), file, subject)
}

/// The FTP destination: `ftp://user@host[:port]/folder` in the settings;
/// the password comes from the platform credential store, never from a file.
pub fn ftp(url: &str, file: &Path) -> Result<String, String> {
    ftp_with_credential(url, file, credential)
}

fn ftp_argument(name: &str, value: &str) -> Result<(), String> {
    if value.contains(['\r', '\n']) {
        Err(format!("the FTP {name} contains a line break"))
    } else {
        Ok(())
    }
}

fn passive_address(reply: &str) -> Result<SocketAddrV4, String> {
    let address = reply.split(['(', ')']).nth(1).ok_or("bad PASV reply")?;
    let nums = address.split(',').map(|part| part.trim().parse::<u8>())
        .collect::<Result<Vec<_>, _>>().map_err(|_| "bad PASV reply")?;
    let [a, b, c, d, high, low] = nums.as_slice() else { return Err("bad PASV reply".into()) };
    let port = u16::from(*high) * 256 + u16::from(*low);
    if port == 0 {
        return Err("bad PASV reply".into());
    }
    Ok(SocketAddrV4::new(Ipv4Addr::new(*a, *b, *c, *d), port))
}

fn ftp_with_credential(
    url: &str,
    file: &Path,
    lookup: impl FnOnce(&str, &str) -> Result<Option<String>, String>,
) -> Result<String, String> {
    let rest = url.strip_prefix("ftp://").ok_or("the FTP address must start with ftp://")?;
    let (authority, folder) = rest.split_once('/').unwrap_or((rest, ""));
    let (user, hostport) = authority.rsplit_once('@').unwrap_or(("anonymous", authority));
    let host = if hostport.contains(':') { hostport.to_owned() } else { format!("{hostport}:21") };
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "capture.png".into());
    for (name, value) in [("username", user), ("folder", folder), ("filename", name.as_str())] {
        ftp_argument(name, value)?;
    }
    let mut source = std::fs::File::open(file).map_err(|e| format!("cannot open the FTP source: {e}"))?;
    if !source.metadata().map_err(|e| format!("cannot inspect the FTP source: {e}"))?.is_file() {
        return Err("the FTP source is not a regular file".into());
    }
    let password = match lookup("Jiffshot FTP", user)? {
        Some(password) => password,
        None if user == "anonymous" => "jiffshot@".to_owned(),
        None => return Err(format!("No Jiffshot FTP password was found in the system credential store for {user}.")),
    };
    ftp_argument("password", &password)?;

    let control = TcpStream::connect(&host).map_err(|e| format!("cannot reach {host}: {e}"))?;
    control.set_read_timeout(Some(std::time::Duration::from_secs(20))).map_err(|e| format!("cannot set the FTP read timeout: {e}"))?;
    control.set_write_timeout(Some(std::time::Duration::from_secs(20))).map_err(|e| format!("cannot set the FTP write timeout: {e}"))?;
    let mut reader = BufReader::new(control.try_clone().map_err(|e| e.to_string())?);
    let mut writer = control;
    let mut reply = || -> Result<(u32, String), String> {
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Err("FTP server closed the connection".into());
            }
            // Multi-line replies end with "NNN " (space after the code).
            if line.len() >= 4 && line.as_bytes()[3] == b' ' {
                let code = line[..3].parse().unwrap_or(0);
                return Ok((code, line.trim().to_owned()));
            }
        }
    };
    let mut send = |cmd: &str| writer.write_all(format!("{cmd}\r\n").as_bytes()).map_err(|e| e.to_string());
    let expect = |(code, text): (u32, String), ok: &[u32]| if ok.contains(&code) { Ok(text) } else { Err(format!("FTP server: {text}")) };
    expect(reply()?, &[220])?;
    send(&format!("USER {user}"))?;
    let (code, text) = reply()?;
    if code == 331 {
        send(&format!("PASS {password}"))?;
        expect(reply()?, &[230, 202])?;
    } else {
        expect((code, text), &[230])?;
    }
    send("TYPE I")?;
    expect(reply()?, &[200])?;
    if !folder.is_empty() {
        send(&format!("CWD /{folder}"))?;
        expect(reply()?, &[250])?;
    }
    send("PASV")?;
    let pasv = expect(reply()?, &[227])?;
    let data_addr = passive_address(&pasv)?;
    let mut data = TcpStream::connect(data_addr).map_err(|e| format!("cannot connect to the FTP data channel: {e}"))?;
    data.set_write_timeout(Some(std::time::Duration::from_secs(20))).map_err(|e| format!("cannot set the FTP data timeout: {e}"))?;
    send(&format!("STOR {name}"))?;
    expect(reply()?, &[125, 150])?;
    io::copy(&mut source, &mut data).map_err(|e| format!("FTP upload failed: {e}; the remote file may be incomplete"))?;
    drop(data);
    expect(reply()?, &[226, 250])?;
    send("QUIT")?;
    Ok(format!("{}/{name}", url.trim_end_matches('/')))
}

/// A generic credential's password from Windows Credential Manager.
#[cfg(windows)]
fn credential(target: &str, _user: &str) -> Result<Option<String>, String> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
    use windows_sys::Win32::Security::Credentials::{CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC};
    let name = wide(target);
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: CredReadW allocates the credential, freed with CredFree.
    unsafe {
        if CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) == 0 {
            let error = GetLastError();
            return if error == ERROR_NOT_FOUND { Ok(None) } else { Err(format!("Could not read the FTP credential (Windows error {error}).")) };
        }
        if cred.is_null() {
            return Err("Windows returned an empty FTP credential.".into());
        }
        let c = &*cred;
        let units: Vec<u16> = if c.CredentialBlobSize == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize)
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect()
        };
        let password = String::from_utf16_lossy(&units);
        CredFree(cred as *const _);
        Ok(Some(password))
    }
}

/// A generic-password item with this service and FTP username in Keychain.
#[cfg(target_os = "macos")]
fn credential(target: &str, user: &str) -> Result<Option<String>, String> {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSDictionary, NSNumber, NSString};
    use std::ffi::c_void;
    use std::ptr;

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecClass: *const NSString;
        static kSecClassGenericPassword: *const AnyObject;
        static kSecAttrService: *const NSString;
        static kSecAttrAccount: *const NSString;
        static kSecReturnData: *const NSString;
        fn SecItemCopyMatching(query: *const c_void, result: *mut *const c_void) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFGetTypeID(value: *const c_void) -> usize;
        fn CFDataGetTypeID() -> usize;
        fn CFDataGetLength(data: *const c_void) -> isize;
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFRelease(value: *const c_void);
    }

    let service = NSString::from_str(target);
    let account = NSString::from_str(user);
    let yes = NSNumber::new_bool(true);
    // SAFETY: Security's constants are immutable, toll-free-bridged
    // Foundation objects, and the query retains its keys and values.
    let query = unsafe {
        NSDictionary::from_slices(
            &[&*kSecClass, &*kSecAttrService, &*kSecAttrAccount, &*kSecReturnData],
            &[&*kSecClassGenericPassword, &service, &account, &yes],
        )
    };
    let mut result = ptr::null();
    // SAFETY: `query` and the out-parameter are valid for the call.
    let status = unsafe { SecItemCopyMatching((&*query as *const NSDictionary<NSString, AnyObject>).cast(), &mut result) };
    if status == -25300 {
        return Ok(None);
    }
    if status != 0 {
        return Err(format!("Could not read the FTP password from Keychain (security status {status})."));
    }
    if result.is_null() {
        return Err("Keychain returned an empty FTP credential.".into());
    }
    // SAFETY: a successful query returns a retained object, released here.
    unsafe {
        let password = if CFGetTypeID(result) != CFDataGetTypeID() {
            Err("Keychain returned an unexpected FTP credential.".to_owned())
        } else {
            let len = CFDataGetLength(result);
            let bytes = CFDataGetBytePtr(result);
            if len < 0 || (len > 0 && bytes.is_null()) {
                Err("Keychain returned invalid FTP password data.".to_owned())
            } else {
                let bytes = if len == 0 { &[][..] } else { std::slice::from_raw_parts(bytes, len as usize) };
                String::from_utf8(bytes.to_vec()).map(Some).map_err(|_| "The FTP password in Keychain is not UTF-8.".to_owned())
            }
        };
        CFRelease(result);
        password
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;

    struct ScratchFile(std::path::PathBuf);

    impl ScratchFile {
        fn new(stem: &str, contents: &[u8]) -> Self {
            let path = temporary_export_path(stem, "png").unwrap();
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }
    }

    impl Drop for ScratchFile {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_file(&self.0) {
                eprintln!("could not remove the FTP test file: {error}");
            }
            if let Some(parent) = self.0.parent() {
                if let Err(error) = std::fs::remove_dir(parent) {
                    eprintln!("could not remove the FTP test folder: {error}");
                }
            }
        }
    }

    #[test]
    fn ftp_passive_address_rejects_malformed_octets_and_ports() {
        assert_eq!(passive_address("227 Entering Passive Mode (127,0,0,1,16,42)").unwrap().to_string(), "127.0.0.1:4138");
        assert_eq!(passive_address("227 (255,255,255,255,255,255)").unwrap().port(), 65535);
        for reply in [
            "227 no address", "227 (127,0,0,1,0,0)", "227 (127,0,0,1,256,2)",
            "227 (127,0,0,1,-1,2)", "227 (127,0,0,1,1,2,3)", "227 (127,0,0,1,no,2)",
        ] {
            assert!(passive_address(reply).is_err(), "{reply}");
        }
    }

    #[test]
    fn ftp_rejects_line_breaks_before_opening_a_connection() {
        let file = ScratchFile::new("Capture", b"fixture");
        let bad_name = ScratchFile::new("Bad\nName", b"fixture");
        let must_not_lookup = |_: &str, _: &str| -> Result<Option<String>, String> {
            panic!("invalid FTP arguments must not reach the credential store")
        };
        assert!(ftp_with_credential("ftp://alice\r\nQUIT@127.0.0.1/folder", &file.0, must_not_lookup).unwrap_err().contains("username"));
        assert!(ftp_with_credential("ftp://alice@127.0.0.1/folder\nQUIT", &file.0, must_not_lookup).unwrap_err().contains("folder"));
        assert!(ftp_with_credential("ftp://alice@127.0.0.1/folder", &bad_name.0, must_not_lookup).unwrap_err().contains("filename"));
        assert!(ftp_with_credential("ftp://alice@127.0.0.1/folder", &file.0, |_, _| Ok(Some("secret\r\nQUIT".into())))
            .unwrap_err().contains("password"));
    }

    #[test]
    fn ftp_uploads_the_source_to_a_local_server() {
        fn exchange(reader: &mut BufReader<TcpStream>, writer: &mut TcpStream, expected: &str, response: &str) {
            let mut command = String::new();
            reader.read_line(&mut command).unwrap();
            assert_eq!(command, format!("{expected}\r\n"));
            writer.write_all(response.as_bytes()).unwrap();
        }

        let bytes = b"\x89PNG\r\n\x1a\nsynthetic image payload";
        let file = ScratchFile::new("Capture", bytes);
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut control, _) = listener.accept().unwrap();
            control.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(control.try_clone().unwrap());
            control.write_all(b"220 fixture ready\r\n").unwrap();
            exchange(&mut reader, &mut control, "USER alice", "331 password required\r\n");
            exchange(&mut reader, &mut control, "PASS fixture-password", "230 logged in\r\n");
            exchange(&mut reader, &mut control, "TYPE I", "200 binary\r\n");
            exchange(&mut reader, &mut control, "CWD /synthetic", "250 changed\r\n");
            let data_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let data_port = data_listener.local_addr().unwrap().port();
            exchange(&mut reader, &mut control, "PASV", &format!("227 (127,0,0,1,{},{})\r\n", data_port / 256, data_port % 256));
            let (mut data, _) = data_listener.accept().unwrap();
            data.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            exchange(&mut reader, &mut control, "STOR Capture.png", "150 send data\r\n");
            let mut received = Vec::new();
            data.read_to_end(&mut received).unwrap();
            control.write_all(b"226 stored\r\n").unwrap();
            let mut command = String::new();
            reader.read_line(&mut command).unwrap();
            assert_eq!(command, "QUIT\r\n");
            received
        });
        let url = format!("ftp://alice@127.0.0.1:{port}/synthetic");
        let result = ftp_with_credential(&url, &file.0, |service, user| {
            assert_eq!((service, user), ("Jiffshot FTP", "alice"));
            Ok(Some("fixture-password".into()))
        }).unwrap();
        assert_eq!(result, format!("{url}/Capture.png"));
        assert_eq!(server.join().unwrap(), bytes);
    }

    #[test]
    fn temporary_exports_are_private_and_do_not_overwrite_each_other() {
        let first = temporary_export_path("Capture", "png").unwrap();
        let second = temporary_export_path("Capture", "png").unwrap();
        assert_ne!(first, second);
        assert_eq!(first.file_name(), second.file_name());

        for path in [&first, &second] {
            let dir = path.parent().unwrap();
            assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o077, 0);
        }
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");

        for path in [&first, &second] {
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn temporary_exports_reject_path_components() {
        assert!(temporary_export_path("../capture", "png").is_err());
        assert!(temporary_export_path("capture", "../png").is_err());
        assert!(temporary_export_path("..", "png").is_err());
    }
}
