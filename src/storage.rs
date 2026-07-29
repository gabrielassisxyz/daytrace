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
