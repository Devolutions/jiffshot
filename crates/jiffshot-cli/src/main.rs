#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[path = "windows.rs"]
mod desktop;

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn main() -> std::process::ExitCode {
    desktop::main()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn main() -> std::process::ExitCode {
    eprintln!("jiffshot-cli is currently available only on Windows and macOS");
    std::process::ExitCode::FAILURE
}
