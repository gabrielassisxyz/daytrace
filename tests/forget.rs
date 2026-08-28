//! Whole-binary exercises for `daytrace forget`.
//!
//! `forget` is the counterpart to `prune`: it deletes by content instead of by date, so these
//! cases seed rows by their app class and title rather than by age and check the same command
//! surface `retention.rs` already checks for `prune`: dry run, a real deletion, and a pattern
//! that matches nothing.

use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A segment as stored, named after the text a pattern is meant to find.
struct StoredSegment {
    app_class: &'static str,
    title: &'static str,
}

fn seeded_database(directory: &Path, segments: &[StoredSegment]) -> PathBuf {
    let db_path = directory.join("daytrace.db");
    let connection = Connection::open(&db_path).expect("open scratch database");
    connection
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
                last_seen_at INTEGER,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .expect("create schema");

    for (index, segment) in segments.iter().enumerate() {
        let started_at = (index as i64) * 100;
        connection
            .execute(
                "INSERT INTO activity_segments
                    (started_at, ended_at, kind, app_class, title, last_seen_at)
                 VALUES (?1, ?2, 'window', ?3, ?4, ?2)",
                params![
                    started_at,
                    started_at + 10,
                    segment.app_class,
                    segment.title,
                ],
            )
            .expect("insert segment");
    }
    db_path
}

fn run_daytrace(db_path: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .args(args)
        .env(
            "DAYTRACE_DB_PATH",
            db_path.as_os_str().to_str().expect("a utf-8 path"),
        )
        .output()
        .expect("run daytrace");
    assert!(
        output.status.success(),
        "daytrace {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("output is utf-8")
}

fn surviving_app_classes(db_path: &Path) -> Vec<String> {
    let connection = Connection::open(db_path).expect("open scratch database");
    let mut statement = connection
        .prepare("SELECT app_class FROM activity_segments ORDER BY started_at ASC")
        .expect("prepare");
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query");
    rows.collect::<Result<Vec<_>, _>>().expect("materialize")
}

#[test]
fn forgetting_deletes_the_rows_a_pattern_matches_and_keeps_the_rest() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[
            StoredSegment {
                app_class: "org.keepassxc.KeePassXC",
                title: "Passwords",
            },
            StoredSegment {
                app_class: "com.mitchellh.ghostty",
                title: "tmux",
            },
        ],
    );

    let report = run_daytrace(&db_path, &["forget", "--matching", "keepassxc"]);

    assert!(
        report.contains("Deleted 1 activity segment"),
        "the command has to say how much it deleted: {report}"
    );
    assert_eq!(
        surviving_app_classes(&db_path),
        vec!["com.mitchellh.ghostty"],
        "the matched row goes and nothing else does"
    );
}

#[test]
fn a_dry_run_reports_the_match_count_and_deletes_nothing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[
            StoredSegment {
                app_class: "signal",
                title: "Private chat",
            },
            StoredSegment {
                app_class: "ghostty",
                title: "tmux",
            },
        ],
    );

    let preview = run_daytrace(&db_path, &["forget", "--matching", "private", "--dry-run"]);

    assert!(
        preview.contains("1 activity segment is matched by it")
            && preview.contains("Nothing was deleted"),
        "a preview of an irreversible command must state both the count and its own \
         harmlessness: {preview}"
    );
    assert_eq!(
        surviving_app_classes(&db_path).len(),
        2,
        "a dry run that removes a row is not a dry run"
    );
}

#[test]
fn a_pattern_matching_nothing_says_so_rather_than_failing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[StoredSegment {
            app_class: "ghostty",
            title: "tmux",
        }],
    );

    let report = run_daytrace(
        &db_path,
        &["forget", "--matching", "nothing-stored-has-this"],
    );

    assert!(
        report.contains("Deleted 0 activity segments"),
        "a pattern matching nothing has to say so, not fail: {report}"
    );
    assert_eq!(surviving_app_classes(&db_path).len(), 1);
}

#[test]
fn forgetting_on_a_machine_with_no_stored_activity_creates_no_database() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = directory.path().join("never-recorded.db");

    let report = run_daytrace(&db_path, &["forget", "--matching", "anything"]);

    assert!(
        report.contains("No stored activity was found"),
        "a machine that never captured anything gets an answer, not a failure: {report}"
    );
    assert!(
        !db_path.exists(),
        "a command whose job is to remove activity must not be the thing that creates the store"
    );
}
