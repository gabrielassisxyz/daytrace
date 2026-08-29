//! Whole-binary exercises for exit codes and usage text.
//!
//! These run the built binary as a child process so the exit code is the real one and
//! the usage block is exactly what a user would see.

use std::process::{Command, Stdio};

/// A database path that exists nowhere on disk, so `today` and `export` read it as an empty
/// day rather than either touching the real capture database or requiring one to be built here.
fn empty_db_path(directory: &std::path::Path) -> std::path::PathBuf {
    directory.join("no-such-database.db")
}

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

#[test]
fn today_raw_runs_and_exits_zero() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .args(["today", "--raw", "--date", "2026-01-01"])
        .env("DAYTRACE_DB_PATH", empty_db_path(directory.path()))
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace today --raw");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "a valid --raw invocation must exit zero: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "a valid --raw invocation must exit zero: {stderr}"
    );
}

#[test]
fn export_raw_is_a_usage_error() {
    // export was never aggregated, so the flag has nothing to opt out of.
    let directory = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .args(["export", "--raw"])
        .env("DAYTRACE_DB_PATH", empty_db_path(directory.path()))
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace export --raw");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "export --raw must fail: {stderr}");
    assert_eq!(
        output.status.code(),
        Some(2),
        "export --raw is a usage error, not a runtime one: {stderr}"
    );
    assert!(
        stderr.contains("Usage:"),
        "a usage error must print the usage block: {stderr}"
    );
    assert!(
        stderr.contains("--raw"),
        "the error has to name the unexpected flag: {stderr}"
    );
}

#[test]
fn an_unknown_flag_on_today_alongside_raw_still_exits_two() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .args(["today", "--raw", "--bogus"])
        .env("DAYTRACE_DB_PATH", empty_db_path(directory.path()))
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace today --raw --bogus");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "an unrecognised flag must fail even beside --raw: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unrecognised flag is a usage error: {stderr}"
    );
    assert!(
        stderr.contains("Usage:"),
        "a usage error must print the usage block: {stderr}"
    );
    assert!(
        stderr.contains("--bogus"),
        "the error has to name the unrecognised flag: {stderr}"
    );
}
