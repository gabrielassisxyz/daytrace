//! Whole-binary exercise for `DAYTRACE_MEDIA_POLL_SECONDS`.
//!
//! `today` runs `Config::from_env` without touching the compositor or the bus, which is what
//! makes it the cheapest command that still proves a configuration failure: no live desktop and
//! no `busctl` are required for the process to reach the point where the variable is parsed.

use std::path::Path;
use std::process::{Command, Output};

fn observable(output: &Output) -> (Option<i32>, String, String) {
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_today(directory: &Path, value: Option<&str>) -> Output {
    let db_path = directory.join("daytrace.db");
    let mut command = Command::new(env!("CARGO_BIN_EXE_daytrace"));
    command
        .arg("today")
        .env("DAYTRACE_DB_PATH", &db_path)
        .stdin(std::process::Stdio::null());
    if let Some(value) = value {
        command.env("DAYTRACE_MEDIA_POLL_SECONDS", value);
    }
    command.output().expect("run daytrace today")
}

#[test]
fn an_unset_media_poll_interval_does_not_fail_configuration() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = run_today(directory.path(), None);
    assert_eq!(output.status.code(), Some(0), "{:?}", observable(&output));
}

#[test]
fn a_zero_media_poll_interval_clamps_rather_than_failing_configuration() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = run_today(directory.path(), Some("0"));
    assert_eq!(
        output.status.code(),
        Some(0),
        "a value below one must clamp to one, not be refused: {:?}",
        observable(&output)
    );
}

#[test]
fn a_malformed_media_poll_interval_fails_naming_the_variable() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = run_today(directory.path(), Some("soon"));
    assert_eq!(
        observable(&output),
        (
            Some(1),
            String::new(),
            "DAYTRACE_MEDIA_POLL_SECONDS must be an integer number of seconds\n".to_string()
        ),
        "a configuration failure must name DAYTRACE_MEDIA_POLL_SECONDS, not some other setting"
    );
}

#[test]
fn changing_the_desktop_poll_interval_does_not_change_the_media_one() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = directory.path().join("daytrace.db");
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .arg("today")
        .env("DAYTRACE_DB_PATH", &db_path)
        // A malformed DAYTRACE_POLL_SECONDS must be reported as itself: if the two settings
        // shared a variable, this would either fail for the wrong reason or not fail at all.
        .env("DAYTRACE_POLL_SECONDS", "soon")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run daytrace today");
    assert_eq!(
        observable(&output),
        (
            Some(1),
            String::new(),
            "DAYTRACE_POLL_SECONDS must be an integer number of seconds\n".to_string()
        ),
        "DAYTRACE_MEDIA_POLL_SECONDS must not be affected by, or reported instead of, \
         DAYTRACE_POLL_SECONDS: {:?}",
        observable(&output)
    );
}
