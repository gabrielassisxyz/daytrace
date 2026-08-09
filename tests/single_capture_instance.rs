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
use std::process::{Command, Stdio};

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

/// Every non-empty line the usage block prints, read from the binary itself rather than
/// copied by hand, so a runtime-failure test that checks for leaked usage lines stays correct
/// even if the block's wording changes.
fn usage_lines() -> Vec<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .arg("help")
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace help");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        // The block opens with the bare program name as a heading. It carries no usage
        // wording of its own and collides with unrelated output that happens to name a
        // `daytrace.db` file, so checking for it produces false positives rather than
        // catching a leaked usage block.
        .filter(|line| !line.trim().is_empty() && line.trim() != "daytrace")
        .map(str::to_string)
        .collect()
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

    assert!(
        !output.status.success(),
        "a second daemon on a database already being captured must not start: {stderr}"
    );
    assert!(
        output.status.code() == Some(1),
        "a duplicate-capture refusal is a runtime failure, so it must exit 1, not 2: {stderr}"
    );

    // The only value in the message that is not fixed by the source is the pid of the holder,
    // which is this test process itself, so the full message can be reconstructed and compared
    // for exact equality instead of only asserting a handful of substrings are present.
    let expected = format!(
        "capture is already running as pid {} on {}\n\
         a second capture process reads its own configuration, so the two would disagree about \
         what may be recorded; stop the running one before starting another\n",
        std::process::id(),
        db_path.display(),
    );
    assert_eq!(
        stderr, expected,
        "a duplicate-capture refusal must print exactly this message and nothing else"
    );
    // A regression that appended the usage block after the real message would still contain
    // every substring checked above; comparing full lines against the block itself catches that.
    for line in usage_lines() {
        assert!(
            !stderr.contains(&line),
            "a runtime failure must not leak any line of the usage block ({line:?}): {stderr}"
        );
    }
    // Stderr alone does not settle it. The usage block reaches stdout on the path that prints
    // help, since that path returns it as a successful result, so a runtime failure that printed
    // it would leak it there rather than into the stream asserted above.
    assert!(
        output.stdout.is_empty(),
        "a runtime failure must write nothing to stdout: {}",
        String::from_utf8_lossy(&output.stdout)
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
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "a configuration failure must fail: {stderr}"
    );
    assert!(
        output.status.code() == Some(1),
        "a configuration failure is a runtime failure, so it must exit 1: {stderr}"
    );
    // The message is fixed by the source for this input, so nothing short of exact equality
    // rules out a regression that prints the right error alongside a leaked usage block.
    assert_eq!(
        stderr, "DAYTRACE_IDLE_AFTER_SECONDS must be an integer number of seconds\n",
        "a runtime failure must print exactly the offending setting and nothing else"
    );
    assert!(
        output.stdout.is_empty(),
        "a runtime failure must write nothing to stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Reading the day must not wait on capture, or a report would block whenever the daemon runs.
///
/// The claim is deliberately a file beside the database rather than the database itself, and
/// `today` and `export` never open it. Moving the claim onto the store would break both while
/// still passing every test above.
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
