use crate::activity::{ActivitySnapshot, TimelineSegment};
use crate::config::{Blacklist, Config};
use crate::desktop::{ActiveWindowSource, HyprlandClient};
use crate::export::render_day_export;
use crate::input::InputActivity;
use crate::service::render_user_unit;
use crate::storage::Store;
use crate::timeline::{day_bounds, local_date, render_day, unix_now};
use chrono::NaiveDate;
use std::env;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const USAGE: &str = "\
daytrace

Usage:
  daytrace start
  daytrace today [--date YYYY-MM-DD]
  daytrace export [--date YYYY-MM-DD]
  daytrace service unit
  daytrace help

Commands:
  start         Start the desktop activity capture daemon.
  today         Print a chronological activity timeline, by default for today.
  export        Print one day of stored activity as JSON, by default today.
  service unit  Print a systemd user unit that runs the daemon for this login session.
  help          Print this help text.

Environment:
  DAYTRACE_DB_PATH                 Override the SQLite database path.
  DAYTRACE_IDLE_AFTER_SECONDS      Idle threshold, default 300.
  DAYTRACE_BLACKLIST_APPS          Comma-separated app class substrings to skip.
  DAYTRACE_BLACKLIST_TITLES        Comma-separated title substrings to skip.
  DAYTRACE_BLACKLIST_DOMAINS       Comma-separated URL/domain substrings to skip.
";

pub fn main_exit() -> ExitCode {
    match run(env::args().skip(1)) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            eprintln!();
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run(args: impl IntoIterator<Item = String>) -> Result<String, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(USAGE.to_string()),
        [arg] if arg == "help" || arg == "--help" || arg == "-h" => Ok(USAGE.to_string()),
        [arg] if arg == "start" => {
            let config = Config::from_env()?;
            run_daemon(config)?;
            Ok(String::new())
        }
        // The arguments are read before the environment, so a mistyped flag is reported as
        // itself rather than as whatever the configuration complains about first.
        [arg, rest @ ..] if arg == "today" => {
            let requested = requested_day(rest)?;
            let (date, segments) = stored_day(&Config::from_env()?, requested)?;
            render_day(date, &segments)
        }
        [arg, rest @ ..] if arg == "export" => {
            let requested = requested_day(rest)?;
            let (date, segments) = stored_day(&Config::from_env()?, requested)?;
            render_day_export(date, &segments)
        }
        [first, second] if first == "service" && second == "unit" => render_service_unit(),
        [unknown] => Err(format!("unknown command: {unknown}")),
        _ => Err(format!("unknown command: {}", args.join(" "))),
    }
}

/// The unit has to name the binary that will actually run, and only the running process
/// knows where that is. Rendering from the running path means the printed unit points at
/// the same installation that produced it, instead of at a guessed location.
fn render_service_unit() -> Result<String, String> {
    let exec_path = env::current_exe()
        .map_err(|error| format!("failed to resolve the daytrace binary path: {error}"))?;
    Ok(render_user_unit(&exec_path))
}

/// The day named by `--date`, or `None` when the caller wants the default.
///
/// Hand-rolled rather than delegated to an argument parser: the surface is one flag, and
/// what an argument parser would add here is a dependency, not an explanation of what a
/// rejected date should have looked like.
fn requested_day(args: &[String]) -> Result<Option<NaiveDate>, String> {
    match args {
        [] => Ok(None),
        [flag, value] if flag == "--date" => parse_day(value).map(Some),
        [single] if single == "--date" => {
            Err("--date needs a day, as --date YYYY-MM-DD".to_string())
        }
        [single] => match single.strip_prefix("--date=") {
            Some(value) => parse_day(value).map(Some),
            None => Err(format!("unexpected argument: {single}")),
        },
        _ => Err(format!("unexpected arguments: {}", args.join(" "))),
    }
}

/// The exact format is required, not merely a readable one.
///
/// Date parsing accepts an unpadded field, which turns two plausible typos into a report for
/// a day nobody asked for: `26-07-20` reads as the year 26, and the report is then headed with
/// a date the reader has to notice is wrong.
fn parse_day(value: &str) -> Result<NaiveDate, String> {
    const EXPECTED_LENGTH: usize = "YYYY-MM-DD".len();

    if value.len() != EXPECTED_LENGTH {
        return Err(format!("invalid date: {value}, expected YYYY-MM-DD"));
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("invalid date: {value}, expected YYYY-MM-DD"))
}

fn run_daemon(config: Config) -> Result<(), String> {
    let running = Arc::new(AtomicBool::new(true));
    let running_for_signal = Arc::clone(&running);
    ctrlc::set_handler(move || running_for_signal.store(false, Ordering::Relaxed))
        .map_err(|error| format!("failed to install Ctrl-C handler: {error}"))?;

    let hyprland = HyprlandClient::new();
    let input_activity = InputActivity::start(Arc::clone(&running), unix_now())?;
    let mut store = Store::open(&config.db_path, config.secure_data_dir.clone())?;
    store.close_stale_open_segments()?;

    let mut streak = FailureStreak::default();
    while running.load(Ordering::Relaxed) {
        let observed_at = unix_now();
        // Idle is only detectable once the threshold has already passed, so the transition is
        // dated back to the last input rather than to the moment it was noticed. Dating it to
        // now credited the whole threshold window to whichever window still held focus, which
        // inflated that one application by up to the threshold on every single absence.
        let last_input = input_activity.last_activity_at();
        let idle_since =
            (observed_at - last_input >= config.idle_after.as_secs() as i64).then_some(last_input);

        match capture_once(
            &mut store,
            &hyprland,
            &config.blacklist,
            observed_at,
            idle_since,
        ) {
            // Only a poll that actually reached the desktop is evidence that it is reachable.
            Ok(Observed::Desktop) => streak.record_success(),
            Ok(Observed::Idle) => {}
            Err(error) => match streak.record_failure() {
                Some(count) => eprintln!(
                    "daytrace: observation failed ({count}/{MAX_CONSECUTIVE_FAILURES}): {error}"
                ),
                None => {
                    return Err(format!(
                        "capture failed {MAX_CONSECUTIVE_FAILURES} times in a row, giving up: {error}"
                    ));
                }
            },
        }

        wait_for_next_poll(&running, config.poll_interval);
    }

    store.close_open(unix_now())
}

/// How many consecutive failed observations end the daemon.
///
/// WHY tolerate any: one failed compositor query used to terminate capture for the rest of
/// the day, and a restarted compositor or a momentarily busy socket produces exactly that.
/// The failure was invisible until the timeline turned up empty hours later.
///
/// WHY not tolerate forever: a permanently broken setup would then spin silently, which from
/// the outside is indistinguishable from working. At the default poll interval this is about
/// a minute of uninterrupted failure before the daemon gives up and says why.
const MAX_CONSECUTIVE_FAILURES: u32 = 60;

#[derive(Debug, Default)]
struct FailureStreak {
    consecutive: u32,
}

impl FailureStreak {
    fn record_success(&mut self) {
        self.consecutive = 0;
    }

    /// The streak so far, or `None` once the daemon has to stop.
    fn record_failure(&mut self) -> Option<u32> {
        self.consecutive += 1;
        (self.consecutive < MAX_CONSECUTIVE_FAILURES).then_some(self.consecutive)
    }
}

/// What a poll actually did, which is not the same as whether it succeeded.
///
/// An idle poll answers from the input timestamps alone and never reaches the desktop, so it
/// carries no evidence that the desktop is reachable. Counting it as a success would let a
/// permanently broken compositor query be forgiven by every quiet stretch, and the daemon
/// would run forever writing a timeline made entirely of AFK.
#[derive(Debug, Eq, PartialEq)]
enum Observed {
    Desktop,
    Idle,
}

/// Record one observation. `idle_since` carries the moment input stopped when the machine is
/// idle, which is earlier than `observed_at` by at least the idle threshold.
fn capture_once(
    store: &mut Store,
    source: &dyn ActiveWindowSource,
    blacklist: &Blacklist,
    observed_at: i64,
    idle_since: Option<i64>,
) -> Result<Observed, String> {
    let (starts_at, snapshot, observed) = match idle_since {
        Some(idle_since) => (idle_since, Some(ActivitySnapshot::idle()), Observed::Idle),
        None => (
            observed_at,
            source.active_snapshot(blacklist)?,
            Observed::Desktop,
        ),
    };

    match snapshot {
        Some(snapshot) => store.record_observation(starts_at, observed_at, &snapshot)?,
        None => store.close_open(observed_at)?,
    }
    Ok(observed)
}

fn wait_for_next_poll(running: &AtomicBool, poll_interval: Duration) {
    let deadline = Instant::now() + poll_interval;
    while running.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(200)));
    }
}

/// The stored segments of the requested day, or of today when no day was requested.
///
/// A day that was never recorded reads as an empty day rather than an error, so a machine
/// that has not run the daemon yet gets an answer instead of a failure.
fn stored_day(
    config: &Config,
    requested: Option<NaiveDate>,
) -> Result<(NaiveDate, Vec<TimelineSegment>), String> {
    let now = unix_now();
    let date = match requested {
        Some(date) => date,
        None => local_date(now)?,
    };

    if !config.db_path.exists() {
        return Ok((date, Vec::new()));
    }

    let store = Store::open(&config.db_path, config.secure_data_dir.clone())?;
    let (start, end) = day_bounds(date)?;
    // `now` still bounds a segment left open, which for a past day the query clips to the
    // end of that day.
    Ok((date, store.timeline_between(start, end, now)?))
}

#[cfg(test)]
mod tests {
    use super::{
        FailureStreak, MAX_CONSECUTIVE_FAILURES, Observed, capture_once, requested_day, run,
    };
    use crate::activity::{ActivitySnapshot, TimelineSegment};
    use crate::config::Blacklist;
    use crate::desktop::ActiveWindowSource;
    use crate::storage::Store;
    use chrono::NaiveDate;
    use std::cell::RefCell;

    /// A desktop boundary that replays a fixed script of outcomes, so a transient failure
    /// followed by a recovery can be staged deterministically.
    struct ScriptedWindowSource {
        remaining: RefCell<Vec<Result<Option<ActivitySnapshot>, String>>>,
    }

    impl ScriptedWindowSource {
        fn new(responses: Vec<Result<Option<ActivitySnapshot>, String>>) -> Self {
            Self {
                remaining: RefCell::new(responses),
            }
        }
    }

    impl ActiveWindowSource for ScriptedWindowSource {
        fn active_snapshot(
            &self,
            _blacklist: &Blacklist,
        ) -> Result<Option<ActivitySnapshot>, String> {
            self.remaining
                .borrow_mut()
                .pop()
                .expect("script ran out of responses")
        }
    }

    #[test]
    fn a_failed_observation_does_not_stop_later_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let window = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );
        // Popped from the back, so the failure is served first.
        let source = ScriptedWindowSource::new(vec![
            Ok(Some(window.clone())),
            Err("hyprctl activewindow failed".to_string()),
        ]);
        let blacklist = Blacklist::default();

        capture_once(&mut store, &source, &blacklist, 100, None)
            .expect_err("the compositor query failed");
        capture_once(&mut store, &source, &blacklist, 110, None)
            .expect("the recovered query is recorded");
        store.close_open(120).expect("close");

        let rows = store.timeline_between(0, 200, 200).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].started_at, 110);
        assert_eq!(rows[0].snapshot, window);
    }

    #[test]
    fn an_idle_stretch_is_credited_from_the_last_input_not_from_its_detection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let window = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );
        let source = ScriptedWindowSource::new(vec![Ok(Some(window))]);
        let blacklist = Blacklist::default();

        // Input stops at 1000. Idle is only detected at 1300, once the 300 second threshold
        // has elapsed, and the five minutes in between belong to nobody but the absence.
        capture_once(&mut store, &source, &blacklist, 1000, None).expect("window observed");
        capture_once(&mut store, &source, &blacklist, 1300, Some(1000)).expect("idle detected");
        store.close_open(1400).expect("close");

        let rows = store.timeline_between(0, 2000, 2000).expect("timeline");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].ended_at, 1000,
            "the window must not absorb the wait"
        );
        assert_eq!(rows[1].started_at, 1000);
        assert_eq!(rows[1].snapshot, ActivitySnapshot::idle());
    }

    #[test]
    fn a_backdated_idle_start_never_overlaps_time_already_accounted_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let window = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );
        // The window vanishes at 1100 with no input since 1000, which is the ordinary path
        // when a lock screen takes over or a focused terminal exits on its own.
        let source = ScriptedWindowSource::new(vec![
            Ok(Some(ActivitySnapshot::unknown())),
            Ok(Some(window)),
        ]);
        let blacklist = Blacklist::default();

        capture_once(&mut store, &source, &blacklist, 900, None).expect("window observed");
        capture_once(&mut store, &source, &blacklist, 1100, None).expect("window gone");
        capture_once(&mut store, &source, &blacklist, 1300, Some(1000)).expect("idle detected");

        let rows = store.timeline_between(0, 2000, 2000).expect("timeline");
        for pair in rows.windows(2) {
            assert!(
                pair[0].ended_at <= pair[1].started_at,
                "segments must not overlap, or the day totals more than it lasted: {rows:?}"
            );
        }
        assert!(
            rows.iter().all(|row| row.ended_at >= row.started_at),
            "no segment may end before it starts: {rows:?}"
        );
    }

    #[test]
    fn an_idle_poll_is_not_evidence_that_the_desktop_is_reachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        // An empty script panics if consulted, proving the idle path never reaches the source.
        let source = ScriptedWindowSource::new(Vec::new());

        let observed = capture_once(&mut store, &source, &Blacklist::default(), 1300, Some(1000))
            .expect("idle is recorded without the desktop");

        assert_eq!(
            observed,
            Observed::Idle,
            "an idle poll must not be reported as a reachable desktop, or a broken \
             compositor query would be forgiven by every quiet stretch of the day"
        );
    }

    #[test]
    fn transient_failures_are_tolerated_but_a_sustained_one_gives_up() {
        let mut streak = FailureStreak::default();

        for expected in 1..MAX_CONSECUTIVE_FAILURES {
            assert_eq!(streak.record_failure(), Some(expected));
        }
        assert_eq!(streak.record_failure(), None);
    }

    #[test]
    fn a_success_clears_the_failure_streak() {
        let mut streak = FailureStreak::default();

        for _ in 0..MAX_CONSECUTIVE_FAILURES - 1 {
            streak.record_failure();
        }
        streak.record_success();

        assert_eq!(streak.record_failure(), Some(1));
    }

    #[test]
    fn prints_help_without_args() {
        let output = run(Vec::<String>::new()).expect("help should succeed");
        assert!(output.contains("daytrace start"));
        assert!(output.contains("DAYTRACE_BLACKLIST_APPS"));
    }

    #[test]
    fn prints_a_systemd_unit_pointing_at_the_running_binary() {
        let output = run(["service".to_string(), "unit".to_string()])
            .expect("the unit should render for the running binary");
        let exec_path = std::env::current_exe().expect("current exe");

        assert!(output.contains("[Install]"), "not a unit file: {output}");
        assert!(
            output.contains(&format!("ExecStart=\"{}\" start", exec_path.display())),
            "the unit must point at the binary that printed it: {output}"
        );
    }

    #[test]
    fn no_date_argument_asks_for_the_default_day() {
        assert_eq!(requested_day(&[]).expect("no argument"), None);
    }

    #[test]
    fn a_date_argument_selects_that_day_in_either_spelling() {
        let expected = NaiveDate::from_ymd_opt(2026, 7, 20);

        for spelling in [
            vec!["--date".to_string(), "2026-07-20".to_string()],
            vec!["--date=2026-07-20".to_string()],
        ] {
            assert_eq!(
                requested_day(&spelling).expect("valid date"),
                expected,
                "failed for {spelling:?}"
            );
        }
    }

    #[test]
    fn a_rejected_date_says_what_a_day_should_look_like() {
        for argument in [
            vec!["--date".to_string()],
            vec!["--date".to_string(), "2026-13-40".to_string()],
            vec!["--date".to_string(), "yesterday".to_string()],
            vec!["--date=".to_string()],
            // Both parse, and neither means what it looks like: an unpadded day is not the
            // documented format, and a two-digit year is read as the year 26.
            vec!["--date".to_string(), "2026-7-2".to_string()],
            vec!["--date".to_string(), "26-07-20".to_string()],
        ] {
            let error = requested_day(&argument).expect_err("should be rejected");
            assert!(
                error.contains("YYYY-MM-DD"),
                "{argument:?} was rejected without saying what was expected: {error}"
            );
        }
    }

    #[test]
    fn an_argument_that_is_not_a_date_flag_is_named_in_the_error() {
        let error = requested_day(&["yesterday".to_string()]).expect_err("should be rejected");
        assert_eq!(error, "unexpected argument: yesterday");
    }

    #[test]
    fn rejects_unknown_commands() {
        let error = run(["capture".to_string()]).expect_err("unknown command should fail");
        assert_eq!(error, "unknown command: capture");
    }
}
