//! Whole-binary exercise for the refusal a duplicate capture daemon has to produce.
//!
//! It runs headless, and that is the point. The claim on the database is tested before the
//! compositor is queried and before an input device is opened, so a duplicate is refused on a
//! machine that has neither, and the refusal says what is actually wrong. A test that needed a
//! live desktop could not pin that ordering, and the ordering is the part that matters: a
//! duplicate must not open every input device on its way to being told it is a duplicate.
//!
//! The claim is held here with a plain lock on the file beside the database rather than through
//! the daemon's own code, so what the test depends on is the observable contract.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Stand in for a daemon already capturing into `db_path`.
///
/// The returned file has to outlive the child process: dropping it releases the claim, which is
/// exactly how a real daemon hands capture over when it exits.
fn hold_capture_claim(db_path: &Path) -> File {
    let path = PathBuf::from(format!("{}.lock", db_path.display()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Stated rather than left to the default, because not truncating is the whole point on
        // the real path: the claim is opened before the lock can be tested, so a truncating open
        // would erase a live holder's pid on the way to being refused.
        .truncate(false)
        .open(&path)
        .expect("open the capture claim");
    file.try_lock().expect("hold the capture claim");

    let mut handle = &file;
    handle
        .write_all(format!("{}\n", std::process::id()).as_bytes())
        .expect("record this process as the holder");
    file
}

/// Everything a caller can observe of a run: a process has exactly these three channels and no
/// fourth, so comparing them together is what turns "and nothing else" into a claim the test
/// actually holds. Asserting them one at a time states only what each assertion happens to
/// mention, and the channel nobody thought of is the one a regression escapes through.
///
/// Lossy strings rather than raw bytes, so a mismatch prints as text that can be read.
fn observable(output: &Output) -> (Option<i32>, String, String) {
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_second_capture_daemon_is_refused_and_names_the_running_one() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = directory.path().join("daytrace.db");
    let _holder = hold_capture_claim(&db_path);

    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .arg("start")
        .env("DAYTRACE_DB_PATH", &db_path)
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace start");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The only value in the message that is not fixed by the source is the pid of the holder,
    // which is this test process itself, so the whole run can be reconstructed and compared.
    // Exit 1 rather than 2: a duplicate is a runtime failure, not a bad invocation, and the unit
    // needs it to stay one, since a refusal declared successful removes the retry window a manual
    // daemon's exit relies on. Empty stdout is not a formality either, since the usage block
    // reaches stdout on the path that prints help, which returns it as a successful result.
    // Naming the holder's pid is part of the same comparison rather than a check of its own: the
    // expected message is built from it, so an exact match is what pins it.
    let expected = format!(
        "capture is already running as pid {} on {}\n\
         a second capture process reads its own configuration, so the two would disagree about \
         what may be recorded; stop the running one before starting another\n",
        std::process::id(),
        db_path.display(),
    );
    assert_eq!(
        observable(&output),
        (Some(1), String::new(), expected),
        "a duplicate-capture refusal must produce exactly this and nothing else, on any stream"
    );
    // Worth stating what this last one is worth: it discriminates the two orderings only where
    // `InputActivity::start` fails, which is a machine with no readable input device. On a
    // desktop that has one, opening the devices succeeds silently and the refusal reads the same
    // either way, so the ordering is pinned on a headless runner and merely documented here.
    assert!(
        !stderr.contains("/dev/input"),
        "the claim must be tested before the input devices are opened, or a duplicate acquires \
         that capability just to be refused afterwards: {stderr}"
    );
}

#[test]
fn a_configuration_failure_prints_only_the_error_and_exits_one() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = directory.path().join("daytrace.db");

    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .arg("today")
        .env("DAYTRACE_DB_PATH", &db_path)
        .env("DAYTRACE_IDLE_AFTER_SECONDS", "abc")
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace today");
    // Every part of this run is fixed by the source for this input, so nothing short of the whole
    // observable result rules out a regression that prints the right error and a usage block too.
    assert_eq!(
        observable(&output),
        (
            Some(1),
            String::new(),
            "DAYTRACE_IDLE_AFTER_SECONDS must be an integer number of seconds\n".to_string()
        ),
        "a configuration failure must produce exactly this and nothing else, on any stream"
    );
}

/// Pruning must not wait on capture either, and for a sharper reason than reading does.
///
/// The command is meant to be run from a timer beside a daemon that runs all day, so a prune that
/// took the claim would be refused on every machine that follows the documented setup, and the
/// retention window would quietly never be applied on exactly those.
#[test]
fn pruning_does_not_wait_on_the_capture_claim() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = directory.path().join("daytrace.db");
    let _holder = hold_capture_claim(&db_path);

    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .arg("prune")
        .env("DAYTRACE_DB_PATH", &db_path)
        .env("DAYTRACE_RETENTION_DAYS", "30")
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace prune");

    assert!(
        output.status.success(),
        "pruning while capture is held must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Reading the day must not wait on capture, or a report would block whenever the daemon runs.
///
/// The claim is deliberately a file beside the database rather than the database itself, and
/// `today` and `export` never open it. Moving the claim onto the store would break both while
/// still passing every test above.
#[test]
fn reporting_a_day_does_not_wait_on_the_capture_claim() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = directory.path().join("daytrace.db");
    let _holder = hold_capture_claim(&db_path);

    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .arg("today")
        .env("DAYTRACE_DB_PATH", &db_path)
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace today");

    assert!(
        output.status.success(),
        "reporting a day while capture is held must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
