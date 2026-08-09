//! Whole-binary exercises for a `DAYTRACE_DB_PATH` that is not valid UTF-8.
//!
//! A path is bytes on this platform, and `env::var` fails to decode one that is not valid
//! UTF-8, which used to be handled exactly like an unset variable: capture moved to the
//! default store without saying so. Every case here sets the override on the *spawned*
//! process rather than through `std::env::set_var`, since the variable is shared by every
//! test in the binary; and every case points `HOME`/`XDG_DATA_HOME` at a throwaway sandbox,
//! so "the default store was never touched" is a real filesystem check rather than an
//! assumption about the machine a test happens to run on.
//!
//! `daytrace start` cannot be asserted the same way a reporting command can. A reporting
//! command never creates a store from nothing, since an empty report must not leave a file
//! behind, so proving it opened the exact byte path requires seeding one first. The daemon,
//! by contrast, always tries to create one, but on a machine with a real compositor and a
//! readable input device (this repository's own gate runs on the maintainer's desktop, not
//! inside a headless container) it would run forever rather than fail. Both cases are worked
//! around the same way the existing duplicate-daemon test already does: a capture claim is
//! held at the path the daemon would compute for the byte-exact override before it starts,
//! so a daemon that resolved that same path is refused immediately, before it ever queries
//! the compositor or opens an input device. A daemon that instead fell back to the default
//! store would not collide with that claim at all, and a bounded wait then treats "did not
//! exit" itself as a failure, so a hung real daemon is never left running in the background.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long a daemon that correctly collided with the held claim should take to exit.
///
/// Generous on purpose: the failure mode this guards is a daemon that did not collide and
/// instead ran to completion, which never returns on its own. A slow but correct exit must
/// never be mistaken for that.
const START_REFUSAL_TIMEOUT: Duration = Duration::from_secs(5);

/// A directory layout that makes "the default store" a real, checkable location instead of
/// whatever `~/.local/share` happens to be on the machine a test runs on.
struct Sandbox {
    _root: tempfile::TempDir,
    home: PathBuf,
    xdg_data_home: PathBuf,
    store_dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create a sandbox directory");
        let home = root.path().join("home");
        let xdg_data_home = root.path().join("xdg-data");
        let store_dir = root.path().join("store");
        for dir in [&home, &xdg_data_home, &store_dir] {
            fs::create_dir_all(dir).expect("create a sandbox subdirectory");
        }
        Self {
            _root: root,
            home,
            xdg_data_home,
            store_dir,
        }
    }

    /// A path whose final component fails UTF-8 decoding, confined to the leaf name so the
    /// parent directory a lossy reconstruction elsewhere in the codebase might still compute
    /// (`lock.rs`'s own `display()`-based lock path, unrelated to this defect) still lands
    /// where this test expects it.
    fn non_utf8_db_path(&self) -> PathBuf {
        let mut bytes = self.store_dir.as_os_str().as_bytes().to_vec();
        bytes.push(b'/');
        bytes.extend_from_slice(b"daytrace-\xFF-store.db");
        PathBuf::from(OsString::from_vec(bytes))
    }

    fn default_store_dir(&self) -> PathBuf {
        self.xdg_data_home.join("daytrace")
    }

    fn default_store_path(&self) -> PathBuf {
        self.default_store_dir().join("daytrace.db")
    }

    fn command(&self, arg: &str, db_path: &std::path::Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_daytrace"));
        command
            .arg(arg)
            .env("DAYTRACE_DB_PATH", db_path)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.xdg_data_home)
            .stdin(Stdio::null());
        command
    }
}

#[test]
fn a_non_utf8_db_path_is_opened_and_secured_by_a_reporting_command() {
    let sandbox = Sandbox::new();
    let db_path = sandbox.non_utf8_db_path();

    // Seeded rather than left absent: `today` never creates a store when there is nothing to
    // report, so the only way to observe it acting on this exact path is to give it a file
    // to open. World-readable on purpose, so a chmod to 0600 during `Store::open` is a change
    // this test can actually see rather than a mode the file already had.
    fs::write(&db_path, []).expect("seed an empty store file at the byte-exact path");
    fs::set_permissions(&db_path, fs::Permissions::from_mode(0o644)).expect("seed permissions");

    assert!(
        !sandbox.default_store_path().exists(),
        "the default store must not exist before the run, or its absence afterwards would \
         prove nothing"
    );

    let output = sandbox
        .command("today", &db_path)
        .output()
        .expect("run daytrace today");

    assert!(
        output.status.success(),
        "a non-UTF-8 override must not be treated as a bad invocation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !sandbox.default_store_dir().exists(),
        "a reporting command given a non-UTF-8 path must never fall back to the default \
         store, checked after the run against the same throwaway HOME/XDG_DATA_HOME"
    );

    let metadata = fs::metadata(&db_path)
        .expect("the byte-exact path must still be the store, not a path nobody wrote to");
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        0o600,
        "opening the store must have run `secure_permissions` against this exact file, which \
         only happens if the override was read as the byte-exact path rather than as unset"
    );
    assert!(
        metadata.len() > 0,
        "opening the store must have run its schema migration against this exact file"
    );
}

#[test]
fn a_non_utf8_db_path_is_resolved_by_start_before_the_compositor_or_input_is_touched() {
    let sandbox = Sandbox::new();
    let db_path = sandbox.non_utf8_db_path();

    // The exact lock path `CaptureLock::acquire` computes for this database. Held here so a
    // daemon that resolves the same byte-exact override collides with it immediately, the
    // same technique the duplicate-daemon test already relies on to make `start` observable
    // without a compositor or an input device.
    let lock_path = PathBuf::from(format!("{}.lock", db_path.display()));
    let holder = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open the capture claim ahead of the daemon");
    holder.try_lock().expect("hold the capture claim");

    let output = run_with_timeout(sandbox.command("start", &db_path), START_REFUSAL_TIMEOUT);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "a daemon refused for a claim already held on the exact path it resolved must exit \
         with a runtime failure: {stderr}"
    );
    assert!(
        stderr.contains("capture is already running"),
        "the refusal must be the capture-claim one, proving the daemon resolved the same \
         byte-exact path this test is holding a claim on: {stderr}"
    );
    assert!(
        stderr.contains(&sandbox.store_dir.display().to_string()),
        "the refusal must name a path under the byte-exact override's own directory, not a \
         default location it fell back to: {stderr}"
    );
    assert!(
        !sandbox.default_store_dir().exists(),
        "a daemon given a non-UTF-8 path must never fall back to the default store, checked \
         after the run against the same throwaway HOME/XDG_DATA_HOME"
    );
}

/// Run `command`, but never let it outlive `timeout`.
///
/// A daemon that fell back to the default store would not collide with the held claim at
/// all: it would query the compositor, open an input device and loop forever. `Output`'s own
/// blocking wait has no bound, and this repository's own gate runs on a real desktop rather
/// than inside a headless container, so nothing else stops such a daemon from actually
/// running. Killing it on timeout is not an optimization; it is what keeps a failing case
/// from leaving a real capture daemon behind.
fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daytrace");

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the daytrace process") {
            break status;
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .expect("kill a daemon that did not resolve the path fast enough");
            child.wait().expect("reap the killed daemon");
            panic!(
                "daytrace start did not exit within {timeout:?}: a daemon that resolved the \
                 default store instead of the given byte path would not collide with the held \
                 capture claim, and would run to completion instead of failing fast"
            );
        }
        thread::sleep(Duration::from_millis(20));
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout was piped")
        .read_to_end(&mut stdout)
        .expect("read stdout");
    child
        .stderr
        .take()
        .expect("stderr was piped")
        .read_to_end(&mut stderr)
        .expect("read stderr");

    Output {
        status,
        stdout,
        stderr,
    }
}
