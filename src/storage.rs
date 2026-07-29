use crate::activity::{ActivityKind, ActivitySnapshot, TimelineSegment};
use rusqlite::{Connection, OptionalExtension, params};
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
    snapshot: ActivitySnapshot,
}

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

    pub fn record_observation(
        &mut self,
        observed_at: i64,
        snapshot: &ActivitySnapshot,
    ) -> Result<(), String> {
        if !snapshot.is_recordable() {
            self.close_open(observed_at)?;
            return Ok(());
        }

        let tx = self
            .conn
            .transaction()
            .map_err(|error| format!("failed to start transaction: {error}"))?;
        let open = load_open_segment(&tx)?;

        match open {
            Some(open) if open.snapshot == *snapshot => {}
            Some(open) => {
                tx.execute(
                    "UPDATE activity_segments SET ended_at = ?1 WHERE id = ?2",
                    params![observed_at, open.id],
                )
                .map_err(|error| format!("failed to close segment: {error}"))?;
                insert_segment(&tx, observed_at, snapshot)?;
            }
            None => insert_segment(&tx, observed_at, snapshot)?,
        }

        tx.commit()
            .map_err(|error| format!("failed to commit observation: {error}"))?;
        self.secure_permissions()
    }

    pub fn close_open(&mut self, ended_at: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE activity_segments SET ended_at = ?1 WHERE ended_at IS NULL",
                params![ended_at],
            )
            .map_err(|error| format!("failed to close open segments: {error}"))?;
        self.secure_permissions()
    }

    pub fn close_stale_open_segments(&mut self) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE activity_segments SET ended_at = started_at WHERE ended_at IS NULL",
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
                "SELECT started_at, COALESCE(ended_at, ?3), kind, app_class, title, workspace, monitor
                 FROM activity_segments
                 WHERE started_at < ?2 AND COALESCE(ended_at, ?3) > ?1
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
                    created_at INTEGER NOT NULL DEFAULT (unixepoch())
                );

                CREATE INDEX IF NOT EXISTS idx_activity_segments_time
                ON activity_segments(started_at, ended_at);
                ",
            )
            .map_err(|error| format!("failed to migrate DB: {error}"))
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
        "SELECT id, kind, app_class, title, workspace, monitor
         FROM activity_segments
         WHERE ended_at IS NULL
         ORDER BY id DESC
         LIMIT 1",
        [],
        |row| {
            Ok(OpenSegment {
                id: row.get(0)?,
                snapshot: ActivitySnapshot {
                    kind: ActivityKind::from_str(&row.get::<_, String>(1)?),
                    app_class: row.get(2)?,
                    title: row.get(3)?,
                    workspace: row.get(4)?,
                    monitor: row.get(5)?,
                },
            })
        },
    )
    .optional()
    .map_err(|error| format!("failed to load open segment: {error}"))
}

fn insert_segment(
    conn: &Connection,
    started_at: i64,
    snapshot: &ActivitySnapshot,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO activity_segments (started_at, kind, app_class, title, workspace, monitor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            started_at,
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

        store.record_observation(100, &ghostty).expect("insert");
        store.record_observation(105, &ghostty).expect("no change");
        store.record_observation(120, &browser).expect("change");
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

        store.record_observation(100, &active).expect("insert");
        store
            .record_observation(110, &ActivitySnapshot::unknown())
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

        store.record_observation(100, &active).expect("insert");
        store
            .close_stale_open_segments()
            .expect("recover stale open segment");

        let rows = store.timeline_between(0, 200, 200).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].started_at, 100);
        assert_eq!(rows[0].ended_at, 100);
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
