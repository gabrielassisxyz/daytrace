//! Whole-binary proof that `daytrace today` renders the narrative, not the raw segments.
//!
//! The unit tests in `src/timeline.rs` pin the exact rendered bytes; this file exists to prove
//! the same shape comes out of the command a user actually runs, built and invoked as a
//! subprocess, rather than only through a function call inside the test binary.

use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 2026-07-20 00:00 in America/Sao_Paulo, a zone with no daylight saving since 2019, so every
/// offset below reads back at a fixed clock time regardless of when this test happens to run.
const MIDNIGHT: i64 = 1_784_516_400;
const ZONE: &str = "America/Sao_Paulo";

struct StoredSegment {
    started_at: i64,
    ended_at: i64,
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

    for segment in segments {
        connection
            .execute(
                "INSERT INTO activity_segments
                    (started_at, ended_at, kind, app_class, title, last_seen_at)
                 VALUES (?1, ?2, 'window', ?3, ?4, ?2)",
                params![
                    segment.started_at,
                    segment.ended_at,
                    segment.app_class,
                    segment.title,
                ],
            )
            .expect("insert segment");
    }
    db_path
}

/// Trigger `Store::open`'s migration, the same way `tests/day_report.rs` does, so a media row can
/// be inserted into the current schema afterward rather than a second, drifting copy of it.
fn migrate_schema(db_path: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .args(["today", "--date", "1970-01-02"])
        .env("DAYTRACE_DB_PATH", db_path)
        .output()
        .expect("run daytrace to trigger migration");
    assert!(
        output.status.success(),
        "the migrating run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn seed_media_row(db_path: &Path, started_at: i64, ended_at: i64, player: &str, title: &str) {
    let connection = Connection::open(db_path).expect("open migrated database");
    connection
        .execute(
            "INSERT INTO activity_segments
                (started_at, ended_at, last_seen_at, kind, app_class, title, lane)
             VALUES (?1, ?2, ?2, 'media', ?3, ?4, ?5)",
            params![
                started_at,
                ended_at,
                player,
                title,
                format!("media:{player}"),
            ],
        )
        .expect("insert media row");
}

fn run_today(db_path: &Path, date: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .args(["today", "--date", date])
        .env("TZ", ZONE)
        .env("DAYTRACE_DB_PATH", db_path)
        .output()
        .expect("run daytrace");
    assert!(
        output.status.success(),
        "daytrace today failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("output is utf-8")
}

/// The bead's own "done when": a video playing behind a focused terminal prints one line naming
/// both, the terminal owns the time (including the three seconds a foreign focus interrupted it
/// for, which the block absorbs rather than reports as its own row), and the printed totals close
/// to the day rather than to the raw segment count.
#[test]
fn a_video_behind_a_focused_terminal_names_both_and_the_terminal_owns_the_time() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[
            StoredSegment {
                started_at: MIDNIGHT,
                ended_at: MIDNIGHT + 600,
                app_class: "kitty",
                title: "editing",
            },
            // A three-second foreign focus, short enough to be swallowed into the terminal
            // block around it rather than reported as its own row.
            StoredSegment {
                started_at: MIDNIGHT + 600,
                ended_at: MIDNIGHT + 603,
                app_class: "rofi",
                title: "quick check",
            },
            StoredSegment {
                started_at: MIDNIGHT + 603,
                ended_at: MIDNIGHT + 1_200,
                app_class: "kitty",
                title: "editing",
            },
        ],
    );
    migrate_schema(&db_path);
    seed_media_row(
        &db_path,
        MIDNIGHT + 100,
        MIDNIGHT + 400,
        "mpv",
        "Some video",
    );

    let rendered = run_today(&db_path, "2026-07-20");

    let block_line = rendered
        .lines()
        .find(|line| line.starts_with("00:00-00:20"))
        .unwrap_or_else(|| panic!("no block line covering the whole 20 minutes: {rendered}"));
    assert!(
        block_line.contains("kitty") && block_line.contains("mpv playing in the background"),
        "the block line must name both the terminal and the video playing behind it: {block_line}"
    );

    assert!(
        rendered.contains("   20m  kitty"),
        "the terminal must be credited the whole twenty minutes, the three seconds a foreign \
         focus interrupted it for included and the video's own five minutes not added on top: \
         {rendered}"
    );
    assert!(
        !rendered.contains("rofi"),
        "a foreign focus swallowed into the block around it must not print a row of its own: \
         {rendered}"
    );
    assert!(
        rendered.contains("Media playing") && rendered.contains("mpv - Some video"),
        "the video still gets its own row in the media section: {rendered}"
    );
}
