//! Without a login the binary explains how to log in and exits before touching the terminal.

use std::process::{Command, Stdio};

#[test]
fn not_logged_in_prints_hint_and_exits_1_without_touching_the_terminal() {
    let out = Command::new(env!("CARGO_BIN_EXE_railway-tui"))
        .env("HOME", "/nonexistent")
        .env_remove("RAILWAY_API_TOKEN")
        .env_remove("RAILWAY_ENV")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("railway login"), "stderr: {stderr}");
    // No alternate screen, raw mode or other escape sequences were written.
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
    assert!(!stderr.contains('\x1b'), "stderr: {stderr:?}");
}
