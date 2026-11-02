#![cfg(not(any(windows, target_os = "macos")))]

use std::process::Command;

#[test]
fn non_windows_cli_reports_unavailability_and_fails() {
    let output = Command::new(env!("CARGO_BIN_EXE_jiffshot-cli"))
        .output()
        .expect("start the CLI");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "jiffshot-cli is currently available only on Windows and macOS",
    );
}
