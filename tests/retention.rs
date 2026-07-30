//! Whole-binary exercises for the retention window.
//!
//! `prune` is the only command that deletes, and the window it applies opens at a local midnight,
//! so the cases that turn on where a day begins run the built binary with a timezone in its
//! environment. A process reads its zone once and caches it, which makes an in-process test of a
//! local boundary either ineffective or contagious to every other test sharing the process. The
//! cutoff is also relative to the day the case runs on, which nothing outside the process can
//! pin, so the day the binary believes it is in is read back from it rather than assumed.

use chrono::{Days, NaiveDate};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const SECONDS_PER_DAY: i64 = 86_400;

/// A zone with no daylight saving since 2019, for the cases that are not about the boundary.
const STEADY_ZONE: &str = "America/Sao_Paulo";

/// Havana moves its clock forward at midnight on 2026-03-08, so that local midnight never happens
/// at all. A window whose oldest kept day is that one has to open at the first hour that does
/// exist, 01:00, which is this instant.
const FIRST_HOUR_OF_A_DAY_WITHOUT_A_MIDNIGHT: i64 = 1_772_946_000;
const ZONE_THAT_SKIPS_A_MIDNIGHT: &str = "America/Havana";
const DAY_WITHOUT_A_MIDNIGHT: &str = "2026-03-08";

/// A segment as stored, in the shape a case needs to control.
struct StoredSegment {
    started_at: i64,
    ended_at: Option<i64>,
    last_seen_at: Option<i64>,
    app_class: String,
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64
}

/// A closed half-hour segment `age` days back, named after its age.
///
/// Ages are kept well clear of whichever window boundary a case uses. The data is relative to now
/// while the boundary is a local midnight, so a segment placed exactly one day outside the window
/// straddles the cutoff during the last half hour of every local day: a case built that way is
/// green all day and fails at 23:31.
fn aged(age: i64) -> StoredSegment {
    let started_at = unix_now() - age * SECONDS_PER_DAY;
    StoredSegment {
        started_at,
        ended_at: Some(started_at + 1800),
        last_seen_at: Some(started_at + 1800),
        app_class: format!("aged-{age}-days"),
    }
}

fn spanning(started_at: i64, ended_at: i64, app_class: &str) -> StoredSegment {
    StoredSegment {
        started_at,
        ended_at: Some(ended_at),
        last_seen_at: Some(ended_at),
        app_class: app_class.to_string(),
    }
}

/// A segment with neither an end nor a progress marker, which is what a store written before
/// progress was tracked holds for a daemon that never closed one.
fn never_closed(started_at: i64, app_class: &str) -> StoredSegment {
    StoredSegment {
        started_at,
        ended_at: None,
        last_seen_at: None,
        app_class: app_class.to_string(),
    }
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

/// Run the built binary against `db_path`, in a fixed zone and with a fixed window.
///
/// `retention_days` is `None` for the case that has to prove what the documented default does,
/// which is the only way to test it: an unset variable is what a first install has.
fn run_in_zone(db_path: &Path, zone: &str, retention_days: Option<&str>, args: &[&str]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_daytrace"));
    command.args(args).env("TZ", zone).env(
        "DAYTRACE_DB_PATH",
        db_path.as_os_str().to_str().expect("a utf-8 path"),
    );
    if let Some(days) = retention_days {
        command.env("DAYTRACE_RETENTION_DAYS", days);
    }

    let output = command.output().expect("run daytrace");
    assert!(
        output.status.success(),
        "daytrace {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("output is utf-8")
}

fn run_daytrace(db_path: &Path, retention_days: Option<&str>, args: &[&str]) -> String {
    run_in_zone(db_path, STEADY_ZONE, retention_days, args)
}

fn surviving_segments(db_path: &Path) -> Vec<String> {
    let connection = Connection::open(db_path).expect("open scratch database");
    let mut statement = connection
        .prepare("SELECT app_class FROM activity_segments ORDER BY started_at ASC")
        .expect("prepare");
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query");
    rows.collect::<Result<Vec<_>, _>>().expect("materialize")
}

/// The local day the binary itself believes it is in, read back from an empty day's report.
///
/// Asked of the child process rather than computed here, because the zone that decides where a day
/// begins is the child's and the test process has its own.
fn today_according_to_daytrace(db_path: &Path, zone: &str) -> NaiveDate {
    let report = run_in_zone(db_path, zone, Some("30"), &["today"]);
    let named_day = report
        .split_whitespace()
        .next_back()
        .expect("a non-empty report")
        .trim_end_matches('.');
    NaiveDate::parse_from_str(named_day, "%Y-%m-%d")
        .unwrap_or_else(|error| panic!("{named_day} is not a day: {error}"))
}

#[test]
fn pruning_deletes_the_days_outside_the_window_and_keeps_the_rest() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(directory.path(), &[aged(200), aged(100), aged(45), aged(2)]);

    let report = run_daytrace(&db_path, Some("30"), &["prune"]);

    assert!(
        report.contains("Deleted 3 activity segments"),
        "the command has to say how much it deleted: {report}"
    );
    assert_eq!(
        surviving_segments(&db_path),
        vec!["aged-2-days"],
        "everything that ended before the window opened should be gone, and nothing else"
    );
}

#[test]
fn the_documented_default_window_is_what_an_unset_variable_applies() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(directory.path(), &[aged(200), aged(45)]);

    let report = run_daytrace(&db_path, None, &["prune"]);

    assert!(
        report.contains("Retention window: 90 days plus today"),
        "an installation that configures nothing must still be told which window ran: {report}"
    );
    assert_eq!(
        surviving_segments(&db_path),
        vec!["aged-45-days"],
        "the default keeps ninety days, so six months back goes and six weeks back stays"
    );
}

#[test]
fn a_dry_run_reports_what_it_would_delete_and_deletes_nothing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(directory.path(), &[aged(200), aged(100), aged(45), aged(2)]);

    let preview = run_daytrace(&db_path, Some("30"), &["prune", "--dry-run"]);

    assert!(
        preview.contains("3 activity segments are outside it")
            && preview.contains("Nothing was deleted"),
        "a preview of an irreversible command must state both the count and its own \
         harmlessness: {preview}"
    );
    assert_eq!(
        surviving_segments(&db_path).len(),
        4,
        "a dry run that removes a row is not a dry run"
    );
}

#[test]
fn pruning_names_the_window_and_the_first_day_it_keeps() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(directory.path(), &[aged(200)]);
    let today = today_according_to_daytrace(&db_path, STEADY_ZONE);

    let report = run_daytrace(&db_path, Some("30"), &["prune"]);

    let first_day_kept = today
        .checked_sub_days(Days::new(30))
        .expect("thirty days before today");
    assert!(
        report.contains(&format!(
            "Retention window: 30 days plus today, keeping activity from {first_day_kept} onwards."
        )),
        "the policy applied has to be visible in the output of the command applying it, and \
         thirty days back from {today} is {first_day_kept}: {report}"
    );
}

#[test]
fn a_window_opening_on_a_day_without_a_midnight_opens_where_that_day_does() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(
        directory.path(),
        &[
            spanning(
                FIRST_HOUR_OF_A_DAY_WITHOUT_A_MIDNIGHT - 7200,
                FIRST_HOUR_OF_A_DAY_WITHOUT_A_MIDNIGHT - 5400,
                "the-evening-before",
            ),
            spanning(
                FIRST_HOUR_OF_A_DAY_WITHOUT_A_MIDNIGHT + 1800,
                FIRST_HOUR_OF_A_DAY_WITHOUT_A_MIDNIGHT + 3600,
                "the-first-hour-that-exists",
            ),
        ],
    );
    // Measured from the day the binary is in rather than written as a number, or the case would
    // name a different oldest kept day every day it runs.
    let today = today_according_to_daytrace(&db_path, ZONE_THAT_SKIPS_A_MIDNIGHT);
    let boundary_day =
        NaiveDate::parse_from_str(DAY_WITHOUT_A_MIDNIGHT, "%Y-%m-%d").expect("a valid day");
    let window = (today - boundary_day).num_days();
    assert!(
        window > 0,
        "{DAY_WITHOUT_A_MIDNIGHT} has to be in the past for this case to describe a window"
    );

    let report = run_in_zone(
        &db_path,
        ZONE_THAT_SKIPS_A_MIDNIGHT,
        Some(&window.to_string()),
        &["prune"],
    );

    assert!(
        report.contains(&format!(
            "keeping activity from {DAY_WITHOUT_A_MIDNIGHT} onwards"
        )),
        "a day whose midnight the clock skipped is still a day a window can open on: {report}"
    );
    assert_eq!(
        surviving_segments(&db_path),
        vec!["the-first-hour-that-exists"],
        "the window opens when that day began, which is the first hour that exists there, so \
         the evening before goes and the hour after stays"
    );
}

#[test]
fn a_segment_the_report_still_shows_is_not_deleted_by_the_window() {
    let directory = tempfile::tempdir().expect("tempdir");
    // No end and no progress marker, which is what a store written before progress was tracked
    // holds for a daemon that never closed a segment. The report draws such a row as reaching
    // the present, so the window has to read it the same way.
    let db_path = seeded_database(
        directory.path(),
        &[never_closed(
            unix_now() - 200 * SECONDS_PER_DAY,
            "never-closed-app",
        )],
    );

    let before = run_daytrace(&db_path, Some("30"), &["today"]);
    let report = run_daytrace(&db_path, Some("30"), &["prune"]);
    let after = run_daytrace(&db_path, Some("30"), &["today"]);

    assert!(
        before.contains("never-closed-app"),
        "the report shows the segment before the prune: {before}"
    );
    assert!(
        report.contains("Deleted 0 activity segments"),
        "a segment the report draws as covering today must survive a window that closed months \
         ago: {report}"
    );
    assert!(
        after.contains("never-closed-app"),
        "and it has to still be there afterwards, or a window about the past emptied today's \
         report: {after}"
    );
}

#[test]
fn pruning_a_machine_with_no_stored_activity_creates_no_database() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = directory.path().join("never-recorded.db");

    let report = run_daytrace(&db_path, Some("30"), &["prune"]);

    assert!(
        report.contains("No stored activity was found"),
        "a machine that never captured anything gets an answer, not a failure: {report}"
    );
    assert!(
        !db_path.exists(),
        "a command whose job is to shrink the store must not be the thing that creates it"
    );
}

#[test]
fn reporting_a_day_never_applies_the_retention_window() {
    let directory = tempfile::tempdir().expect("tempdir");
    let db_path = seeded_database(directory.path(), &[aged(200), aged(2)]);

    run_daytrace(&db_path, Some("30"), &["today"]);
    run_daytrace(&db_path, Some("30"), &["export"]);

    assert_eq!(
        surviving_segments(&db_path).len(),
        2,
        "the retention window is applied by prune alone: a read path that enforced it would \
         delete activity while answering a question about it"
    );
}
