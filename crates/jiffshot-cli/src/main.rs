#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    windows::main()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("jiffshot-cli is currently available only on Windows");
    std::process::ExitCode::FAILURE
}
