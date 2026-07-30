//! Whole-binary exercises for the day a report answers for.
//!
//! These live outside the unit tests for one reason: the timezone. A process reads its zone
//! once and caches it, so a test that changes `TZ` in place either affects every other test
//! in the same process or does nothing at all. Handing the zone to a child process is the
//! only way to pin the behaviour that matters most here, which is where a day begins and
//! ends when a clock change moves or repeats its first hour.

use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A segment as stored.
///
/// `ended_at` is missing for a segment the daemon never closed, which is what a crash leaves
/// behind; `last_seen_at` is the progress marker the daemon advances on every observation, so
/// the two together describe how much of the segment was actually seen.
struct StoredSegment {
    started_at: i64,
    ended_at: Option<i64>,
    last_seen_at: i64,
    app_class: &'static str,
}

/// A scratch database holding exactly the segments a case needs.
///
/// The schema is created here rather than by the daemon, since the daemon would have to run
/// against a live compositor to write anything, and the point of these cases is to control
/// the instants precisely.
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
                 VALUES (?1, ?2, 'window', ?3, 'a window', ?4)",
                params![
                    segment.started_at,
                    segment.ended_at,
                    segment.app_class,
                    segment.last_seen_at,
                ],
            )
            .expect("insert segment");
    }
    db_path
}

fn run_in_zone(db_path: &Path, timezone: &str, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .args(args)
        .env("TZ", timezone)
        .env("DAYTRACE_DB_PATH", db_path)
        .output()
        .expect("run daytrace");

    assert!(
        output.status.success(),
        "daytrace {args:?} failed in {timezone}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("output is utf-8")
}

#[test]
fn activity_in_a_repeated_hour_belongs_to_the_day_it_happened_on() {
    // Havana ends daylight saving at 01:00 on 2026-11-01, so local 00:00 to 00:59 happens
    // twice that night. This instant is the first pass, half an hour into November 1st.
    const FIRST_PASS_OF_HALF_PAST_MIDNIGHT: i64 = 1_793_507_400;
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[StoredSegment {
            started_at: FIRST_PASS_OF_HALF_PAST_MIDNIGHT,
            ended_at: Some(FIRST_PASS_OF_HALF_PAST_MIDNIGHT + 600),
            last_seen_at: FIRST_PASS_OF_HALF_PAST_MIDNIGHT + 600,
            app_class: "ghostty",
        }],
    );

    let november = run_in_zone(
        &db_path,
        "America/Havana",
        &["today", "--date", "2026-11-01"],
    );
    let october = run_in_zone(
        &db_path,
        "America/Havana",
        &["today", "--date", "2026-10-31"],
    );

    assert!(
        november.contains("ghostty"),
        "the day the activity happened on must report it: {november}"
    );
    assert!(
        !october.contains("ghostty"),
        "the previous day must not swallow the first of two passes through midnight: {october}"
    );
}

#[test]
fn a_day_whose_local_midnight_never_happened_is_still_reported() {
    // Santiago moves the clock forward at midnight, so on these dates local 00:00 does not
    // exist at all. Refusing to name the start of the day used to refuse the report itself,
    // and for the day before as well, whose end is this day's start.
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(directory.path(), &[]);

    for date in ["2038-09-04", "2038-09-05"] {
        let report = run_in_zone(&db_path, "America/Santiago", &["today", "--date", date]);
        assert!(
            report.contains(date),
            "{date} must be reported rather than refused: {report}"
        );
    }
}

#[test]
fn a_report_answers_for_the_requested_day_and_not_for_another() {
    // 2026-07-20 09:00 and 2026-07-21 09:00, in a zone with a fixed offset.
    const MONDAY_MORNING: i64 = 1_784_557_200;
    const TUESDAY_MORNING: i64 = 1_784_643_600;
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[
            StoredSegment {
                started_at: MONDAY_MORNING,
                ended_at: Some(MONDAY_MORNING + 1800),
                last_seen_at: MONDAY_MORNING + 1800,
                app_class: "monday-app",
            },
            StoredSegment {
                started_at: TUESDAY_MORNING,
                ended_at: Some(TUESDAY_MORNING + 1800),
                last_seen_at: TUESDAY_MORNING + 1800,
                app_class: "tuesday-app",
            },
        ],
    );

    let monday = run_in_zone(
        &db_path,
        "America/Sao_Paulo",
        &["today", "--date", "2026-07-20"],
    );

    assert!(monday.contains("monday-app"), "{monday}");
    assert!(
        !monday.contains("tuesday-app"),
        "a day must not report a neighbour's activity: {monday}"
    );
    assert!(
        monday.contains("Time per application"),
        "the totals belong to the requested day too: {monday}"
    );
}

#[test]
fn an_export_answers_for_the_requested_day() {
    const MONDAY_MORNING: i64 = 1_784_557_200;
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[StoredSegment {
            started_at: MONDAY_MORNING,
            ended_at: Some(MONDAY_MORNING + 1800),
            last_seen_at: MONDAY_MORNING + 1800,
            app_class: "monday-app",
        }],
    );

    let exported = run_in_zone(
        &db_path,
        "America/Sao_Paulo",
        &["export", "--date", "2026-07-20"],
    );
    let value: serde_json::Value = serde_json::from_str(&exported).expect("valid JSON");

    assert_eq!(value["date"], "2026-07-20");
    assert_eq!(value["segments"][0]["app_class"], "monday-app");
    assert_eq!(value["segments"][0]["duration_seconds"], 1800);
}

#[test]
fn a_segment_covering_a_whole_day_does_not_begin_and_end_at_the_same_time() {
    // 2026-07-20 and 2026-07-21 local midnight, in a zone with a fixed offset.
    const MONDAY: i64 = 1_784_516_400;
    const TUESDAY: i64 = 1_784_602_800;
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[StoredSegment {
            started_at: MONDAY,
            ended_at: Some(TUESDAY),
            last_seen_at: TUESDAY,
            app_class: "all-day-app",
        }],
    );

    let monday = run_in_zone(
        &db_path,
        "America/Sao_Paulo",
        &["today", "--date", "2026-07-20"],
    );

    assert!(
        monday.contains("00:00-24:00"),
        "a segment holding the whole day cannot read as starting and ending at once: {monday}"
    );
    assert!(
        monday.contains("24h"),
        "the span shown and the hours claimed have to agree: {monday}"
    );
}

/// The one case where naming the boundary and formatting the instant genuinely disagree.
///
/// Santiago moves the clock forward at midnight on 2038-09-05, so that local midnight never
/// happens and the end of 2038-09-04 is the instant a clock reads as 01:00 the next day.
/// Formatting the clip target therefore reports the end of Friday as `01:00`, which belongs to
/// Saturday. In any zone without that transition both implementations print the same thing, which
/// is exactly why this has to run against the built binary with a zone in its environment.
#[test]
fn the_end_of_a_day_whose_midnight_never_happened_is_still_the_end_of_that_day() {
    // 2038-09-04 00:00 in Santiago, and the instant 24 hours later, which is that day's end.
    const FRIDAY: i64 = 2_167_185_600;
    const FRIDAY_END: i64 = 2_167_272_000;
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[StoredSegment {
            started_at: FRIDAY_END - 600,
            ended_at: Some(FRIDAY_END + 3600),
            last_seen_at: FRIDAY_END + 3600,
            app_class: "crossing-app",
        }],
    );

    let friday = run_in_zone(
        &db_path,
        "America/Santiago",
        &["today", "--date", "2038-09-04"],
    );

    assert!(
        friday.contains("-24:00"),
        "a segment clipped to the end of the day says so: {friday}"
    );
    // The whole row, not a fragment of it: asserting the span and the duration together is what
    // pins the boundary to the right instant, and it fails loudly if tzdata lacks the 2038 rule.
    assert!(
        friday.contains("23:50-24:00  10m"),
        "the ten minutes before the day's end belong to that day and end at it: {friday}"
    );
    assert!(
        !friday.contains("-01:00"),
        "the end of Friday must not be reported as an hour that belongs to Saturday: {friday}"
    );
}

/// The mirror of the case above, and the one that had no test at all.
///
/// Sao_Paulo moved the clock back *at midnight* until 2019: at 2018-02-18 00:00 the clock returned
/// to 23:00 on the 17th, so the 17th is 25 hours long and its 23:00 hour happens twice. Naming the
/// start of the 18th by the earlier of the two candidate instants picked one whose local reading is
/// still 23:00 on the 17th, which put the boundary an hour inside the day it was supposed to close:
/// the 17th reported 24 hours of a 25 hour day, and its final hour was filed under the 18th.
#[test]
fn a_day_whose_clock_moves_back_at_midnight_keeps_its_last_hour() {
    // 2018-02-17 00:00 in Sao_Paulo, and the true start of the 18th, 25 hours later.
    const SATURDAY: i64 = 1_518_832_800;
    const SUNDAY: i64 = 1_518_922_800;
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[StoredSegment {
            started_at: SATURDAY,
            ended_at: Some(SUNDAY),
            last_seen_at: SUNDAY,
            app_class: "all-day-app",
        }],
    );

    let saturday = run_in_zone(
        &db_path,
        "America/Sao_Paulo",
        &["today", "--date", "2018-02-17"],
    );
    let sunday = run_in_zone(
        &db_path,
        "America/Sao_Paulo",
        &["today", "--date", "2018-02-18"],
    );

    assert!(
        saturday.contains("00:00-24:00  25h"),
        "a day the clock lengthens to 25 hours has to report all of them: {saturday}"
    );
    assert!(
        !sunday.contains("all-day-app"),
        "the last hour of Saturday must not be reported as Sunday's: {sunday}"
    );
}

#[test]
fn a_day_left_open_by_a_crash_does_not_spill_into_later_days() {
    // 2026-07-20 09:00, observed until 09:05 and never closed.
    const MONDAY_MORNING: i64 = 1_784_557_200;
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[StoredSegment {
            started_at: MONDAY_MORNING,
            ended_at: None,
            last_seen_at: MONDAY_MORNING + 300,
            app_class: "crashed-app",
        }],
    );

    let monday = run_in_zone(
        &db_path,
        "America/Sao_Paulo",
        &["today", "--date", "2026-07-20"],
    );
    let tuesday = run_in_zone(
        &db_path,
        "America/Sao_Paulo",
        &["today", "--date", "2026-07-21"],
    );

    assert!(
        monday.contains("5m      crashed-app"),
        "an unclosed segment must report the five minutes that were observed: {monday}"
    );
    assert!(
        !tuesday.contains("crashed-app"),
        "a later day must not inherit a segment nobody closed: {tuesday}"
    );
}
