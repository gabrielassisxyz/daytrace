//! Whole-binary exercises for exit codes and usage text.
//!
//! These run the built binary as a child process so the exit code is the real one and
//! the usage block is exactly what a user would see.

use std::process::{Command, Stdio};

#[test]
fn an_unknown_command_prints_usage_and_exits_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .arg("capture")
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace with an unknown command");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "an unknown command must fail: {stderr}"
    );
    assert!(
        output.status.code() == Some(2),
        "a usage error must exit 2: {stderr}"
    );
    assert!(
        stderr.contains("Usage:"),
        "a usage error must print the usage block: {stderr}"
    );
    assert!(
        stderr.contains("unknown command: capture"),
        "the error has to name the bad command: {stderr}"
    );
}

#[test]
fn a_mistyped_reporting_flag_prints_usage_and_exits_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .args(["today", "--dat", "2026-01-01"])
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace today with a mistyped flag");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "a mistyped flag must fail: {stderr}"
    );
    assert!(
        output.status.code() == Some(2),
        "a usage error must exit 2: {stderr}"
    );
    assert!(
        stderr.contains("Usage:"),
        "a usage error must print the usage block: {stderr}"
    );
    assert!(
        stderr.contains("--dat"),
        "the error has to name the mistyped flag: {stderr}"
    );
}
