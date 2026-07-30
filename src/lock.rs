use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

/// An exclusive claim on one database's capture, held for as long as the daemon runs.
///
/// WHY a lock at all: the store survives two daemons, since every write takes an IMMEDIATE
/// transaction and the loser waits its turn. What does not survive is agreement about what
/// may be recorded. Each process reads its own configuration, so a blacklist set in one shell
/// is invisible to the other, and the process without it records the application the other
/// one exists to skip. The duplicate is therefore a privacy regression, not a storage one.
///
/// WHY `flock` on a file beside the database, rather than a pid file: the kernel releases it
/// when the holder dies, including under SIGKILL, so there is no stale claim to detect and no
/// recovery path to get wrong. A pid file would need both, and would still confuse a reused
/// pid for a live daemon.
#[derive(Debug)]
pub struct CaptureLock {
    /// Held only for its lifetime: dropping the file releases the claim.
    _file: File,
}

impl CaptureLock {
    /// Claim capture for this process, or fail naming the process that already holds it.
    ///
    /// The claim is per database rather than per machine. Two daemons writing different stores
    /// are not duplicates of each other, and scoping it to the path keeps the guard honest
    /// about what it actually protects.
    pub fn acquire(db_path: &Path) -> Result<Self, String> {
        let path = lock_path(db_path);
        if let Some(parent) = path.parent() {
            create_private_dir(parent)?;
        }

        let file = open_private(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(already_capturing(db_path, holder_pid(&path)));
            }
            Err(TryLockError::Error(error)) => {
                return Err(format!("failed to claim capture: {error}"));
            }
        }
        record_holder(&file)?;

        Ok(Self { _file: file })
    }
}

fn lock_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", db_path.display()))
}

fn already_capturing(db_path: &Path, holder: Option<u32>) -> String {
    let holder = match holder {
        Some(pid) => format!("as pid {pid}"),
        None => "in another process".to_string(),
    };
    format!(
        "capture is already running {holder} on {}\n\
         a second capture process reads its own configuration, so the two would disagree about \
         what may be recorded; stop the running one before starting another",
        db_path.display()
    )
}

/// Create the directory the claim lives in, private from the start.
///
/// The claim is the first thing the daemon takes, so on a fresh machine this is what creates
/// the data directory, before the store exists to tighten it to 0700. On a machine where
/// capture cannot start at all, because no input device is readable, the store is never opened
/// and nothing tightens it later, so a directory created at the ambient umask would stay
/// traversable. An existing directory keeps the mode it has, which is what an explicit
/// `DAYTRACE_DB_PATH` pointing into a directory the user manages relies on.
fn create_private_dir(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }

    builder
        .create(path)
        .map_err(|error| format!("failed to create data directory: {error}"))
}

/// Open the lock file without disturbing what a live holder wrote there.
///
/// Deliberately not `truncate`: the file has to be opened before the lock can be tested, so a
/// truncating open would erase the holder's pid on the way to being refused, leaving the
/// refusal with nobody to name.
fn open_private(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        // The claim is opened for writing and then truncated, so following a symlink planted at
        // this path would make starting capture a way to empty any file this user can write.
        // That is reachable whenever the database sits in a directory something else can write
        // to, which an explicit `DAYTRACE_DB_PATH` allows. A lock is never legitimately a
        // symlink, so refusing one costs nothing.
        options.custom_flags(libc::O_NOFOLLOW);
    }

    let file = options
        .open(path)
        .map_err(|error| describe_open_failure(path, &error))?;
    // The mode above applies only to a file this call creates, and the lock sits in the same
    // directory as the database, whose contents are private.
    secure_lock_permissions(&file)?;
    Ok(file)
}

/// Say what went wrong, distinguishing a planted symlink from an ordinary mishap.
///
/// `O_NOFOLLOW` reports a symlink as `ELOOP`, which reads as "too many levels of symbolic
/// links" and sends the reader looking for a loop that is not there. The refusal is not
/// matched on `ErrorKind`, since the variant for it is still unstable.
fn describe_open_failure(path: &Path, error: &io::Error) -> String {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return format!(
            "the capture lock at {} is a symbolic link, which it must never be; refusing to \
             follow it",
            path.display()
        );
    }

    format!(
        "failed to open the capture lock at {}: {error}",
        path.display()
    )
}

/// Set the mode through the open file rather than through the path.
///
/// Re-resolving the path after the open would change the mode of whatever answers to that name
/// now, which need not be the file the claim is held on.
#[cfg(unix)]
fn secure_lock_permissions(file: &File) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("failed to secure capture lock permissions: {error}"))
}

#[cfg(not(unix))]
fn secure_lock_permissions(_file: &File) -> Result<(), String> {
    Ok(())
}

/// The pid the file records, which is the holder's except in one window.
///
/// A claim is taken in two steps, the lock and then the pid, and the file is not the claim: the
/// kernel is. So a process refused between those two steps reads either nothing, on a lock file
/// that never had a pid in it, or the pid of the *previous* holder, which by then may be dead or
/// reused. Both are reported as read. Widening the guard to reject them would mean asking
/// whether that pid is alive, which is the pid-file liveness check this design exists to avoid,
/// and which cannot answer the question either. The refusal is therefore accurate about capture
/// being held and best-effort about by whom.
fn holder_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Name this process in the lock file, so a refusal can point at something actionable.
///
/// Written only by a process that holds the lock, and immediately, so the window in which the
/// file still names the previous holder is two syscalls wide. The truncation matters: a shorter
/// pid written over a longer one would otherwise leave the tail of the old number behind, and
/// the result would parse as neither.
fn record_holder(mut file: &File) -> Result<(), String> {
    file.set_len(0)
        .map_err(|error| format!("failed to clear the capture lock: {error}"))?;
    file.write_all(format!("{}\n", process::id()).as_bytes())
        .map_err(|error| format!("failed to record the capture lock holder: {error}"))
}

#[cfg(test)]
mod tests {
    use super::CaptureLock;
    use std::path::PathBuf;
    use std::process;

    fn scratch_database(directory: &tempfile::TempDir, name: &str) -> PathBuf {
        directory.path().join(name)
    }

    #[test]
    fn a_second_capture_on_the_same_database_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let db_path = scratch_database(&directory, "daytrace.db");

        let _running = CaptureLock::acquire(&db_path).expect("the first daemon claims capture");
        let error = CaptureLock::acquire(&db_path)
            .expect_err("a second daemon on the same database must be refused");

        assert!(
            error.contains(&format!("pid {}", process::id())),
            "the refusal has to name the process already capturing, or there is nothing to \
             act on: {error}"
        );
    }

    #[test]
    fn capture_can_be_claimed_again_once_the_holder_is_gone() {
        let directory = tempfile::tempdir().expect("tempdir");
        let db_path = scratch_database(&directory, "daytrace.db");

        drop(CaptureLock::acquire(&db_path).expect("the first daemon claims capture"));

        CaptureLock::acquire(&db_path)
            .expect("a released claim must not outlive the process that held it");
    }

    #[test]
    fn two_databases_are_two_independent_claims() {
        let directory = tempfile::tempdir().expect("tempdir");

        let _first = CaptureLock::acquire(&scratch_database(&directory, "one.db"))
            .expect("claim the first database");

        CaptureLock::acquire(&scratch_database(&directory, "two.db"))
            .expect("a daemon on another database is not a duplicate");
    }

    /// A leftover lock file names a pid that no longer holds anything, and a claim that trusted
    /// the contents over the kernel would refuse to start for a process that is long gone.
    #[test]
    fn a_leftover_lock_file_does_not_refuse_a_fresh_daemon() {
        use std::fs;

        let directory = tempfile::tempdir().expect("tempdir");
        let db_path = scratch_database(&directory, "daytrace.db");
        fs::write(format!("{}.lock", db_path.display()), "4242\n").expect("leftover claim");

        let _running = CaptureLock::acquire(&db_path)
            .expect("a lock file nobody holds is not a running daemon");

        let recorded =
            fs::read_to_string(format!("{}.lock", db_path.display())).expect("read the claim back");
        assert_eq!(
            recorded.trim(),
            process::id().to_string(),
            "the claim must name the process holding it now, not the one that left the file"
        );
    }

    /// Starting capture must not be a way to empty a file that happens to sit somewhere else.
    ///
    /// The claim is opened for writing and truncated, so a symlink planted at the lock path used
    /// to be followed: the target was chmodded to 0600, truncated to nothing and given a pid.
    /// Reachable whenever the database sits in a directory something else can write to, which an
    /// explicit `DAYTRACE_DB_PATH` allows and which this project's own examples use.
    #[cfg(unix)]
    #[test]
    fn a_claim_never_follows_a_symlink_planted_at_the_lock_path() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let db_path = scratch_database(&directory, "daytrace.db");
        let target = directory.path().join("not-the-lock");
        fs::write(&target, "contents that must survive\n").expect("write the target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("set target mode");
        std::os::unix::fs::symlink(&target, format!("{}.lock", db_path.display()))
            .expect("plant the symlink");

        CaptureLock::acquire(&db_path).expect_err("a symlinked claim must be refused");

        assert_eq!(
            fs::read_to_string(&target).expect("read the target back"),
            "contents that must survive\n",
            "claiming capture must not truncate the file a symlink pointed at"
        );
        assert_eq!(
            fs::metadata(&target)
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "claiming capture must not change the mode of the file a symlink pointed at"
        );
    }

    /// The claim runs before the store exists, so on a fresh machine it is what creates the data
    /// directory. A start that then fails, which is what a machine with no readable input device
    /// does, never opens the store, so nothing else is left to tighten the directory.
    #[cfg(unix)]
    #[test]
    fn a_data_directory_the_claim_creates_is_private() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let data_dir = directory.path().join("daytrace");

        let _running = CaptureLock::acquire(&data_dir.join("daytrace.db")).expect("claim capture");

        let mode = fs::metadata(&data_dir)
            .expect("data directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "a data directory the daemon creates is private"
        );
    }

    /// The counterpart to the case above: a directory the user already manages is theirs. The
    /// store draws the same line, and a claim that tightened an existing directory would take
    /// an explicit database path as licence to change permissions nobody asked it to.
    #[cfg(unix)]
    #[test]
    fn an_existing_directory_keeps_the_mode_it_had() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))
            .expect("set parent mode");

        let _running = CaptureLock::acquire(&scratch_database(&directory, "daytrace.db"))
            .expect("claim capture");

        let mode = fs::metadata(directory.path())
            .expect("parent metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_private_on_unix() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let db_path = scratch_database(&directory, "daytrace.db");
        let lock_path = format!("{}.lock", db_path.display());
        fs::write(&lock_path, "").expect("a world-readable leftover");
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644))
            .expect("set leftover mode");

        let _running = CaptureLock::acquire(&db_path).expect("claim capture");

        let mode = fs::metadata(&lock_path)
            .expect("lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the lock sits in the data directory");
    }
}
