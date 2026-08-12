use crate::activity::{ActivitySnapshot, TimelineSegment};
use crate::config::{Blacklist, Config};
use crate::desktop::{ActiveWindowSource, HyprlandClient};
use crate::export::render_day_export;
use crate::input::InputActivity;
use crate::lock::CaptureLock;
use crate::service::render_user_unit;
use crate::session::{PowerGapWatch, PoweredDownGap, SystemSessionClock};
use crate::storage::{CaptureStore, Pruned, Store};
use crate::timeline::{day_bounds, local_date, render_day, retention_cutoff, unix_now};
use chrono::NaiveDate;
use std::env;
use std::fmt;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Exit code returned for a bad invocation: unknown command, mistyped flag, invalid date.
const EXIT_USAGE: u8 = 2;

/// Exit code returned when a valid invocation fails while running.
const EXIT_RUNTIME_FAILURE: u8 = 1;

const USAGE: &str = "\
daytrace

Usage:
  daytrace start
  daytrace today [--date YYYY-MM-DD]
  daytrace export [--date YYYY-MM-DD]
  daytrace prune [--dry-run]
  daytrace service unit
  daytrace help

Commands:
  start         Start the desktop activity capture daemon.
  today         Print a chronological activity timeline, by default for today.
  export        Print one day of stored activity as JSON, by default today.
  prune         Delete stored activity from before the retention window.
  service unit  Print a systemd user unit that runs the daemon for this login session.
  help          Print this help text.

Environment:
  DAYTRACE_DB_PATH                 Override the SQLite database path.
  DAYTRACE_IDLE_AFTER_SECONDS      Idle threshold, default 300.
  DAYTRACE_RETENTION_DAYS          Days before today that prune keeps, default 90.
  DAYTRACE_BLACKLIST_APPS          Comma-separated app class substrings to skip.
  DAYTRACE_BLACKLIST_TITLES        Comma-separated title substrings to skip.
  DAYTRACE_BLACKLIST_DOMAINS       Comma-separated URL/domain substrings to skip.
  DAYTRACE_POLL_SECONDS            Desktop polling interval, default 1.
";

/// The two ways dispatch can fail.
///
/// A usage error is a bad invocation: the user asked for something the binary does not
/// understand, so the refusal prints the usage block. A runtime failure is a valid
/// invocation that could not complete, and the failure message alone is printed.
#[derive(Debug, Eq, PartialEq)]
enum AppError {
    Usage(String),
    Runtime(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Usage(message) | AppError::Runtime(message) => formatter.write_str(message),
        }
    }
}

impl From<String> for AppError {
    fn from(error: String) -> Self {
        AppError::Runtime(error)
    }
}

pub fn main_exit() -> ExitCode {
    match run(env::args().skip(1)) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(AppError::Usage(error)) => {
            eprintln!("{error}");
            eprintln!();
            eprint!("{USAGE}");
            ExitCode::from(EXIT_USAGE)
        }
        Err(AppError::Runtime(error)) => {
            eprintln!("{error}");
            ExitCode::from(EXIT_RUNTIME_FAILURE)
        }
    }
}

fn run(args: impl IntoIterator<Item = String>) -> Result<String, AppError> {
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
            render_day(date, &segments).map_err(AppError::from)
        }
        [arg, rest @ ..] if arg == "export" => {
            let requested = requested_day(rest)?;
            let (date, segments) = stored_day(&Config::from_env()?, requested)?;
            render_day_export(date, &segments).map_err(AppError::from)
        }
        [arg, rest @ ..] if arg == "prune" => {
            let dry_run = prune_is_dry_run(rest)?;
            prune_old_activity(&Config::from_env()?, dry_run).map_err(AppError::from)
        }
        [first, second] if first == "service" && second == "unit" => {
            render_service_unit().map_err(AppError::from)
        }
        [unknown] => Err(AppError::Usage(format!("unknown command: {unknown}"))),
        _ => Err(AppError::Usage(format!(
            "unknown command: {}",
            args.join(" ")
        ))),
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
fn requested_day(args: &[String]) -> Result<Option<NaiveDate>, AppError> {
    match args {
        [] => Ok(None),
        [flag, value] if flag == "--date" => parse_day(value).map(Some),
        [single] if single == "--date" => Err(AppError::Usage(
            "--date needs a day, as --date YYYY-MM-DD".to_string(),
        )),
        [single] => match single.strip_prefix("--date=") {
            Some(value) => parse_day(value).map(Some),
            None => Err(AppError::Usage(format!("unexpected argument: {single}"))),
        },
        _ => Err(AppError::Usage(format!(
            "unexpected arguments: {}",
            args.join(" ")
        ))),
    }
}

/// Whether `prune` should only report what the window would remove.
fn prune_is_dry_run(args: &[String]) -> Result<bool, AppError> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--dry-run" => Ok(true),
        [single] => Err(AppError::Usage(format!("unexpected argument: {single}"))),
        _ => Err(AppError::Usage(format!(
            "unexpected arguments: {}",
            args.join(" ")
        ))),
    }
}

/// The exact format is required, not merely a readable one.
///
/// Date parsing accepts an unpadded field, which turns two plausible typos into a report for
/// a day nobody asked for: `26-07-20` reads as the year 26, and the report is then headed with
/// a date the reader has to notice is wrong.
fn parse_day(value: &str) -> Result<NaiveDate, AppError> {
    const EXPECTED_LENGTH: usize = "YYYY-MM-DD".len();

    if value.len() != EXPECTED_LENGTH {
        return Err(AppError::Usage(format!(
            "invalid date: {value}, expected YYYY-MM-DD"
        )));
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| AppError::Usage(format!("invalid date: {value}, expected YYYY-MM-DD")))
}

fn run_daemon(config: Config) -> Result<(), String> {
    // Claimed before anything else, and in particular before the input devices are opened: a
    // duplicate must not acquire that capability on its way to being refused, and refusing it
    // here means the message says what is wrong rather than whatever the second setup step
    // happens to complain about first.
    let _capture = CaptureLock::acquire(&config.db_path)?;

    let running = Arc::new(AtomicBool::new(true));
    let running_for_signal = Arc::clone(&running);
    ctrlc::set_handler(move || running_for_signal.store(false, Ordering::Relaxed))
        .map_err(|error| format!("failed to install Ctrl-C handler: {error}"))?;

    let hyprland = HyprlandClient::new();
    let input_activity = InputActivity::start(Arc::clone(&running), unix_now())?;
    let mut store = Store::open(&config.db_path, config.secure_data_dir.clone())?;
    store.close_stale_open_segments()?;

    let session_clock = SystemSessionClock;
    let mut power_gaps = PowerGapWatch::default();
    let mut pending_gaps = PendingGaps::default();
    let mut streak = FailureStreak::default();
    while running.load(Ordering::Relaxed) {
        // The clock is read through the watch rather than separately, so the instant the poll
        // is dated by is the same one the powered-down stretch was measured against.
        let session = power_gaps.observe(&session_clock);
        let observed_at = session.observed_at;
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
            session.powered_down_gap,
            &mut pending_gaps,
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

/// Powered-down stretches measured but not yet durably recorded.
///
/// `PowerGapWatch` reports a stretch exactly once, at the poll right after it ends; if that
/// poll's write fails, the stretch has to survive here or it is gone for good, and the segment
/// it interrupted is left to be silently credited to whichever window comes back into focus.
/// A second stretch measured while one is still owed is queued behind it rather than merged
/// or replaced: the poll that measured the second stretch is itself proof the machine ran in
/// between, so folding the two into one segment would state an absence that never happened,
/// and dropping the first to make room for the second would state that it never happened at
/// all. Both are stretches that were really spent off, so both get a row.
#[derive(Debug, Default)]
struct PendingGaps(Vec<PoweredDownGap>);

impl PendingGaps {
    fn push(&mut self, gap: PoweredDownGap) {
        self.0.push(gap);
    }

    /// Store every gap still owed, oldest first, stopping at the first failure so the rest stay
    /// queued for the next attempt instead of being tried out of order.
    fn flush(&mut self, store: &mut dyn CaptureStore) -> Result<(), String> {
        while let Some(gap) = self.0.first().copied() {
            store.record_powered_down_gap(gap.started_at, gap.ended_at)?;
            self.0.remove(0);
        }
        Ok(())
    }
}

/// Record one observation. `idle_since` carries the moment input stopped when the machine is
/// idle, which is earlier than `observed_at` by at least the idle threshold.
/// `powered_down_gap` carries a stretch the machine spent off, which is only ever known on the
/// first poll after it ended; `pending_gaps` carries whatever earlier stretch is still owed
/// because a previous poll could not write it.
fn capture_once(
    store: &mut dyn CaptureStore,
    source: &dyn ActiveWindowSource,
    blacklist: &Blacklist,
    observed_at: i64,
    idle_since: Option<i64>,
    powered_down_gap: Option<PoweredDownGap>,
    pending_gaps: &mut PendingGaps,
) -> Result<Observed, String> {
    // Queued rather than written directly, so a failure here leaves the stretch pending for
    // the next poll instead of losing it. Flushed before this poll's own observation, so the
    // segment that was open when the machine stopped ends there instead of swallowing the
    // whole stretch, and so the observation below finds no open segment to continue and starts
    // a fresh one at the resume.
    if let Some(gap) = powered_down_gap {
        pending_gaps.push(gap);
    }
    pending_gaps.flush(store)?;

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

/// Delete the stored activity that the retention window no longer covers.
///
/// Deleting is explicit, and nothing prunes on its own. Capture writes at most a segment a
/// second and the window is measured in months, so there is no point at which the store has
/// to be trimmed for the daemon to keep working; what an automatic prune would buy is a
/// smaller file, paid for by deleting activity the user never asked to lose, at a moment they
/// were not present for, with no undo and no record that it happened. The cost of the manual
/// alternative is that an installation nobody prunes still grows, which is a file that gets
/// larger rather than a day that silently disappears.
///
/// The report names the window and the first day it keeps before saying what happened, so the
/// policy being applied is visible in the output of the command that applies it.
fn prune_old_activity(config: &Config, dry_run: bool) -> Result<String, String> {
    let now = unix_now();
    let cutoff = retention_cutoff(now, config.retention_days)?;
    let window = window_applied(config.retention_days, local_date(cutoff)?);

    // Nothing stored is nothing to prune, and opening a store to discover that would create
    // the database this command exists to keep small.
    if !config.db_path.exists() {
        return Ok(format!(
            "{window}No stored activity was found, so nothing was deleted.\n"
        ));
    }

    let mut store = Store::open(&config.db_path, config.secure_data_dir.clone())?;
    if dry_run {
        let removable = store.count_segments_ended_before(cutoff, now)?;
        return Ok(format!("{window}{}", render_preview(removable)));
    }

    let pruned = store.prune_segments_ended_before(cutoff, now)?;
    Ok(format!("{window}{}", render_prune(&pruned)))
}

/// The first two facts of any prune, preview or not: the window, and where it opens.
fn window_applied(retention_days: u32, first_day_kept: NaiveDate) -> String {
    format!(
        "Retention window: {retention_days} days plus today, keeping activity from \
         {first_day_kept} onwards.\n"
    )
}

fn render_preview(removable: u64) -> String {
    let are = if removable == 1 { "is" } else { "are" };
    format!(
        "{} {are} outside it. Nothing was deleted.\n",
        segments(removable)
    )
}

/// What happened, and separately what could not be finished afterwards.
///
/// The second line is not an error, and the command does not fail: the rows are already gone,
/// so a caller told only that something failed would be left guessing whether the deletion
/// happened. It says what is still readable and what makes it go away.
fn render_prune(pruned: &Pruned) -> String {
    let mut report = format!("Deleted {}.\n", segments(pruned.deleted));
    if let Some(reason) = &pruned.still_in_the_file {
        report.push_str(&format!(
            "The deleted activity is still readable in the database file, because {reason}. \
             Running prune again finishes clearing it.\n"
        ));
    }
    report
}

fn segments(count: u64) -> String {
    if count == 1 {
        return "1 activity segment".to_string();
    }
    format!("{count} activity segments")
}

#[cfg(test)]
mod tests {
    use super::{
        AppError, FailureStreak, MAX_CONSECUTIVE_FAILURES, Observed, PendingGaps, capture_once,
        prune_is_dry_run, render_preview, render_prune, requested_day, run,
    };
    use crate::activity::{ActivitySnapshot, TimelineSegment};
    use crate::config::Blacklist;
    use crate::desktop::ActiveWindowSource;
    use crate::session::PoweredDownGap;
    use crate::storage::CaptureStore;
    use crate::storage::Pruned;
    use crate::storage::Store;
    use chrono::NaiveDate;
    use std::cell::RefCell;

    fn pruned(deleted: u64, still_in_the_file: Option<&str>) -> Pruned {
        Pruned {
            deleted,
            still_in_the_file: still_in_the_file.map(ToOwned::to_owned),
        }
    }

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

    /// A storage boundary whose gap write fails a fixed number of times before it starts
    /// delegating to a real store, so a busy database that later frees up can be staged
    /// deterministically instead of waited for.
    struct FlakyGapStore {
        inner: Store,
        remaining_failures: u32,
    }

    impl FlakyGapStore {
        fn new(inner: Store, remaining_failures: u32) -> Self {
            Self {
                inner,
                remaining_failures,
            }
        }
    }

    impl CaptureStore for FlakyGapStore {
        fn record_observation(
            &mut self,
            starts_at: i64,
            seen_at: i64,
            snapshot: &ActivitySnapshot,
        ) -> Result<(), String> {
            self.inner.record_observation(starts_at, seen_at, snapshot)
        }

        fn record_powered_down_gap(
            &mut self,
            started_at: i64,
            ended_at: i64,
        ) -> Result<(), String> {
            if self.remaining_failures > 0 {
                self.remaining_failures -= 1;
                return Err("database is busy".to_string());
            }
            self.inner.record_powered_down_gap(started_at, ended_at)
        }

        fn close_open(&mut self, ended_at: i64) -> Result<(), String> {
            self.inner.close_open(ended_at)
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
        let mut pending_gaps = PendingGaps::default();

        capture_once(
            &mut store,
            &source,
            &blacklist,
            100,
            None,
            None,
            &mut pending_gaps,
        )
        .expect_err("the compositor query failed");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            110,
            None,
            None,
            &mut pending_gaps,
        )
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
        let mut pending_gaps = PendingGaps::default();

        // Input stops at 1000. Idle is only detected at 1300, once the 300 second threshold
        // has elapsed, and the five minutes in between belong to nobody but the absence.
        capture_once(
            &mut store,
            &source,
            &blacklist,
            1000,
            None,
            None,
            &mut pending_gaps,
        )
        .expect("window observed");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            1300,
            Some(1000),
            None,
            &mut pending_gaps,
        )
        .expect("idle detected");
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
        let mut pending_gaps = PendingGaps::default();

        capture_once(
            &mut store,
            &source,
            &blacklist,
            900,
            None,
            None,
            &mut pending_gaps,
        )
        .expect("window observed");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            1100,
            None,
            None,
            &mut pending_gaps,
        )
        .expect("window gone");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            1300,
            Some(1000),
            None,
            &mut pending_gaps,
        )
        .expect("idle detected");

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
    fn a_suspend_is_not_credited_to_whichever_window_held_focus() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let window = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );
        // The same window is still in focus after the resume, which is the ordinary case and
        // the one that used to be indistinguishable from an hour of work.
        let source = ScriptedWindowSource::new(vec![
            Ok(Some(window.clone())),
            Ok(Some(window.clone())),
            Ok(Some(window.clone())),
        ]);
        let blacklist = Blacklist::default();
        let mut pending_gaps = PendingGaps::default();

        capture_once(
            &mut store,
            &source,
            &blacklist,
            1_000,
            None,
            None,
            &mut pending_gaps,
        )
        .expect("window observed");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            1_010,
            None,
            None,
            &mut pending_gaps,
        )
        .expect("still observed");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_000,
            None,
            Some(PoweredDownGap {
                started_at: 1_010,
                ended_at: 5_000,
            }),
            &mut pending_gaps,
        )
        .expect("resume observed");

        let rows = store.timeline_between(0, 10_000, 10_000).expect("timeline");
        assert_eq!(
            rows,
            vec![
                TimelineSegment {
                    started_at: 1_000,
                    ended_at: 1_010,
                    snapshot: window.clone(),
                },
                TimelineSegment {
                    started_at: 1_010,
                    ended_at: 5_000,
                    snapshot: ActivitySnapshot::suspended(),
                },
                TimelineSegment {
                    started_at: 5_000,
                    ended_at: 5_000,
                    snapshot: window,
                },
            ],
            "the hours the machine was off must be their own segment, and the resume must open \
             a segment of its own rather than continue the one from before"
        );
    }

    #[test]
    fn a_resume_after_a_long_absence_leaves_no_overlapping_or_backwards_segment() {
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
        let mut pending_gaps = PendingGaps::default();

        // Input stopped at 1000 and the machine went down at 1010. The input timestamps are
        // untouched by a suspend, so the first poll after the resume reports an idle stretch
        // dated hours before the machine came back.
        capture_once(
            &mut store,
            &source,
            &blacklist,
            1_000,
            None,
            None,
            &mut pending_gaps,
        )
        .expect("window observed");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_000,
            Some(1_000),
            Some(PoweredDownGap {
                started_at: 1_010,
                ended_at: 5_000,
            }),
            &mut pending_gaps,
        )
        .expect("resume observed");

        let rows = store.timeline_between(0, 10_000, 10_000).expect("timeline");
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
        assert!(
            rows.iter()
                .any(|row| row.snapshot == ActivitySnapshot::suspended()),
            "the powered-down stretch must survive the backdated idle start: {rows:?}"
        );
    }

    #[test]
    fn an_idle_poll_is_not_evidence_that_the_desktop_is_reachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        // An empty script panics if consulted, proving the idle path never reaches the source.
        let source = ScriptedWindowSource::new(Vec::new());
        let mut pending_gaps = PendingGaps::default();

        let observed = capture_once(
            &mut store,
            &source,
            &Blacklist::default(),
            1300,
            Some(1000),
            None,
            &mut pending_gaps,
        )
        .expect("idle is recorded without the desktop");

        assert_eq!(
            observed,
            Observed::Idle,
            "an idle poll must not be reported as a reachable desktop, or a broken \
             compositor query would be forgiven by every quiet stretch of the day"
        );
    }

    #[test]
    fn a_gap_that_fails_to_store_is_retried_until_it_is() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let mut store = FlakyGapStore::new(inner, 1);
        let source = ScriptedWindowSource::new(Vec::new());
        let blacklist = Blacklist::default();
        let mut pending_gaps = PendingGaps::default();
        let gap = PoweredDownGap {
            started_at: 1_010,
            ended_at: 5_000,
        };

        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_000,
            Some(4_999),
            Some(gap),
            &mut pending_gaps,
        )
        .expect_err("a busy database refuses the first write");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_010,
            Some(5_009),
            None,
            &mut pending_gaps,
        )
        .expect("the retried write succeeds once the database is free");

        let rows = store
            .inner
            .timeline_between(0, 10_000, 10_000)
            .expect("timeline");
        let suspended: Vec<_> = rows
            .iter()
            .filter(|row| row.snapshot == ActivitySnapshot::suspended())
            .collect();
        assert_eq!(
            suspended.len(),
            1,
            "the stretch must survive the failed write and reach storage exactly once: {rows:?}"
        );
        assert_eq!(
            (suspended[0].started_at, suspended[0].ended_at),
            (1_010, 5_000),
            "the stored gap must carry the boundaries it was measured with, not values \
             re-derived from the poll that finally wrote it"
        );
    }

    #[test]
    fn a_gap_survives_two_consecutive_failures_before_a_recovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let mut store = FlakyGapStore::new(inner, 2);
        let source = ScriptedWindowSource::new(Vec::new());
        let blacklist = Blacklist::default();
        let mut pending_gaps = PendingGaps::default();
        let gap = PoweredDownGap {
            started_at: 1_010,
            ended_at: 5_000,
        };

        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_000,
            Some(4_999),
            Some(gap),
            &mut pending_gaps,
        )
        .expect_err("the first write fails");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_010,
            Some(5_009),
            None,
            &mut pending_gaps,
        )
        .expect_err("the retry fails too");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_020,
            Some(5_019),
            None,
            &mut pending_gaps,
        )
        .expect("the second retry succeeds");

        let rows = store
            .inner
            .timeline_between(0, 10_000, 10_000)
            .expect("timeline");
        let suspended: Vec<_> = rows
            .iter()
            .filter(|row| row.snapshot == ActivitySnapshot::suspended())
            .collect();
        assert_eq!(
            suspended.len(),
            1,
            "the gap is owed until it is written, not until it is merely offered: {rows:?}"
        );
        assert_eq!(
            (suspended[0].started_at, suspended[0].ended_at),
            (1_010, 5_000)
        );
    }

    #[test]
    fn a_stored_gap_is_never_offered_to_the_store_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let mut store = FlakyGapStore::new(inner, 0);
        let source = ScriptedWindowSource::new(Vec::new());
        let blacklist = Blacklist::default();
        let mut pending_gaps = PendingGaps::default();
        let gap = PoweredDownGap {
            started_at: 1_010,
            ended_at: 5_000,
        };

        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_000,
            Some(4_999),
            Some(gap),
            &mut pending_gaps,
        )
        .expect("the write succeeds");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_010,
            Some(5_009),
            None,
            &mut pending_gaps,
        )
        .expect("a poll with nothing new to report still succeeds");

        let rows = store
            .inner
            .timeline_between(0, 10_000, 10_000)
            .expect("timeline");
        let suspended = rows
            .iter()
            .filter(|row| row.snapshot == ActivitySnapshot::suspended())
            .count();
        assert_eq!(
            suspended, 1,
            "a poll that measured no new suspend must not write a second row: {rows:?}"
        );
    }

    #[test]
    fn a_second_stretch_measured_before_the_first_is_stored_gets_its_own_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let mut store = FlakyGapStore::new(inner, 1);
        let source = ScriptedWindowSource::new(Vec::new());
        let blacklist = Blacklist::default();
        let mut pending_gaps = PendingGaps::default();
        // The gap between them is exactly the poll that measured the second stretch: proof
        // the machine was running there, which is what rules out folding the two into one.
        let first = PoweredDownGap {
            started_at: 1_010,
            ended_at: 4_610,
        };
        let second = PoweredDownGap {
            started_at: 4_620,
            ended_at: 8_220,
        };

        capture_once(
            &mut store,
            &source,
            &blacklist,
            4_610,
            Some(4_609),
            Some(first),
            &mut pending_gaps,
        )
        .expect_err("the first stretch's write fails");
        capture_once(
            &mut store,
            &source,
            &blacklist,
            8_220,
            Some(8_219),
            Some(second),
            &mut pending_gaps,
        )
        .expect("both the owed stretch and the newly measured one are now written");

        let rows = store
            .inner
            .timeline_between(0, 10_000, 10_000)
            .expect("timeline");
        let suspended: Vec<_> = rows
            .iter()
            .filter(|row| row.snapshot == ActivitySnapshot::suspended())
            .collect();
        assert_eq!(
            suspended.len(),
            2,
            "neither stretch may be merged into the other or replace it: {rows:?}"
        );
        assert_eq!(
            (suspended[0].started_at, suspended[0].ended_at),
            (1_010, 4_610)
        );
        assert_eq!(
            (suspended[1].started_at, suspended[1].ended_at),
            (4_620, 8_220)
        );

        let total_suspended: i64 = suspended
            .iter()
            .map(|row| row.ended_at - row.started_at)
            .sum();
        assert_eq!(
            total_suspended,
            (4_610 - 1_010) + (8_220 - 4_620),
            "the total suspended time written must equal the total the clock reported"
        );
    }

    #[test]
    fn a_store_that_never_recovers_still_exhausts_the_failure_streak() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let mut store = FlakyGapStore::new(inner, u32::MAX);
        let source = ScriptedWindowSource::new(Vec::new());
        let blacklist = Blacklist::default();
        let mut pending_gaps = PendingGaps::default();
        let mut streak = FailureStreak::default();
        let gap = PoweredDownGap {
            started_at: 1_010,
            ended_at: 5_000,
        };

        capture_once(
            &mut store,
            &source,
            &blacklist,
            5_000,
            Some(4_999),
            Some(gap),
            &mut pending_gaps,
        )
        .expect_err("the database never frees up in this test");
        assert_eq!(streak.record_failure(), Some(1));

        for expected in 2..MAX_CONSECUTIVE_FAILURES {
            capture_once(
                &mut store,
                &source,
                &blacklist,
                5_000 + expected as i64,
                Some(4_999 + expected as i64),
                None,
                &mut pending_gaps,
            )
            .expect_err("a gap still owed keeps failing while the database refuses it");
            assert_eq!(streak.record_failure(), Some(expected));
        }

        capture_once(
            &mut store,
            &source,
            &blacklist,
            9_999,
            Some(9_998),
            None,
            &mut pending_gaps,
        )
        .expect_err("still refusing");
        assert_eq!(
            streak.record_failure(),
            None,
            "a store that never recovers must end the daemon rather than loop silently"
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
            let error = requested_day(&argument)
                .expect_err("should be rejected")
                .to_string();
            assert!(
                error.contains("YYYY-MM-DD"),
                "{argument:?} was rejected without saying what was expected: {error}"
            );
        }
    }

    #[test]
    fn an_argument_that_is_not_a_date_flag_is_named_in_the_error() {
        let error = requested_day(&["yesterday".to_string()])
            .expect_err("should be rejected")
            .to_string();
        assert_eq!(error, "unexpected argument: yesterday");
    }

    #[test]
    fn pruning_deletes_for_real_unless_a_dry_run_is_asked_for() {
        assert!(
            !prune_is_dry_run(&[]).expect("no argument"),
            "a bare prune is the working command, not a preview"
        );
        assert!(prune_is_dry_run(&["--dry-run".to_string()]).expect("dry run"));
    }

    #[test]
    fn an_unrecognised_prune_argument_is_refused_rather_than_ignored() {
        // Silently ignoring one would run the irreversible command while the caller believes
        // they asked for something narrower.
        for argument in [
            vec!["--dryrun".to_string()],
            vec!["--dry-run=yes".to_string()],
            vec!["--older-than".to_string(), "7".to_string()],
        ] {
            let error = prune_is_dry_run(&argument)
                .expect_err("should be rejected")
                .to_string();
            assert!(
                error.starts_with("unexpected argument"),
                "{argument:?} was not named in its own rejection: {error}"
            );
        }
    }

    #[test]
    fn a_single_segment_is_not_reported_in_the_plural() {
        assert_eq!(
            render_prune(&pruned(1, None)),
            "Deleted 1 activity segment.\n"
        );
        assert_eq!(
            render_preview(1),
            "1 activity segment is outside it. Nothing was deleted.\n"
        );
        assert_eq!(
            render_prune(&pruned(2, None)),
            "Deleted 2 activity segments.\n"
        );
        assert_eq!(
            render_preview(0),
            "0 activity segments are outside it. Nothing was deleted.\n"
        );
    }

    #[test]
    fn a_rewrite_that_could_not_finish_is_reported_beside_the_deletion_not_instead_of_it() {
        let report = render_prune(&pruned(7, Some("another process is reading it")));

        assert!(
            report.starts_with("Deleted 7 activity segments.\n"),
            "the deletion has already committed and cannot be reported as a failure: {report}"
        );
        assert!(
            report.contains("still readable in the database file")
                && report.contains("another process is reading it")
                && report.contains("Running prune again"),
            "the reader has to learn what is left, why, and what clears it: {report}"
        );
    }

    #[test]
    fn the_help_text_documents_pruning_and_the_retention_window() {
        let output = run(Vec::<String>::new()).expect("help should succeed");

        assert!(output.contains("daytrace prune"), "{output}");
        assert!(output.contains("DAYTRACE_RETENTION_DAYS"), "{output}");
    }

    #[test]
    fn rejects_unknown_commands() {
        let error = run(["capture".to_string()]).expect_err("unknown command should fail");
        assert_eq!(
            error,
            AppError::Usage("unknown command: capture".to_string())
        );
    }
}
