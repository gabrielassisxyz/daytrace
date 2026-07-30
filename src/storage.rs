use crate::activity::{ActivityKind, ActivitySnapshot, TimelineSegment};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Store {
    conn: Connection,
    db_path: PathBuf,
    secure_data_dir: Option<PathBuf>,
}

#[derive(Debug)]
struct OpenSegment {
    id: i64,
    started_at: i64,
    snapshot: ActivitySnapshot,
}

/// How far a recovered segment may reach when the daemon never closed it.
///
/// WHY a separate column instead of just writing `ended_at` on every poll: `ended_at IS NULL`
/// is what marks the segment still in progress, so heartbeating into it would close the
/// segment against itself and every later observation would start a new one.
const LAST_SEEN_COLUMN: &str = "last_seen_at";

/// What a prune deleted, and what it could not finish once the deletion had committed.
///
/// The two are reported apart because they fail apart. The rows go in a transaction that either
/// commits or does not; rewriting the file so the deleted activity stops being readable happens
/// after that commit, and can be refused by a reader that is still attached. Answering with a
/// bare failure for the second would say nothing about the first, which has already happened and
/// cannot be undone.
#[derive(Debug)]
pub struct Pruned {
    pub deleted: u64,
    /// Why the deleted activity is still readable in the database file, when it is.
    pub still_in_the_file: Option<String>,
}

/// Which segments a retention window no longer covers: cutoff in `?1`, the present in `?2`.
///
/// A segment has to have ENDED before the cutoff, so one that straddles the boundary is kept
/// whole: trimming it to fit would rewrite a span that was actually observed, and the day it
/// belongs to would then report less time than it lasted.
///
/// The chain is deliberately the one `timeline_between` reads a segment's end through, down to
/// the last fallback, so that nothing can be deleted while a report still shows it. `last_seen_at`
/// is the progress marker every poll advances, so a segment a crash left open ages out by the
/// last moment anyone saw it, and the segment a running daemon is writing can never match because
/// its marker is seconds old. A row stored before that column existed has neither, and both sides
/// resolve it to the present: the report draws it as reaching now, so pruning has to treat it as
/// reaching now too, which keeps it until a daemon start writes it a real end. Reading it as its
/// own start instead would delete a block the same store had just printed as covering today.
const ENDED_BEFORE_CUTOFF: &str = "COALESCE(ended_at, last_seen_at, ?2) < ?1";

impl Store {
    pub fn open(path: impl AsRef<Path>, secure_data_dir: Option<PathBuf>) -> Result<Self, String> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create data directory: {error}"))?;
        }

        let db_path = path.as_ref().to_path_buf();
        let conn =
            Connection::open(&db_path).map_err(|error| format!("failed to open DB: {error}"))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|error| format!("failed to set DB busy timeout: {error}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| format!("failed to enable WAL: {error}"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| format!("failed to set synchronous=NORMAL: {error}"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| format!("failed to enable foreign keys: {error}"))?;

        let store = Self {
            conn,
            db_path,
            secure_data_dir,
        };
        store.migrate()?;
        store.secure_permissions()?;
        Ok(store)
    }

    /// Record what is happening now, as of `starts_at`.
    ///
    /// The two timestamps differ whenever a transition is detected after the fact. Idle is the
    /// case that matters: the daemon only learns the machine went idle once the threshold has
    /// already elapsed, so `starts_at` is when input actually stopped while `seen_at` stays
    /// the current time and keeps the segment's progress moving.
    pub fn record_observation(
        &mut self,
        starts_at: i64,
        seen_at: i64,
        snapshot: &ActivitySnapshot,
    ) -> Result<(), String> {
        if !snapshot.is_recordable() {
            self.close_open(seen_at)?;
            return Ok(());
        }

        // IMMEDIATE, not the default DEFERRED. A deferred transaction reads first and upgrades
        // on the first write, and SQLite answers that upgrade with SQLITE_BUSY_SNAPSHOT
        // immediately, without ever consulting the busy handler, so the busy_timeout set in
        // `open` would not apply and a second daytrace process would fail outright rather than
        // wait its turn.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("failed to start transaction: {error}"))?;
        let open = load_open_segment(&tx)?;

        // A backdated start may not reach behind time that is already accounted for, or the
        // new segment overlaps an existing one and the day adds up to more than it lasted.
        // The floor is the last boundary written, whether that is a closed segment's end or
        // the open segment's start; the open segment's own progress is deliberately excluded,
        // since displacing it is exactly what a backdated start is for.
        let begins_at = match last_accounted_instant(&tx)? {
            Some(floor) => starts_at.max(floor),
            None => starts_at,
        };

        match open {
            // An unchanged snapshot still has to leave a trace: without it a segment that
            // stays in focus for hours carries no evidence of how long it lasted, and a
            // daemon killed before the next window change loses the whole stretch.
            Some(open) if open.snapshot == *snapshot => {
                // `unix_now` is wall clock, not monotonic: an NTP step backwards would
                // otherwise park the progress marker before the segment even started, and
                // recovery would then write an end that precedes the beginning.
                tx.execute(
                    "UPDATE activity_segments SET last_seen_at = ?1 WHERE id = ?2",
                    params![seen_at.max(open.started_at), open.id],
                )
                .map_err(|error| format!("failed to record segment progress: {error}"))?;
            }
            Some(open) => {
                tx.execute(
                    "UPDATE activity_segments SET ended_at = ?1, last_seen_at = ?1 WHERE id = ?2",
                    params![begins_at, open.id],
                )
                .map_err(|error| format!("failed to close segment: {error}"))?;
                insert_segment(&tx, begins_at, seen_at, snapshot)?;
            }
            None => insert_segment(&tx, begins_at, seen_at, snapshot)?,
        }

        tx.commit()
            .map_err(|error| format!("failed to commit observation: {error}"))?;
        self.secure_permissions()
    }

    pub fn close_open(&mut self, ended_at: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE activity_segments SET ended_at = ?1, last_seen_at = ?1 WHERE ended_at IS NULL",
                params![ended_at],
            )
            .map_err(|error| format!("failed to close open segments: {error}"))?;
        self.secure_permissions()
    }

    /// Close whatever the previous run left open, at the last moment it was observed.
    ///
    /// The daemon can die without closing anything: a crash, a reboot, an OOM kill. Falling
    /// back to `started_at` used to discard the entire stretch, so an afternoon spent in one
    /// window disappeared from the timeline. `last_seen_at` bounds the loss to a single poll.
    pub fn close_stale_open_segments(&mut self) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE activity_segments
                 SET ended_at = COALESCE(last_seen_at, started_at)
                 WHERE ended_at IS NULL",
                [],
            )
            .map_err(|error| format!("failed to close stale open segments: {error}"))?;
        self.secure_permissions()
    }

    /// How many stored segments fall outside a window opening at `cutoff`, removing none.
    ///
    /// The count is what makes an irreversible window inspectable before it is applied, so it
    /// has to answer the same question the delete does, from the same predicate.
    pub fn count_segments_ended_before(&self, cutoff: i64, now: i64) -> Result<u64, String> {
        self.conn
            .query_row(
                &format!("SELECT COUNT(*) FROM activity_segments WHERE {ENDED_BEFORE_CUTOFF}"),
                params![cutoff, now],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as u64)
            .map_err(|error| format!("failed to count segments outside the window: {error}"))
    }

    /// Delete every stored segment that ended before `cutoff`.
    pub fn prune_segments_ended_before(&mut self, cutoff: i64, now: i64) -> Result<Pruned, String> {
        // IMMEDIATE, matching `record_observation`, the write this one has to interleave with.
        // A deferred transaction reads first and upgrades on its first write, and SQLite
        // answers that upgrade with SQLITE_BUSY_SNAPSHOT without consulting the busy handler,
        // so pruning beside a running daemon would fail outright rather than wait its turn.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("failed to start transaction: {error}"))?;
        let deleted = tx
            .execute(
                &format!("DELETE FROM activity_segments WHERE {ENDED_BEFORE_CUTOFF}"),
                params![cutoff, now],
            )
            .map_err(|error| format!("failed to delete segments outside the window: {error}"))?;
        tx.commit()
            .map_err(|error| format!("failed to commit the prune: {error}"))?;

        // Past this point the rows are gone for good, so nothing below may be raised as if the
        // deletion had not happened. The rewrite is attempted every time rather than only after
        // a delete, which is what makes running the command again finish an interrupted one.
        let still_in_the_file = self.rewrite_the_file_without_the_deleted_rows().err();
        self.secure_permissions()?;
        Ok(Pruned {
            deleted: deleted as u64,
            still_in_the_file,
        })
    }

    /// Make the deleted activity stop being readable on disk. Two steps, both required.
    fn rewrite_the_file_without_the_deleted_rows(&self) -> Result<(), String> {
        self.rebuild_from_the_rows_that_are_left()?;
        self.copy_the_log_into_the_file()
    }

    /// Rebuild every page of the database from the rows that survived.
    ///
    /// WHY a rebuild rather than the cheaper `PRAGMA secure_delete`, which zeroes a row's bytes
    /// as it frees them: zeroing only reaches what this delete frees. Earlier writes leave stale
    /// copies in the unused tail of pages that are still in use, and a page split copies cells
    /// rather than moving them. Measured on a store filled the way the daemon fills one, a
    /// transaction per observation: of 300 deleted window titles, six were still readable in the
    /// file after a secure delete, in one contiguous run, and none after a rebuild. A retention
    /// policy that leaves an arbitrary sample of what it deleted behind is not one.
    ///
    /// The rebuild also returns the freed space to the filesystem, which zeroing does not.
    fn rebuild_from_the_rows_that_are_left(&self) -> Result<(), String> {
        self.keep_the_rebuild_scratch_beside_the_database()?;
        // Outside any transaction, because SQLite refuses to rebuild inside one.
        self.conn
            .execute_batch("VACUUM")
            .map_err(|error| format!("rebuilding it failed: {error}"))
    }

    /// Point SQLite's scratch file at the database's own directory.
    ///
    /// WHY: the rebuild is written to a temporary database and copied back, and SQLite places
    /// that file wherever its temp-file rules point, which is `/var/tmp` on a Linux desktop. A
    /// full plaintext copy of the activity store therefore lands outside the 0700 directory the
    /// store is deliberately kept in, on a filesystem nobody chose for it, and unlinking it at
    /// the end does not scrub the blocks it used. Keeping it beside the database keeps it inside
    /// the private tree, still created 0600 and still unlinked immediately, and makes the free
    /// space the rebuild needs come from the filesystem the store already lives on.
    ///
    /// A path with no parent, or one that is not valid UTF-8, keeps SQLite's default: this can
    /// only improve on a location it can name.
    fn keep_the_rebuild_scratch_beside_the_database(&self) -> Result<(), String> {
        let Some(directory) = self.db_path.parent().and_then(Path::to_str) else {
            return Ok(());
        };
        if directory.is_empty() {
            return Ok(());
        }

        self.conn
            .pragma_update(None, "temp_store_directory", directory)
            .map_err(|error| format!("failed to place the rebuild scratch file: {error}"))
    }

    /// Copy the log into the database file and reset it to nothing.
    ///
    /// WHY this is part of deleting rather than housekeeping: in WAL mode every write lands in
    /// the log, the rebuild included, and the file keeps the page images that still hold the
    /// removed activity until a checkpoint overwrites them. SQLite's automatic checkpoint is
    /// passive and cannot get through while another connection is attached, which is precisely
    /// the arrangement this tool recommends: a timer beside a daemon that runs all day. Measured
    /// there, a prune without this left the file unchanged with every deleted title still in it
    /// and added a copy of each to the log, so the store came out larger than it went in.
    /// TRUNCATE waits for the readers instead, then resets the log to zero bytes.
    ///
    /// A checkpoint that cannot start answers with `busy = 1` and no error at all, so the flag
    /// is what has to be read; treating the call's success as the rewrite's success would state
    /// a guarantee without having kept it.
    fn copy_the_log_into_the_file(&self) -> Result<(), String> {
        let busy = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|error| format!("rewriting it failed: {error}"))?;

        if busy != 0 {
            return Err("another process is reading it".to_string());
        }
        Ok(())
    }

    pub fn timeline_between(
        &self,
        start: i64,
        end: i64,
        now: i64,
    ) -> Result<Vec<TimelineSegment>, String> {
        let mut stmt = self
            .conn
            .prepare(
                // A segment still open reads as ending at its last observation, and only a row
                // with no observation recorded falls back to the moment of the read. Resolving
                // straight to that moment let a segment the daemon never closed grow without
                // bound: five minutes left open by a crash reported as a full day, on its own
                // day and on every day since, and an export stated that as fact. Recovery at
                // the next daemon start writes the same value, so this only anticipates it.
                "SELECT started_at, COALESCE(ended_at, last_seen_at, ?3), kind, app_class, title, workspace, monitor
                 FROM activity_segments
                 WHERE started_at < ?2 AND COALESCE(ended_at, last_seen_at, ?3) > ?1
                 ORDER BY started_at ASC, id ASC",
            )
            .map_err(|error| format!("failed to prepare timeline query: {error}"))?;

        let rows = stmt
            .query_map(params![start, end, now], |row| {
                let started_at = row.get::<_, i64>(0)?.max(start);
                let ended_at = row.get::<_, i64>(1)?.min(end);
                Ok(TimelineSegment {
                    started_at,
                    ended_at,
                    snapshot: ActivitySnapshot {
                        kind: ActivityKind::from_str(&row.get::<_, String>(2)?),
                        app_class: row.get(3)?,
                        title: row.get(4)?,
                        workspace: row.get(5)?,
                        monitor: row.get(6)?,
                    },
                })
            })
            .map_err(|error| format!("failed to read timeline rows: {error}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("failed to materialize timeline: {error}"))
    }

    fn migrate(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS activity_segments (
                    id INTEGER PRIMARY KEY,
                    started_at INTEGER NOT NULL,
                    ended_at INTEGER,
                    kind TEXT NOT NULL CHECK (kind IN ('window', 'idle', 'unknown')),
                    app_class TEXT,
                    title TEXT,
                    workspace TEXT,
                    monitor INTEGER,
                    last_seen_at INTEGER,
                    created_at INTEGER NOT NULL DEFAULT (unixepoch())
                );

                CREATE INDEX IF NOT EXISTS idx_activity_segments_time
                ON activity_segments(started_at, ended_at);
                ",
            )
            .map_err(|error| format!("failed to migrate DB: {error}"))?;

        self.add_last_seen_column_if_missing()
    }

    /// A database written before segment progress was tracked has no `last_seen_at`. Adding it
    /// is the whole migration: existing rows keep NULL, and the recovery query already falls
    /// back to `started_at` for those, which is the old behaviour for old data.
    fn add_last_seen_column_if_missing(&self) -> Result<(), String> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM pragma_table_info('activity_segments')")
            .map_err(|error| format!("failed to inspect schema: {error}"))?;
        let has_column = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| format!("failed to read schema columns: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("failed to materialize schema columns: {error}"))?
            .iter()
            .any(|name| name == LAST_SEEN_COLUMN);

        if has_column {
            return Ok(());
        }

        match self.conn.execute(
            "ALTER TABLE activity_segments ADD COLUMN last_seen_at INTEGER",
            [],
        ) {
            Ok(_) => Ok(()),
            // Two processes can both read the pre-migration schema and both try to add the
            // column. The loser's ALTER is redundant, not a failure, and treating it as one
            // would turn the first run after an upgrade into a hard startup error.
            Err(error) if error.to_string().contains("duplicate column name") => Ok(()),
            Err(error) => Err(format!("failed to add {LAST_SEEN_COLUMN} column: {error}")),
        }
    }

    fn secure_permissions(&self) -> Result<(), String> {
        secure_sqlite_permissions(&self.db_path, self.secure_data_dir.as_deref())
    }
}

#[cfg(unix)]
fn secure_sqlite_permissions(db_path: &Path, secure_data_dir: Option<&Path>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = secure_data_dir
        && parent.exists()
    {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("failed to secure data directory permissions: {error}"))?;
    }

    for path in sqlite_artifact_paths(db_path) {
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("failed to secure SQLite file permissions: {error}"))?;
        }
    }

    Ok(())
}

#[cfg(not(unix))]
fn secure_sqlite_permissions(
    _db_path: &Path,
    _secure_data_dir: Option<&Path>,
) -> Result<(), String> {
    Ok(())
}

fn sqlite_artifact_paths(db_path: &Path) -> [PathBuf; 3] {
    [
        db_path.to_path_buf(),
        PathBuf::from(format!("{}-wal", db_path.display())),
        PathBuf::from(format!("{}-shm", db_path.display())),
    ]
}

fn load_open_segment(conn: &Connection) -> Result<Option<OpenSegment>, String> {
    conn.query_row(
        "SELECT id, started_at, kind, app_class, title, workspace, monitor
         FROM activity_segments
         WHERE ended_at IS NULL
         ORDER BY id DESC
         LIMIT 1",
        [],
        |row| {
            Ok(OpenSegment {
                id: row.get(0)?,
                started_at: row.get(1)?,
                snapshot: ActivitySnapshot {
                    kind: ActivityKind::from_str(&row.get::<_, String>(2)?),
                    app_class: row.get(3)?,
                    title: row.get(4)?,
                    workspace: row.get(5)?,
                    monitor: row.get(6)?,
                },
            })
        },
    )
    .optional()
    .map_err(|error| format!("failed to load open segment: {error}"))
}

/// The latest instant the timeline already accounts for, or `None` on an empty store.
///
/// `COALESCE(ended_at, started_at)` deliberately reads the open segment as its start rather
/// than its progress: a segment still running is the one a backdated transition is entitled
/// to cut short, while everything already closed is settled.
fn last_accounted_instant(conn: &Connection) -> Result<Option<i64>, String> {
    conn.query_row(
        "SELECT MAX(COALESCE(ended_at, started_at)) FROM activity_segments",
        [],
        |row| row.get::<_, Option<i64>>(0),
    )
    .map_err(|error| format!("failed to read the last accounted instant: {error}"))
}

fn insert_segment(
    conn: &Connection,
    started_at: i64,
    last_seen_at: i64,
    snapshot: &ActivitySnapshot,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO activity_segments
            (started_at, last_seen_at, kind, app_class, title, workspace, monitor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            started_at,
            last_seen_at.max(started_at),
            snapshot.kind.as_str(),
            snapshot.app_class,
            snapshot.title,
            snapshot.workspace,
            snapshot.monitor,
        ],
    )
    .map_err(|error| format!("failed to insert activity segment: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Store;
    use crate::activity::{ActivitySnapshot, TimelineSegment};
    use rusqlite::Connection;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[test]
    fn records_only_changes_as_segments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(db, None).expect("store");
        let ghostty = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            Some("3".to_string()),
            Some(1),
        );
        let browser = ActivitySnapshot::window(
            Some("firefox".to_string()),
            Some("Docs".to_string()),
            Some("2".to_string()),
            Some(0),
        );

        store
            .record_observation(100, 100, &ghostty)
            .expect("insert");
        store
            .record_observation(105, 105, &ghostty)
            .expect("no change");
        store
            .record_observation(120, 120, &browser)
            .expect("change");
        store.close_open(150).expect("close");

        let rows = store.timeline_between(0, 200, 200).expect("timeline");
        assert_eq!(
            rows,
            vec![
                TimelineSegment {
                    started_at: 100,
                    ended_at: 120,
                    snapshot: ghostty,
                },
                TimelineSegment {
                    started_at: 120,
                    ended_at: 150,
                    snapshot: browser,
                },
            ]
        );
    }

    #[test]
    fn unknown_observation_closes_open_segment_without_recording_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(db, None).expect("store");
        let active = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );

        store.record_observation(100, 100, &active).expect("insert");
        store
            .record_observation(110, 110, &ActivitySnapshot::unknown())
            .expect("close");

        let rows = store.timeline_between(0, 200, 200).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ended_at, 110);
    }

    #[test]
    fn startup_recovery_does_not_extend_stale_open_segments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(db, None).expect("store");
        let active = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );

        store.record_observation(100, 100, &active).expect("insert");
        store
            .close_stale_open_segments()
            .expect("recover stale open segment");

        let rows = store.timeline_between(0, 200, 200).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].started_at, 100);
        assert_eq!(rows[0].ended_at, 100);
    }

    #[test]
    fn recovery_keeps_time_elapsed_before_an_unclean_shutdown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(&db, None).expect("store");
        let active = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );

        // One long stretch in a single window: the daemon keeps observing the same snapshot
        // and never gets to close the segment, which is what a crash or reboot looks like.
        store.record_observation(100, 100, &active).expect("insert");
        for observed_at in [200, 300, 400] {
            store
                .record_observation(observed_at, observed_at, &active)
                .expect("unchanged observation");
        }

        let mut recovered = Store::open(&db, None).expect("reopen after crash");
        recovered
            .close_stale_open_segments()
            .expect("recover open segment");

        let rows = recovered.timeline_between(0, 500, 500).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].started_at, 100);
        assert_eq!(rows[0].ended_at, 400);
    }

    #[test]
    fn recovery_reads_databases_written_before_progress_tracking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");

        // A schema exactly as it shipped before last_seen_at existed.
        let legacy = rusqlite::Connection::open(&db).expect("legacy connection");
        legacy
            .execute_batch(
                "CREATE TABLE activity_segments (
                    id INTEGER PRIMARY KEY,
                    started_at INTEGER NOT NULL,
                    ended_at INTEGER,
                    kind TEXT NOT NULL CHECK (kind IN ('window', 'idle', 'unknown')),
                    app_class TEXT,
                    title TEXT,
                    workspace TEXT,
                    monitor INTEGER,
                    created_at INTEGER NOT NULL DEFAULT (unixepoch())
                );
                INSERT INTO activity_segments (started_at, kind, app_class)
                VALUES (100, 'window', 'ghostty');",
            )
            .expect("seed legacy rows");
        drop(legacy);

        let mut store = Store::open(&db, None).expect("migrate legacy database");
        store
            .close_stale_open_segments()
            .expect("recover legacy open segment");

        let rows = store.timeline_between(0, 500, 500).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ended_at, 100);
    }

    #[test]
    fn an_unclosed_segment_stops_at_its_last_observation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let active = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );

        // A daemon killed without a chance to close leaves `ended_at` NULL until the next
        // start recovers it. Reading the timeline used to resolve that against the moment of
        // the read, so five minutes left open reported as everything since.
        store.record_observation(100, 100, &active).expect("insert");
        store
            .record_observation(150, 150, &active)
            .expect("unchanged observation");

        let rows = store.timeline_between(0, 10_000, 10_000).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].ended_at, 150,
            "an unclosed segment may not claim time nobody observed"
        );
    }

    #[test]
    fn a_later_day_does_not_inherit_an_unclosed_segment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let active = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );

        store.record_observation(100, 150, &active).expect("insert");

        let rows = store
            .timeline_between(1_000, 2_000, 10_000)
            .expect("timeline");
        assert!(
            rows.is_empty(),
            "a segment last seen at 150 cannot appear in a window that opens at 1000: {rows:?}"
        );
    }

    #[test]
    fn a_row_written_before_progress_tracking_still_reaches_the_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let legacy = rusqlite::Connection::open(&db).expect("legacy connection");
        legacy
            .execute_batch(
                "CREATE TABLE activity_segments (
                    id INTEGER PRIMARY KEY,
                    started_at INTEGER NOT NULL,
                    ended_at INTEGER,
                    kind TEXT NOT NULL CHECK (kind IN ('window', 'idle', 'unknown')),
                    app_class TEXT,
                    title TEXT,
                    workspace TEXT,
                    monitor INTEGER,
                    created_at INTEGER NOT NULL DEFAULT (unixepoch())
                );
                INSERT INTO activity_segments (started_at, kind, app_class)
                VALUES (100, 'window', 'ghostty');",
            )
            .expect("seed legacy rows");
        drop(legacy);

        // Such a row has no observation to fall back to, so the old reading is the only one
        // available and stays in place rather than collapsing the row to nothing.
        let store = Store::open(&db, None).expect("migrate legacy database");

        let rows = store.timeline_between(0, 10_000, 500).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ended_at, 500);
    }

    /// How many times `payload` appears in the bytes of `path`, which need not exist.
    ///
    /// Deliberately not a pragma. `page_count` and `freelist_count` describe what the database
    /// uses, and cleartext sitting in a page the database has stopped using is precisely what a
    /// deletion has to remove, so only the file on disk can answer this. Reading it through the
    /// same connection that performed the delete answers a third question again, since that
    /// connection sees the write-ahead log the rest of the world has not been given yet.
    fn occurrences_in(path: &Path, payload: &str) -> usize {
        let Ok(bytes) = fs::read(path) else {
            return 0;
        };
        bytes
            .windows(payload.len())
            .filter(|window| *window == payload.as_bytes())
            .count()
    }

    /// The same count over both files a store is kept in.
    ///
    /// The guarantee is about what is readable on disk, and until a checkpoint runs a write lives
    /// in the log rather than in the database file, so measuring only one of the two would call a
    /// prune clean whenever the copies had simply not been moved yet.
    fn occurrences_on_disk(db_path: &Path, payload: &str) -> usize {
        occurrences_in(db_path, payload) + occurrences_in(&wal_of(db_path), payload)
    }

    fn byte_len(path: &Path) -> u64 {
        fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
    }

    fn wal_of(db_path: &Path) -> PathBuf {
        PathBuf::from(format!("{}-wal", db_path.display()))
    }

    /// Fill the store with `count` segments, ten seconds apart, whose titles carry `payload`.
    fn seed_segments(store: &mut Store, count: i64, payload: &str, first_at: i64) {
        for index in 0..count {
            let at = first_at + index * 10;
            store
                .record_observation(
                    at,
                    at,
                    &ActivitySnapshot::window(
                        Some(format!("app-{index}")),
                        Some(format!("{payload}-{index}")),
                        None,
                        None,
                    ),
                )
                .expect("record");
        }
        store.close_open(first_at + count * 10).expect("close");
    }

    fn window_snapshot(app_class: &str) -> ActivitySnapshot {
        ActivitySnapshot::window(
            Some(app_class.to_string()),
            Some("a window".to_string()),
            None,
            None,
        )
    }

    #[test]
    fn pruning_removes_only_the_segments_that_ended_before_the_cutoff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let outside = window_snapshot("outside-the-window");
        let inside = window_snapshot("inside-the-window");

        store
            .record_observation(100, 100, &outside)
            .expect("old segment");
        store.close_open(200).expect("close the old segment");
        store
            .record_observation(1_000, 1_000, &inside)
            .expect("recent segment");
        store.close_open(1_100).expect("close the recent segment");

        let pruned = store
            .prune_segments_ended_before(500, 2_000)
            .expect("prune the old segment");

        assert_eq!(pruned.deleted, 1);
        let rows = store.timeline_between(0, 2_000, 2_000).expect("timeline");
        assert_eq!(rows.len(), 1, "only the segment inside the window survives");
        assert_eq!(rows[0].snapshot, inside);
    }

    #[test]
    fn a_segment_straddling_the_cutoff_is_kept_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");

        store
            .record_observation(400, 400, &window_snapshot("across-the-boundary"))
            .expect("record");
        store.close_open(600).expect("close");

        let pruned = store
            .prune_segments_ended_before(500, 1_000)
            .expect("prune");

        assert_eq!(pruned.deleted, 0);
        let rows = store.timeline_between(0, 1_000, 1_000).expect("timeline");
        assert_eq!(
            (rows.len(), rows[0].started_at, rows[0].ended_at),
            (1, 400, 600),
            "a segment is removed whole or kept whole: trimming it to the window would rewrite \
             a span that was actually observed"
        );
    }

    #[test]
    fn the_segment_capture_is_still_writing_cannot_be_pruned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let active = window_snapshot("still-in-focus");

        // What a running daemon leaves in the store: no `ended_at` yet, and a progress marker
        // that every poll moves forward. Pruning while capture runs must not reach it.
        store.record_observation(100, 100, &active).expect("insert");
        store
            .record_observation(1_000, 1_000, &active)
            .expect("unchanged observation");

        let pruned = store
            .prune_segments_ended_before(500, 2_000)
            .expect("prune");

        assert_eq!(
            pruned.deleted, 0,
            "an unclosed segment last observed after the cutoff is still in progress"
        );
        let rows = store.timeline_between(0, 2_000, 2_000).expect("timeline");
        assert_eq!((rows.len(), rows[0].started_at), (1, 100));
    }

    #[test]
    fn an_unclosed_segment_last_observed_before_the_cutoff_is_pruned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");

        // A segment a crash left open months ago. It has no end, so only the moment it was
        // last observed says whether it belongs to the window.
        store
            .record_observation(100, 100, &window_snapshot("crashed-app"))
            .expect("insert");

        let pruned = store
            .prune_segments_ended_before(500, 2_000)
            .expect("prune");

        assert_eq!(pruned.deleted, 1);
        assert!(
            store
                .timeline_between(0, 2_000, 2_000)
                .expect("timeline")
                .is_empty(),
            "a segment nobody closed still ages out, or a crash exempts it forever"
        );
    }

    #[test]
    fn counting_what_a_window_would_remove_deletes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");

        store
            .record_observation(100, 100, &window_snapshot("outside-the-window"))
            .expect("old segment");
        store.close_open(200).expect("close the old segment");
        store
            .record_observation(1_000, 1_000, &window_snapshot("inside-the-window"))
            .expect("recent segment");
        store.close_open(1_100).expect("close the recent segment");

        let removable = store
            .count_segments_ended_before(500, 2_000)
            .expect("count");

        assert_eq!(removable, 1);
        assert_eq!(
            store
                .timeline_between(0, 2_000, 2_000)
                .expect("timeline")
                .len(),
            2,
            "counting is what makes the window inspectable before an irreversible delete, so \
             it must not perform one"
        );
    }

    #[test]
    fn a_row_the_report_still_shows_is_never_pruned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let legacy = rusqlite::Connection::open(&db).expect("legacy connection");
        legacy
            .execute_batch(
                "CREATE TABLE activity_segments (
                    id INTEGER PRIMARY KEY,
                    started_at INTEGER NOT NULL,
                    ended_at INTEGER,
                    kind TEXT NOT NULL CHECK (kind IN ('window', 'idle', 'unknown')),
                    app_class TEXT,
                    title TEXT,
                    workspace TEXT,
                    monitor INTEGER,
                    created_at INTEGER NOT NULL DEFAULT (unixepoch())
                );
                INSERT INTO activity_segments (started_at, kind, app_class)
                VALUES (100, 'window', 'ghostty');",
            )
            .expect("seed legacy rows");
        drop(legacy);

        // A row with neither an end nor a progress marker is drawn by `timeline_between` as
        // reaching the present, so the window has to read it the same way. The two chains
        // disagreeing on this last fallback meant a store written before the column existed
        // reported a block covering today and then had it deleted as months old.
        let mut store = Store::open(&db, None).expect("migrate legacy database");
        let shown = store.timeline_between(0, 2_000, 2_000).expect("timeline");
        let pruned = store
            .prune_segments_ended_before(500, 2_000)
            .expect("prune");

        assert_eq!(shown.len(), 1, "the report shows the row");
        assert_eq!(
            pruned.deleted, 0,
            "nothing a report still shows inside the window may be deleted by the window; one \
             daemon start writes such a row a real end, after which it ages out normally"
        );
    }

    #[test]
    fn a_plain_delete_leaves_the_activity_readable_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(&db, None).expect("store");
        seed_segments(&mut store, 300, "expired-window-title", 0);

        // The premise every case below rests on, measured rather than assumed: removing a row
        // unlinks it and leaves its bytes in the page it freed. Without this, an assertion that
        // finds nothing could be green against a build where there was never anything to find.
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "secure_delete", |row| row.get::<_, i64>(0))
                .expect("secure_delete"),
            0,
            "this build deletes ordinarily, which is what pruning has to do more than"
        );
        store
            .conn
            .execute("DELETE FROM activity_segments", [])
            .expect("delete every row");
        store
            .copy_the_log_into_the_file()
            .expect("put the delete in the file");

        assert!(
            occurrences_on_disk(&db, "expired-window-title") > 0,
            "a plain delete has to leave the titles behind, or the measurements below prove \
             nothing"
        );
    }

    #[test]
    fn pruning_leaves_none_of_the_deleted_activity_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(&db, None).expect("store");
        seed_segments(&mut store, 300, "expired-window-title", 0);
        seed_segments(&mut store, 3, "surviving-window-title", 100_000);

        let pruned = store
            .prune_segments_ended_before(50_000, 200_000)
            .expect("prune");

        assert_eq!(pruned.deleted, 300);
        assert_eq!(
            pruned.still_in_the_file, None,
            "nothing was holding the store, so the rewrite had to complete"
        );
        assert_eq!(
            occurrences_on_disk(&db, "expired-window-title"),
            0,
            "a window title the retention window deleted must not be readable in the file or \
             in the log"
        );
        assert_eq!(
            byte_len(&wal_of(&db)),
            0,
            "the log is reset rather than left holding copies of what was removed"
        );
        assert!(
            occurrences_on_disk(&db, "surviving-window-title") > 0,
            "the measurement has to be able to find a title that is still stored, or it is \
             reading the wrong bytes"
        );
    }

    #[test]
    fn pruning_beside_an_attached_reader_still_clears_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(&db, None).expect("store");
        seed_segments(&mut store, 300, "expired-window-title", 0);

        // The arrangement this tool recommends: a second connection attached for the whole
        // prune, as the capture daemon is while a timer runs. SQLite's own passive checkpoint
        // cannot get through here, so a prune that relied on one reported success while leaving
        // every deleted title in the file and adding a copy of each to the log.
        let daemon = Connection::open(&db).expect("a second connection on the same store");
        let seen: i64 = daemon
            .query_row("SELECT COUNT(*) FROM activity_segments", [], |row| {
                row.get(0)
            })
            .expect("the second connection can read");
        assert_eq!(seen, 300, "the second connection is really attached");

        let pruned = store
            .prune_segments_ended_before(50_000, 200_000)
            .expect("prune");

        assert_eq!(pruned.deleted, 300);
        assert_eq!(
            pruned.still_in_the_file, None,
            "a reader that is up to date must not stop the rewrite"
        );
        assert_eq!(
            occurrences_on_disk(&db, "expired-window-title"),
            0,
            "the configuration the README recommends is the one that has to hold"
        );
        assert_eq!(byte_len(&wal_of(&db)), 0);
        drop(daemon);
    }

    #[test]
    fn a_reader_holding_an_older_snapshot_is_reported_rather_than_hidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(&db, None).expect("store");
        seed_segments(&mut store, 300, "expired-window-title", 0);

        let reader = Connection::open(&db).expect("a second connection");
        reader.execute_batch("BEGIN").expect("open a read");
        let _: i64 = reader
            .query_row("SELECT COUNT(*) FROM activity_segments", [], |row| {
                row.get(0)
            })
            .expect("take a snapshot and hold it");
        // Without this the checkpoint would sit on the busy handler for the five seconds the
        // store is configured to wait before giving the same answer, since this reader is never
        // going to move.
        store
            .conn
            .busy_timeout(Duration::from_millis(50))
            .expect("shorten the wait");

        let pruned = store
            .prune_segments_ended_before(50_000, 200_000)
            .expect("the delete itself still has to succeed");

        assert_eq!(
            pruned.deleted, 300,
            "the rows are gone whatever the file still holds, and reporting a failure here \
             would describe a committed deletion as one that did not happen"
        );
        let reason = pruned
            .still_in_the_file
            .expect("a rewrite that could not run has to be reported");
        assert!(
            reason.contains("reading"),
            "the reason has to name what stopped it: {reason}"
        );
        assert!(
            occurrences_on_disk(&db, "expired-window-title") > 0,
            "the warning has to be true: this is the case where the deleted activity really is \
             still readable on disk"
        );

        // And the advice that goes with it has to work: once the reader lets go, running the
        // command again finishes the rewrite, even though it now deletes nothing.
        drop(reader);
        let again = store
            .prune_segments_ended_before(50_000, 200_000)
            .expect("prune again");

        assert_eq!(again.deleted, 0);
        assert_eq!(again.still_in_the_file, None);
        assert_eq!(occurrences_on_disk(&db, "expired-window-title"), 0);
    }

    #[test]
    fn pruning_gives_the_space_back_and_a_later_day_reuses_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(&db, None).expect("store");
        seed_segments(&mut store, 1_000, "expired-window-title", 0);
        // Measured after a checkpoint in every case, since until then the writes are in the log
        // and the file says nothing about how much is stored.
        store.copy_the_log_into_the_file().expect("checkpoint");
        let before_pruning = byte_len(&db);

        store
            .prune_segments_ended_before(50_000, 200_000)
            .expect("prune");
        let after_pruning = byte_len(&db);
        seed_segments(&mut store, 1_000, "later-window-title", 200_000);
        store.copy_the_log_into_the_file().expect("checkpoint");

        assert!(
            after_pruning < before_pruning,
            "pruning has to return the space, not merely stop using it: {before_pruning} bytes \
             before, {after_pruning} after"
        );
        assert!(
            byte_len(&db) <= before_pruning,
            "a day recorded after a prune reuses what the prune gave back, so a store pruned \
             and refilled forever stays the size of its window"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_rebuild_scratch_file_stays_beside_the_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace.db");
        let mut store = Store::open(&db, None).expect("store");
        seed_segments(&mut store, 50, "expired-window-title", 0);

        store
            .prune_segments_ended_before(50_000, 200_000)
            .expect("prune");

        // The scratch database is unlinked as soon as the rebuild ends, so what can be pinned
        // here is where SQLite was told to put it. That it honours the setting was measured by
        // hand: the file appears as `<dir>/etilqs_<random>`, mode 0600, once the store is large
        // enough to spill out of memory.
        let told: String = store
            .conn
            .pragma_query_value(None, "temp_store_directory", |row| row.get(0))
            .expect("temp_store_directory");
        assert_eq!(
            told,
            dir.path().to_str().expect("a utf-8 path"),
            "a plaintext copy of the store must not be written outside the directory the store \
             is kept private in"
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_files_are_private_on_unix() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("daytrace/daytrace.db");
        let data_dir = dir.path().join("daytrace");
        let mut store = Store::open(&db, Some(data_dir.clone())).expect("store");
        store
            .record_observation(
                100,
                100,
                &ActivitySnapshot::window(
                    Some("ghostty".to_string()),
                    Some("tmux".to_string()),
                    None,
                    None,
                ),
            )
            .expect("write");

        let dir_mode = fs::metadata(data_dir)
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        let db_mode = fs::metadata(&db).expect("db metadata").permissions().mode() & 0o777;

        assert_eq!(dir_mode, 0o700);
        assert_eq!(db_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_database_parent_permissions_are_not_changed() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
            .expect("set parent mode");
        let db = dir.path().join("custom.db");

        Store::open(&db, None).expect("store");

        let parent_mode = fs::metadata(dir.path())
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        let db_mode = fs::metadata(&db).expect("db metadata").permissions().mode() & 0o777;

        assert_eq!(parent_mode, 0o755);
        assert_eq!(db_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_database_parent_named_daytrace_is_not_changed() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let parent = dir.path().join("daytrace");
        fs::create_dir(&parent).expect("create parent");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).expect("set parent mode");
        let db = parent.join("custom.db");

        Store::open(&db, None).expect("store");

        let parent_mode = fs::metadata(parent)
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        let db_mode = fs::metadata(&db).expect("db metadata").permissions().mode() & 0o777;

        assert_eq!(parent_mode, 0o755);
        assert_eq!(db_mode, 0o600);
    }
}
