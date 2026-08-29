use crate::activity::{ActivitySnapshot, MediaSegment, TimelineSegment};
use crate::config::{Blacklist, Config, redact_address, redact_title};
use crate::desktop::{ActiveWindowSource, HyprlandClient};
use crate::export::render_day_export;
use crate::input::{InputActivity, InputObservation};
use crate::lock::CaptureLock;
use crate::media::{BusctlClient, MediaSource, PlayerOutcome, PlayingMedia};
use crate::service::render_user_unit;
use crate::session::{PowerGapWatch, PoweredDownGap, SessionClock, SystemSessionClock};
use crate::storage::{CaptureStore, Lane, Pruned, Store};
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
  daytrace forget --matching <text> [--dry-run]
  daytrace service unit
  daytrace help

Commands:
  start         Start the desktop and media activity capture daemon.
  today         Print a chronological activity timeline, by default for today.
  export        Print one day of stored activity as JSON, by default today.
  prune         Delete stored activity from before the retention window.
  forget        Delete stored activity whose app, title, artist, album or address matches text.
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
  DAYTRACE_MEDIA_POLL_SECONDS      Media polling interval, default 5.
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
            let (date, segments, media) = stored_day(&Config::from_env()?, requested)?;
            render_day(date, &segments, &media).map_err(AppError::from)
        }
        [arg, rest @ ..] if arg == "export" => {
            let requested = requested_day(rest)?;
            let (date, segments, media) = stored_day(&Config::from_env()?, requested)?;
            render_day_export(date, &segments, &media).map_err(AppError::from)
        }
        [arg, rest @ ..] if arg == "prune" => {
            let dry_run = prune_is_dry_run(rest)?;
            prune_old_activity(&Config::from_env()?, dry_run).map_err(AppError::from)
        }
        [arg, rest @ ..] if arg == "forget" => {
            let request = forget_request(rest)?;
            forget_matching_activity(&Config::from_env()?, &request).map_err(AppError::from)
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

/// What `forget`'s arguments named: the pattern to match, and whether it only wants a preview.
#[derive(Debug)]
struct ForgetRequest {
    pattern: String,
    dry_run: bool,
}

/// Parse `forget`'s arguments: a required `--matching TEXT`, and an optional `--dry-run` in
/// either position around it.
fn forget_request(args: &[String]) -> Result<ForgetRequest, AppError> {
    let pattern = match args {
        [flag, pattern] if flag == "--matching" => pattern,
        [flag, pattern, dry] if flag == "--matching" && dry == "--dry-run" => pattern,
        [dry, flag, pattern] if dry == "--dry-run" && flag == "--matching" => pattern,
        [flag] if flag == "--matching" => {
            return Err(AppError::Usage(
                "--matching needs a pattern, as --matching TEXT".to_string(),
            ));
        }
        [] => {
            return Err(AppError::Usage(
                "forget needs a pattern, as forget --matching TEXT".to_string(),
            ));
        }
        _ => {
            return Err(AppError::Usage(format!(
                "unexpected arguments: {}",
                args.join(" ")
            )));
        }
    };

    if pattern.is_empty() {
        return Err(AppError::Usage(
            "--matching needs a non-empty pattern".to_string(),
        ));
    }

    Ok(ForgetRequest {
        pattern: pattern.clone(),
        dry_run: args.iter().any(|arg| arg == "--dry-run"),
    })
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
    let media_source = BusctlClient::new();
    let input_activity = InputActivity::start(Arc::clone(&running), unix_now())?;
    let mut store = Store::open(&config.db_path, config.secure_data_dir.clone())?;
    store.close_stale_open_segments()?;

    let session_clock = SystemSessionClock;
    let sources = WakeSources {
        desktop: &hyprland,
        media: &media_source,
        session_clock: &session_clock,
    };
    let mut wake_state = CaptureWakeState::new(Instant::now());

    while running.load(Ordering::Relaxed) {
        wait_until(
            &running,
            wake_state.desktop_deadline.min(wake_state.media_deadline),
        );
        if !running.load(Ordering::Relaxed) {
            break;
        }
        run_capture_wake(
            &mut store,
            &sources,
            &config,
            input_activity.observation(),
            Instant::now(),
            &mut wake_state,
        )?;
    }

    close_capture_lanes(&mut store, unix_now())
}

/// Everything one wake carries over to the next: the failure streaks, the two rate-limited logs,
/// the gaps still owed, and the two poll deadlines. Gathered here, apart from the real
/// collaborators, so a test can construct one wake's memory without the signal handler, the
/// capture lock or the input-device watcher `run_daemon` sets up around it.
struct CaptureWakeState {
    power_gaps: PowerGapWatch,
    pending_gaps: PendingGaps,
    streak: FailureStreak,
    media_store_streak: FailureStreak,
    media_source_log: RateLimitedFailureLog,
    input_streak: FailureStreak,
    input_observation_log: RateLimitedFailureLog,
    desktop_deadline: Instant,
    media_deadline: Instant,
}

impl CaptureWakeState {
    /// Both deadlines start due, so the first wake polls each source once rather than waiting
    /// out a full interval before the first observation.
    fn new(now: Instant) -> Self {
        Self {
            power_gaps: PowerGapWatch::default(),
            pending_gaps: PendingGaps::default(),
            streak: FailureStreak::default(),
            media_store_streak: FailureStreak::default(),
            media_source_log: RateLimitedFailureLog::default(),
            input_streak: FailureStreak::default(),
            input_observation_log: RateLimitedFailureLog::default(),
            desktop_deadline: now,
            media_deadline: now,
        }
    }
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

/// Flush whatever `pending_gaps` still owes, on the desktop streak's own retry tolerance: a
/// flush failure is logged and retried on a later wake exactly as an ordinary desktop failure
/// is, rather than ending the daemon over the first busy write. What `capture_once` already does
/// as part of a desktop poll, pulled out so a media-only wake with something queued can run the
/// same flush, on the same streak, without a desktop poll alongside it.
fn flush_pending_gaps(
    store: &mut dyn CaptureStore,
    pending_gaps: &mut PendingGaps,
    streak: &mut FailureStreak,
) -> Result<(), String> {
    match pending_gaps.flush(store) {
        Ok(()) => Ok(()),
        Err(error) => match streak.record_failure() {
            Some(count) => {
                eprintln!(
                    "daytrace: observation failed ({count}/{MAX_CONSECUTIVE_FAILURES}): {error}"
                );
                Ok(())
            }
            None => Err(format!(
                "capture failed {MAX_CONSECUTIVE_FAILURES} times in a row, giving up: {error}"
            )),
        },
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
        None => store.close_open(observed_at, &Lane::Desktop)?,
    }
    Ok(observed)
}

/// Sleep until `deadline`, or until `running` turns false, whichever comes first.
///
/// Takes the deadline rather than an interval to sleep for, so a caller juggling more than one
/// source can wait for the earlier of several absolute deadlines in one call instead of picking
/// one interval to sleep on and starving whichever source is not it.
fn wait_until(running: &AtomicBool, deadline: Instant) {
    while running.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(200)));
    }
}

/// The first deadline strictly after `observed`, reached by advancing `previous_deadline` in
/// whole steps of `interval` rather than restarting the schedule from `observed`.
///
/// Pure, and takes the previous deadline rather than "now" plus an interval, because that is
/// what keeps the schedule from drifting by however long a poll took: an interval measured from
/// when the work finished, rather than from when it was due, pushes the whole schedule later by
/// the time the work spent. A poll that fell behind by more than one interval gets exactly one
/// deadline back, in the future, rather than every deadline it missed in between: replaying
/// those as a catch-up burst the instant capture recovers is the failure this avoids.
fn next_poll_deadline(
    previous_deadline: Instant,
    observed: Instant,
    interval: Duration,
) -> Instant {
    let mut deadline = previous_deadline;
    while deadline <= observed {
        deadline += interval;
    }
    deadline
}

/// What one media poll found out, kept apart because the two are unrelated failure domains: the
/// SOURCE answers whether the bus could be read at all, the STORE answers whether the write
/// landed, and a caller keeping one streak for both would end the daemon over an outage that no
/// player being on the bus produces just as reliably as a broken one.
struct MediaPollOutcome {
    source_error: Option<String>,
    store_error: Option<String>,
}

/// Poll media once and apply what it found through `store`.
///
/// A whole-source failure still has to reach the store: every open media lane has to be sealed
/// at its own `last_seen_at` rather than left open to bridge the dark interval as playback once
/// a later poll finds the same track again, so `close_open_media_lanes_at_last_seen` stands in
/// for `record_media_poll` on that path, and its own outcome is reported as the store error the
/// same way.
fn capture_media_once(
    store: &mut dyn CaptureStore,
    source: &dyn MediaSource,
    blacklist: &Blacklist,
    observed_at: i64,
) -> MediaPollOutcome {
    match source.poll(blacklist) {
        Ok(outcomes) => {
            let sanitized: Vec<PlayerOutcome> =
                outcomes.into_iter().map(sanitize_media_outcome).collect();
            MediaPollOutcome {
                source_error: None,
                store_error: store.record_media_poll(observed_at, &sanitized).err(),
            }
        }
        Err(error) => MediaPollOutcome {
            store_error: store.close_open_media_lanes_at_last_seen().err(),
            source_error: Some(error),
        },
    }
}

/// One media wake: poll, apply, and keep the two failure trackers in the shape the ordinary
/// daemon loop needs, right down to the `Err` that means "give up".
///
/// A function of its own, called from `run_daemon` and from a test the same way, because the
/// wiring between `capture_media_once`'s two-error result and each tracker is exactly the part
/// a passing test can silently stop covering the moment it is duplicated instead of shared: the
/// production loop and the test would each keep their own copy, and only one of them has to
/// drift for a red gate to say nothing.
fn handle_media_wake(
    store: &mut dyn CaptureStore,
    source: &dyn MediaSource,
    blacklist: &Blacklist,
    observed_at: i64,
    media_source_log: &mut RateLimitedFailureLog,
    media_store_streak: &mut FailureStreak,
) -> Result<(), String> {
    let outcome = capture_media_once(store, source, blacklist, observed_at);

    match outcome.source_error {
        Some(error) => {
            if let Some(count) = media_source_log.record_failure() {
                eprintln!("daytrace: media source failed ({count}): {error}");
            }
        }
        None => {
            if media_source_log.record_success() {
                eprintln!("daytrace: media source recovered");
            }
        }
    }

    match outcome.store_error {
        Some(error) => match media_store_streak.record_failure() {
            Some(count) => {
                eprintln!(
                    "daytrace: media store write failed ({count}/{MAX_CONSECUTIVE_FAILURES}): {error}"
                );
                Ok(())
            }
            None => Err(format!(
                "media store failed {MAX_CONSECUTIVE_FAILURES} times in a row, giving up: {error}"
            )),
        },
        None => {
            media_store_streak.record_success();
            Ok(())
        }
    }
}

/// The boundaries one wake reaches through, gathered so a test can hand over a fake for every
/// one of them without touching `run_daemon`'s own setup: the real signal handler, capture lock
/// and input-device watcher that nothing about a single wake needs.
struct WakeSources<'a> {
    desktop: &'a dyn ActiveWindowSource,
    media: &'a dyn MediaSource,
    session_clock: &'a dyn SessionClock,
}

/// The idle-since timestamp `capture_once` should use this desktop poll, or `Err` once input
/// cannot be observed at all for too long to keep going.
///
/// While no watcher is alive, `last_activity_at` is a frozen clock rather than evidence of a
/// quiet machine, so idle is never derived from it here: the caller gets `None`, which makes
/// `capture_once` record whatever the desktop source itself reports instead of manufacturing
/// AFK. The condition is logged rate-limited, the way a media source outage already is, and fed
/// into `streak` on the same threshold and shape the compositor's own sustained-failure path
/// uses, so a permanently lost input device ends capture instead of silently recording a day
/// that never happened.
fn idle_since_or_give_up(
    input: InputObservation,
    observed_at: i64,
    idle_after: Duration,
    streak: &mut FailureStreak,
    log: &mut RateLimitedFailureLog,
) -> Result<Option<i64>, String> {
    if !input.is_observing() {
        if let Some(count) = log.record_failure() {
            eprintln!("daytrace: no input device observable ({count}): idle cannot be verified");
        }
        return match streak.record_failure() {
            Some(_) => Ok(None),
            None => Err(format!(
                "no input device observable for {MAX_CONSECUTIVE_FAILURES} polls in a row, \
                 giving up"
            )),
        };
    }

    if log.record_success() {
        eprintln!("daytrace: input observation recovered");
    }
    streak.record_success();

    Ok(
        (observed_at - input.last_activity_at >= idle_after.as_secs() as i64)
            .then_some(input.last_activity_at),
    )
}

/// One wake of the capture loop: read the session clock, flush whatever gap is already owed,
/// then run whichever source is due. `run_daemon` reduces to acquiring the real desktop source,
/// media source, session clock and store, and calling this in a loop.
///
/// The gap flush runs before either source is given a chance to write: it is queued from the
/// session clock above, and then flushed either inside `capture_once` when the desktop poll is
/// due or explicitly here when it is not, in both cases strictly before that wake's media poll.
/// A gap flushed after a source had already written would let a media row move the gap floor
/// forward and silently truncate the very stretch the flush exists to record. The desktop and
/// media polls are otherwise independent: a desktop failure that does not exhaust its streak
/// falls through to the media check rather than returning early, so one source's outage never
/// suppresses the other's poll on the same wake.
fn run_capture_wake(
    store: &mut dyn CaptureStore,
    sources: &WakeSources,
    config: &Config,
    input: InputObservation,
    now: Instant,
    state: &mut CaptureWakeState,
) -> Result<(), String> {
    let desktop_due = now >= state.desktop_deadline;
    let media_due = now >= state.media_deadline;

    // The clock is read through the watch on every wake, whichever source is due, so a gap that
    // just ended is queued before either source's write below rather than only on a desktop
    // tick: the gap floor reads every lane, and a media row written first would move it forward
    // and truncate the gap that had just been measured.
    let session = state.power_gaps.observe(sources.session_clock);
    let observed_at = session.observed_at;
    if let Some(gap) = session.powered_down_gap {
        state.pending_gaps.push(gap);
    }

    if desktop_due {
        // Idle is only detectable once the threshold has already passed, so the transition is
        // dated back to the last input rather than to the moment it was noticed. Dating it to
        // now credited the whole threshold window to whichever window still held focus, which
        // inflated that one application by up to the threshold on every single absence.
        let idle_since = idle_since_or_give_up(
            input,
            observed_at,
            config.idle_after,
            &mut state.input_streak,
            &mut state.input_observation_log,
        )?;

        match capture_once(
            store,
            sources.desktop,
            &config.blacklist,
            observed_at,
            idle_since,
            // Already queued above: passing it again here would double-queue the same stretch.
            // `capture_once` flushes the queue itself, which is also what gives a failed flush
            // the same retry tolerance an ordinary desktop failure gets, through `state.streak`
            // below, rather than ending the daemon on the first busy write.
            None,
            &mut state.pending_gaps,
        ) {
            // Only a poll that actually reached the desktop is evidence that it is reachable.
            Ok(Observed::Desktop) => state.streak.record_success(),
            Ok(Observed::Idle) => {}
            Err(error) => match state.streak.record_failure() {
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
        state.desktop_deadline =
            next_poll_deadline(state.desktop_deadline, now, config.poll_interval);
    } else {
        // A media-only wake: nothing else is going to flush a queued gap this time around, so
        // it happens here instead, on the same retry tolerance a flush failure gets when
        // `capture_once` runs it.
        flush_pending_gaps(store, &mut state.pending_gaps, &mut state.streak)?;
    }

    if media_due {
        handle_media_wake(
            store,
            sources.media,
            &config.blacklist,
            observed_at,
            &mut state.media_source_log,
            &mut state.media_store_streak,
        )?;
        state.media_deadline =
            next_poll_deadline(state.media_deadline, now, config.media_poll_interval);
    }

    Ok(())
}

/// Close both lanes on the way out, so a shutdown mid-wake leaves neither open behind it: the
/// desktop lane at the moment of shutdown, and every open media lane at its own last-seen time.
fn close_capture_lanes(store: &mut dyn CaptureStore, closed_at: i64) -> Result<(), String> {
    store.close_open(closed_at, &Lane::Desktop)?;
    store.close_open_media_lanes_at_last_seen()
}

/// Redact a `Playing` outcome's free-text fields and address before it reaches storage.
///
/// The boundary a raw MPRIS reading crosses on its way into the store, mirroring where the
/// desktop side redacts a window title: `media.rs` stays pure parsing, so its fixtures assert
/// the bus's raw output, and every `MediaSource` implementation, staged fakes included, is
/// sanitized here rather than only the real one.
fn sanitize_media_outcome(outcome: PlayerOutcome) -> PlayerOutcome {
    match outcome {
        PlayerOutcome::Playing(media) => PlayerOutcome::Playing(PlayingMedia {
            title: media.title.as_deref().map(redact_title),
            artist: media.artist.as_deref().map(redact_title),
            album: media.album.as_deref().map(redact_title),
            item_url: media.item_url.as_deref().map(redact_address),
            ..media
        }),
        other => other,
    }
}

/// How often a failure that never ends the daemon on its own still earns a line in the log.
///
/// Used for both a media source outage and a stretch with no input watcher alive: unlike
/// `FailureStreak`, neither of those gives up by itself, so logging every poll of an outage with
/// no upper bound would fill the log at the poll interval for as long as it lasts. Only the
/// first failure and every `LOG_PERIOD`-th one after it get a line. A count rather than a
/// period, so the bound needs no clock seam to test.
const FAILURE_LOG_PERIOD: u32 = 60;

#[derive(Debug, Default)]
struct RateLimitedFailureLog {
    consecutive: u32,
}

impl RateLimitedFailureLog {
    /// The failure count worth logging, or `None` when this one is not.
    fn record_failure(&mut self) -> Option<u32> {
        self.consecutive += 1;
        (self.consecutive == 1 || self.consecutive.is_multiple_of(FAILURE_LOG_PERIOD))
            .then_some(self.consecutive)
    }

    /// Whether this success follows at least one failure, which is when a recovery line is
    /// owed: a source that was never broken has nothing to announce.
    fn record_success(&mut self) -> bool {
        let was_failing = self.consecutive > 0;
        self.consecutive = 0;
        was_failing
    }
}

/// The stored segments of the requested day, or of today when no day was requested.
///
/// A day that was never recorded reads as an empty day rather than an error, so a machine
/// that has not run the daemon yet gets an answer instead of a failure.
fn stored_day(
    config: &Config,
    requested: Option<NaiveDate>,
) -> Result<(NaiveDate, Vec<TimelineSegment>, Vec<MediaSegment>), String> {
    let now = unix_now();
    let date = match requested {
        Some(date) => date,
        None => local_date(now)?,
    };

    if !config.db_path.exists() {
        return Ok((date, Vec::new(), Vec::new()));
    }

    let store = Store::open(&config.db_path, config.secure_data_dir.clone())?;
    let (start, end) = day_bounds(date)?;
    // `now` still bounds a segment left open, which for a past day the query clips to the
    // end of that day. One read against one snapshot, so a desktop row and a media row never
    // describe an instant that never existed together.
    let (segments, media) = store.day_activity(start, end, now)?;
    Ok((date, segments, media))
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

/// Delete stored activity whose app class, title, artist, album or address contains `pattern`.
///
/// Shaped after `prune_old_activity`: name what would go, from the one query the preview and
/// the deletion both run, so a preview cannot disagree with the command it previews.
fn forget_matching_activity(config: &Config, request: &ForgetRequest) -> Result<String, String> {
    // Nothing stored is nothing to match, and opening a store to discover that would create
    // the database this command exists to trim.
    if !config.db_path.exists() {
        return Ok("No stored activity was found, so nothing was deleted.\n".to_string());
    }

    let mut store = Store::open(&config.db_path, config.secure_data_dir.clone())?;
    if request.dry_run {
        let removable = store.count_segments_matching(&request.pattern)?;
        return Ok(render_forget_preview(removable));
    }

    let forgotten = store.forget_segments_matching(&request.pattern)?;
    Ok(render_forget(&forgotten))
}

fn render_forget_preview(removable: u64) -> String {
    let are = if removable == 1 { "is" } else { "are" };
    format!(
        "{} {are} matched by it. Nothing was deleted.\n",
        segments(removable)
    )
}

/// What happened, and separately what could not be finished afterwards: the same split
/// `render_prune` reports, worded for `forget` rather than for the retention window.
fn render_forget(forgotten: &Pruned) -> String {
    let mut report = format!("Deleted {}.\n", segments(forgotten.deleted));
    if let Some(reason) = &forgotten.still_in_the_file {
        report.push_str(&format!(
            "The deleted activity is still readable in the database file, because {reason}. \
             Running forget again finishes clearing it.\n"
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
        AppError, CaptureWakeState, FailureStreak, MAX_CONSECUTIVE_FAILURES, Observed, PendingGaps,
        RateLimitedFailureLog, WakeSources, capture_media_once, capture_once, close_capture_lanes,
        flush_pending_gaps, forget_request, handle_media_wake, next_poll_deadline,
        prune_is_dry_run, render_forget, render_forget_preview, render_preview, render_prune,
        requested_day, run, run_capture_wake, sanitize_media_outcome,
    };
    use crate::activity::{ActivitySnapshot, TimelineSegment};
    use crate::config::{Blacklist, Config};
    use crate::desktop::ActiveWindowSource;
    use crate::input::InputObservation;
    use crate::media::fakes::ScriptedMediaSource;
    use crate::media::{MediaSource, PlayerOutcome, PlayingMedia};
    use crate::session::{ClockReading, PoweredDownGap, SessionClock};
    use crate::storage::CaptureStore;
    use crate::storage::Lane;
    use crate::storage::Pruned;
    use crate::storage::Store;
    use chrono::NaiveDate;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

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

        fn close_open(&mut self, ended_at: i64, lane: &Lane) -> Result<(), String> {
            self.inner.close_open(ended_at, lane)
        }

        fn record_media_poll(
            &mut self,
            observed_at: i64,
            outcomes: &[PlayerOutcome],
        ) -> Result<(), String> {
            self.inner.record_media_poll(observed_at, outcomes)
        }

        fn close_open_media_lanes_at_last_seen(&mut self) -> Result<(), String> {
            self.inner.close_open_media_lanes_at_last_seen()
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
        store.close_open(120, &Lane::Desktop).expect("close");

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
        store.close_open(1400, &Lane::Desktop).expect("close");

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

    // flush_pending_gaps: the media-only wake's own path to the same write, on the same streak.

    #[test]
    fn flush_pending_gaps_retries_a_busy_write_on_the_desktop_streak_rather_than_giving_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let mut store = FlakyGapStore::new(inner, 1);
        let mut pending_gaps = PendingGaps::default();
        let mut streak = FailureStreak::default();
        pending_gaps.push(PoweredDownGap {
            started_at: 1_010,
            ended_at: 5_000,
        });

        flush_pending_gaps(&mut store, &mut pending_gaps, &mut streak)
            .expect("a single busy write must not end the daemon");
        assert_eq!(
            streak.record_failure(),
            Some(2),
            "the first failed flush must count against the same streak an ordinary desktop \
             failure uses"
        );

        flush_pending_gaps(&mut store, &mut pending_gaps, &mut streak)
            .expect("the retried write succeeds once the database is free");
        let rows = store
            .inner
            .timeline_between(0, 10_000, 10_000)
            .expect("timeline");
        assert_eq!(
            rows.iter()
                .filter(|row| row.snapshot == ActivitySnapshot::suspended())
                .count(),
            1,
            "the queued gap must still be written once the retry succeeds: {rows:?}"
        );
    }

    #[test]
    fn flush_pending_gaps_gives_up_once_the_desktop_streak_is_exhausted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let mut store = FlakyGapStore::new(inner, u32::MAX);
        let mut pending_gaps = PendingGaps::default();
        let mut streak = FailureStreak::default();
        pending_gaps.push(PoweredDownGap {
            started_at: 1_010,
            ended_at: 5_000,
        });

        for count in 1..MAX_CONSECUTIVE_FAILURES {
            flush_pending_gaps(&mut store, &mut pending_gaps, &mut streak)
                .unwrap_or_else(|error| panic!("must not give up at failure {count}: {error}"));
        }
        let error = flush_pending_gaps(&mut store, &mut pending_gaps, &mut streak)
            .expect_err("the streak must give up at the same threshold desktop uses elsewhere");
        assert!(error.contains("capture failed"));
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
    fn forget_needs_a_matching_pattern_in_either_order_around_dry_run() {
        assert_eq!(
            forget_request(&["--matching".to_string(), "keepassxc".to_string()])
                .expect("a bare pattern")
                .pattern,
            "keepassxc"
        );

        let leading = forget_request(&[
            "--matching".to_string(),
            "keepassxc".to_string(),
            "--dry-run".to_string(),
        ])
        .expect("pattern then dry-run");
        assert_eq!(leading.pattern, "keepassxc");
        assert!(leading.dry_run);

        let trailing = forget_request(&[
            "--dry-run".to_string(),
            "--matching".to_string(),
            "keepassxc".to_string(),
        ])
        .expect("dry-run then pattern");
        assert_eq!(trailing.pattern, "keepassxc");
        assert!(trailing.dry_run);
    }

    #[test]
    fn forget_without_a_pattern_is_refused_rather_than_matching_everything() {
        let error = forget_request(&[]).expect_err("no pattern").to_string();
        assert!(error.contains("--matching"), "{error}");

        let error = forget_request(&["--matching".to_string()])
            .expect_err("a flag with nothing after it")
            .to_string();
        assert!(error.contains("--matching"), "{error}");

        let error = forget_request(&["--matching".to_string(), String::new()])
            .expect_err("an empty pattern would match every stored row")
            .to_string();
        assert!(error.contains("non-empty"), "{error}");
    }

    #[test]
    fn an_unrecognised_forget_argument_is_refused_rather_than_ignored() {
        for argument in [
            vec!["--matches".to_string(), "keepassxc".to_string()],
            vec![
                "--matching".to_string(),
                "keepassxc".to_string(),
                "--older-than".to_string(),
                "7".to_string(),
            ],
        ] {
            let error = forget_request(&argument)
                .expect_err("should be rejected")
                .to_string();
            assert!(
                error.starts_with("unexpected argument"),
                "{argument:?} was not named in its own rejection: {error}"
            );
        }
    }

    #[test]
    fn forget_reports_are_worded_for_forget_not_for_prune() {
        assert_eq!(
            render_forget_preview(1),
            "1 activity segment is matched by it. Nothing was deleted.\n"
        );
        assert_eq!(
            render_forget_preview(0),
            "0 activity segments are matched by it. Nothing was deleted.\n"
        );
        assert_eq!(
            render_forget(&pruned(3, None)),
            "Deleted 3 activity segments.\n"
        );

        let report = render_forget(&pruned(7, Some("another process is reading it")));
        assert!(
            report.starts_with("Deleted 7 activity segments.\n"),
            "the deletion has already committed and cannot be reported as a failure: {report}"
        );
        assert!(
            report.contains("still readable in the database file")
                && report.contains("another process is reading it")
                && report.contains("Running forget again"),
            "the reader has to learn what is left, why, and what clears it: {report}"
        );
    }

    #[test]
    fn the_help_text_documents_forgetting_by_pattern() {
        let output = run(Vec::<String>::new()).expect("help should succeed");

        assert!(output.contains("daytrace forget"), "{output}");
        assert!(output.contains("--matching"), "{output}");
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

    // Media polling: opens, progress, transitions and closes driven through the loop's own
    // wiring, not through storage directly.

    fn playing(bus_name: &str, title: &str) -> PlayerOutcome {
        PlayerOutcome::Playing(PlayingMedia {
            player_key: "spotify".to_string(),
            bus_name: bus_name.to_string(),
            title: Some(title.to_string()),
            artist: None,
            album: None,
            item_url: None,
        })
    }

    /// Read the row in a player's lane through a connection of its own, since `Store` keeps its
    /// own connection private and these tests only need to observe what landed on disk.
    fn media_row(
        db_path: &std::path::Path,
        bus_name: &str,
    ) -> Option<(Option<i64>, i64, String, Option<String>)> {
        rusqlite::Connection::open(db_path)
            .expect("open a read connection")
            .query_row(
                "SELECT ended_at, last_seen_at, title, item_url FROM activity_segments \
                 WHERE lane = ?1 ORDER BY id DESC LIMIT 1",
                rusqlite::params![format!("media:{bus_name}")],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .ok()
    }

    #[test]
    fn a_scripted_sequence_of_start_track_change_pause_and_disappearance_produces_the_right_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("daytrace.db");
        let mut store = Store::open(&db_path, None).expect("store");
        let blacklist = Blacklist::default();
        // Popped from the back: start, a track change, a pause, then gone from the bus.
        let source = ScriptedMediaSource::new(vec![
            Ok(vec![]),
            Ok(vec![PlayerOutcome::NotPlaying {
                bus_name: "spotify".to_string(),
            }]),
            Ok(vec![playing("spotify", "Track B")]),
            Ok(vec![playing("spotify", "Track A")]),
        ]);

        let start = capture_media_once(&mut store, &source, &blacklist, 100);
        assert_eq!((start.source_error, start.store_error), (None, None));
        let (ended_at, last_seen_at, title, _) =
            media_row(&db_path, "spotify").expect("a row after start");
        assert_eq!(
            (ended_at, last_seen_at, title.as_str()),
            (None, 100, "Track A")
        );

        capture_media_once(&mut store, &source, &blacklist, 110);
        let (ended_at, _, title, _) = media_row(&db_path, "spotify").expect("still one row");
        assert_eq!(
            (ended_at, title.as_str()),
            (None, "Track B"),
            "a track change closes the old row and opens the next, never leaving both open"
        );

        capture_media_once(&mut store, &source, &blacklist, 120);
        let (ended_at, last_seen_at, _, _) = media_row(&db_path, "spotify").expect("closed row");
        assert_eq!(
            (ended_at, last_seen_at),
            (Some(120), 120),
            "a pause closes at the poll instant"
        );

        // Gone entirely: the empty result means nothing to do, since spotify is already closed.
        capture_media_once(&mut store, &source, &blacklist, 130);
        let unchanged = media_row(&db_path, "spotify").expect("row still there, still closed");
        assert_eq!(
            unchanged.0,
            Some(120),
            "an already-closed lane is untouched by an empty poll"
        );
    }

    #[test]
    fn a_whole_source_failure_seals_open_lanes_and_recovery_opens_a_fresh_segment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("daytrace.db");
        let mut store = Store::open(&db_path, None).expect("store");
        let blacklist = Blacklist::default();
        // Popped from the back: playing, then a run of source failures, then a success with the
        // SAME track. The regression this guards: an unchanged snapshot advancing progress
        // would bridge the whole dark interval as one continuous segment.
        let source = ScriptedMediaSource::new(vec![
            Ok(vec![playing("spotify", "Track A")]),
            Err("busctl list failed".to_string()),
            Err("busctl list failed".to_string()),
            Ok(vec![playing("spotify", "Track A")]),
        ]);

        capture_media_once(&mut store, &source, &blacklist, 100);
        let after_open = media_row(&db_path, "spotify").expect("open row");
        assert_eq!(after_open.1, 100, "last_seen_at at the open");

        let first_failure = capture_media_once(&mut store, &source, &blacklist, 110);
        assert!(first_failure.source_error.is_some());
        let sealed_once = media_row(&db_path, "spotify").expect("sealed row");
        assert_eq!(
            sealed_once.0,
            Some(100),
            "a whole-source failure seals at the last successful read (100), never at the \
             failed poll's own instant (110)"
        );

        let second_failure = capture_media_once(&mut store, &source, &blacklist, 200);
        assert!(second_failure.source_error.is_some());
        let still_sealed = media_row(&db_path, "spotify").expect("still one sealed row");
        assert_eq!(
            still_sealed.0,
            Some(100),
            "a second failure re-seals the same instant"
        );

        let recovery = capture_media_once(&mut store, &source, &blacklist, 500);
        assert_eq!((recovery.source_error, recovery.store_error), (None, None));

        let rows: Vec<(i64, Option<i64>)> = rusqlite::Connection::open(&db_path)
            .expect("open a read connection")
            .prepare(
                "SELECT started_at, ended_at FROM activity_segments \
                 WHERE lane = 'media:spotify' ORDER BY id",
            )
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        assert_eq!(
            rows,
            vec![(100, Some(100)), (500, None)],
            "the recovery must open a NEW segment starting at 500, not extend the sealed one: \
             two segments with an unknown gap between them, not one continuous row bridging the \
             whole outage"
        );
    }

    #[test]
    fn free_text_fields_and_the_address_are_redacted_before_they_reach_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("daytrace.db");
        let mut store = Store::open(&db_path, None).expect("store");
        let blacklist = Blacklist::default();
        let source =
            ScriptedMediaSource::new(vec![Ok(vec![PlayerOutcome::Playing(PlayingMedia {
                player_key: "brave".to_string(),
                bus_name: "org.mpris.MediaPlayer2.brave".to_string(),
                title: Some("prefix token=secret-marker more text".to_string()),
                artist: Some("password=secret-marker https://x.test/y".to_string()),
                album: Some("key=secret-marker".to_string()),
                item_url: Some(
                    "https://host.test/cb?access_token=secret-marker&list=RD1".to_string(),
                ),
            })])]);

        let outcome = capture_media_once(&mut store, &source, &blacklist, 100);
        assert_eq!((outcome.source_error, outcome.store_error), (None, None));

        let (title, artist, album, item_url): (String, String, String, String) =
            rusqlite::Connection::open(&db_path)
                .expect("open a read connection")
                .query_row(
                    "SELECT title, artist, album, item_url FROM activity_segments \
                 WHERE lane = 'media:org.mpris.MediaPlayer2.brave'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("stored row");

        for field in [&title, &artist, &album, &item_url] {
            assert!(
                !field.contains("secret-marker"),
                "the marker must never reach storage: {field}"
            );
        }
        assert!(
            title.contains("more text"),
            "ordinary text survives: {title}"
        );
        assert!(
            item_url.contains("list=RD1"),
            "an unmatched parameter survives: {item_url}"
        );
        assert!(
            item_url.contains("access_token=[redacted]"),
            "a sensitive parameter is redacted, keeping the query shape: {item_url}"
        );
    }

    #[test]
    fn sanitize_leaves_a_not_playing_or_failed_outcome_untouched() {
        let not_playing = PlayerOutcome::NotPlaying {
            bus_name: "spotify".to_string(),
        };
        assert_eq!(sanitize_media_outcome(not_playing.clone()), not_playing);

        let failed = PlayerOutcome::Failed {
            bus_name: "spotify".to_string(),
            error: "timed out".to_string(),
        };
        assert_eq!(sanitize_media_outcome(failed.clone()), failed);
    }

    #[test]
    fn desktop_segments_are_identical_whether_or_not_media_was_playing_alongside_them() {
        let window = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );
        let blacklist = Blacklist::default();

        let run_desktop = |media_outcomes: Vec<Result<Vec<PlayerOutcome>, String>>| {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
            let desktop_source =
                ScriptedWindowSource::new(vec![Ok(Some(window.clone())), Ok(Some(window.clone()))]);
            let media_source = ScriptedMediaSource::new(media_outcomes);
            let mut pending_gaps = PendingGaps::default();

            capture_once(
                &mut store,
                &desktop_source,
                &blacklist,
                100,
                None,
                None,
                &mut pending_gaps,
            )
            .expect("desktop poll one");
            capture_media_once(&mut store, &media_source, &blacklist, 100);
            capture_once(
                &mut store,
                &desktop_source,
                &blacklist,
                110,
                None,
                None,
                &mut pending_gaps,
            )
            .expect("desktop poll two");
            capture_media_once(&mut store, &media_source, &blacklist, 110);
            store.close_open(120, &Lane::Desktop).expect("close");

            store.timeline_between(0, 200, 200).expect("timeline")
        };

        let with_media = run_desktop(vec![
            Ok(vec![playing("spotify", "Track A")]),
            Ok(vec![playing("spotify", "Track A")]),
        ]);
        let without_media = run_desktop(vec![Ok(vec![]), Ok(vec![])]);

        assert_eq!(
            with_media, without_media,
            "the desktop timeline must not depend on whether media was playing alongside it"
        );
    }

    #[test]
    fn next_poll_deadline_does_not_drift_when_a_poll_runs_long() {
        let start = std::time::Instant::now();
        let interval = Duration::from_secs(2);
        // The poll ran until 5 seconds past the previous deadline: the next one must be the
        // next multiple of the interval after that (6s), not 5s + interval (7s), which is what
        // scheduling from "now" instead of from the missed deadline would produce.
        let observed = start + Duration::from_secs(5);
        let next = next_poll_deadline(start, observed, interval);
        assert_eq!(next, start + Duration::from_secs(6));
    }

    #[test]
    fn next_poll_deadline_skips_missed_ticks_rather_than_replaying_them_as_a_burst() {
        let start = std::time::Instant::now();
        let interval = Duration::from_secs(1);
        // Ten intervals' worth of time passed in one stall: exactly one deadline comes back, in
        // the future, not every deadline that was missed along the way.
        let observed = start + Duration::from_secs(10);
        let next = next_poll_deadline(start, observed, interval);
        assert_eq!(next, start + Duration::from_secs(11));
    }

    #[test]
    fn the_earlier_of_two_deadlines_is_reached_the_documented_number_of_times_with_no_extra_query()
    {
        // Ten-second desktop interval, two-second media interval: four media-only deadlines
        // occur before the next desktop query, with no desktop query in between. Advanced
        // purely through `next_poll_deadline`, with no real sleep.
        let start = std::time::Instant::now();
        let desktop_interval = Duration::from_secs(10);
        let media_interval = Duration::from_secs(2);
        let mut desktop_deadline = start;
        let mut media_deadline = start;
        let mut desktop_polls = 0;
        let mut media_polls = 0;

        for _ in 0..5 {
            let now = desktop_deadline.min(media_deadline);
            let desktop_due = now >= desktop_deadline;
            let media_due = now >= media_deadline;
            if desktop_due {
                desktop_polls += 1;
                desktop_deadline = next_poll_deadline(desktop_deadline, now, desktop_interval);
            }
            if media_due {
                media_polls += 1;
                media_deadline = next_poll_deadline(media_deadline, now, media_interval);
            }
        }

        assert_eq!(
            desktop_polls, 1,
            "only the very first wake is due for the desktop too"
        );
        assert_eq!(
            media_polls, 5,
            "every one of the five wakes is a media deadline"
        );
    }

    #[test]
    fn media_source_failures_are_logged_at_one_sixty_and_every_period_after_then_once_on_recovery()
    {
        let mut log = RateLimitedFailureLog::default();
        let logged: Vec<u32> = (1..=121_u32).filter_map(|_| log.record_failure()).collect();
        assert_eq!(
            logged,
            vec![1, 60, 120],
            "an outage with no upper bound must not log every failure, or the log fills at the \
             poll interval for as long as the outage lasts"
        );
        assert!(
            log.record_success(),
            "a success following a failure streak owes a recovery line"
        );
        assert!(
            !RateLimitedFailureLog::default().record_success(),
            "a source that was never broken has nothing to announce"
        );
    }

    /// A storage boundary whose media write, or media seal, fails a fixed number of times
    /// before delegating to a real store: the same shape `FlakyGapStore` gives the desktop
    /// side, staged for the media path instead.
    struct FlakyMediaStore {
        inner: Store,
        remaining_failures: u32,
    }

    impl FlakyMediaStore {
        fn new(inner: Store, remaining_failures: u32) -> Self {
            Self {
                inner,
                remaining_failures,
            }
        }

        fn maybe_fail(&mut self) -> Result<(), String> {
            if self.remaining_failures > 0 {
                self.remaining_failures -= 1;
                return Err("database is busy".to_string());
            }
            Ok(())
        }
    }

    impl CaptureStore for FlakyMediaStore {
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
            self.inner.record_powered_down_gap(started_at, ended_at)
        }

        fn close_open(&mut self, ended_at: i64, lane: &Lane) -> Result<(), String> {
            self.inner.close_open(ended_at, lane)
        }

        fn record_media_poll(
            &mut self,
            observed_at: i64,
            outcomes: &[PlayerOutcome],
        ) -> Result<(), String> {
            self.maybe_fail()?;
            self.inner.record_media_poll(observed_at, outcomes)
        }

        fn close_open_media_lanes_at_last_seen(&mut self) -> Result<(), String> {
            self.maybe_fail()?;
            self.inner.close_open_media_lanes_at_last_seen()
        }
    }

    #[test]
    fn a_media_store_that_never_recovers_exhausts_its_own_streak_and_ends_the_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let mut store = FlakyMediaStore::new(inner, u32::MAX);
        let source = ScriptedMediaSource::new(vec![Ok(vec![]); MAX_CONSECUTIVE_FAILURES as usize]);
        let blacklist = Blacklist::default();
        let mut media_source_log = RateLimitedFailureLog::default();
        let mut media_store_streak = FailureStreak::default();

        for count in 1..MAX_CONSECUTIVE_FAILURES {
            handle_media_wake(
                &mut store,
                &source,
                &blacklist,
                count as i64,
                &mut media_source_log,
                &mut media_store_streak,
            )
            .unwrap_or_else(|error| panic!("must not give up at failure {count}: {error}"));
        }

        let error = handle_media_wake(
            &mut store,
            &source,
            &blacklist,
            MAX_CONSECUTIVE_FAILURES as i64,
            &mut media_source_log,
            &mut media_store_streak,
        )
        .expect_err("the media store streak must give up at the same threshold desktop uses");
        assert!(error.contains("media store failed"));
    }

    #[test]
    fn sixty_desktop_source_failures_interleaved_with_successful_media_polls_still_end_the_daemon()
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let desktop_source =
            ScriptedWindowSource::new(vec![Err("hyprctl activewindow failed".to_string()); 60]);
        let blacklist = Blacklist::default();
        let mut pending_gaps = PendingGaps::default();
        let mut streak = FailureStreak::default();
        let mut media_source_log = RateLimitedFailureLog::default();
        let mut media_store_streak = FailureStreak::default();

        for count in 1..MAX_CONSECUTIVE_FAILURES {
            capture_once(
                &mut store,
                &desktop_source,
                &blacklist,
                count as i64,
                None,
                None,
                &mut pending_gaps,
            )
            .expect_err("desktop keeps failing");
            streak.record_failure();
            let media_source = ScriptedMediaSource::new(vec![Ok(vec![])]);
            handle_media_wake(
                &mut store,
                &media_source,
                &blacklist,
                count as i64,
                &mut media_source_log,
                &mut media_store_streak,
            )
            .expect("a successful media poll beside a broken desktop must not give up either");
        }

        capture_once(
            &mut store,
            &desktop_source,
            &blacklist,
            MAX_CONSECUTIVE_FAILURES as i64,
            None,
            None,
            &mut pending_gaps,
        )
        .expect_err("still failing");
        assert!(
            streak.record_failure().is_none(),
            "sixty consecutive desktop failures must give up even though media kept succeeding"
        );
    }

    #[test]
    fn media_source_failures_interleaved_with_successful_desktop_polls_never_end_the_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let window = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );
        let blacklist = Blacklist::default();
        let mut pending_gaps = PendingGaps::default();
        let mut streak = FailureStreak::default();
        let mut media_source_log = RateLimitedFailureLog::default();
        let mut media_store_streak = FailureStreak::default();

        for count in 1..=121 {
            let desktop_source = ScriptedWindowSource::new(vec![Ok(Some(window.clone()))]);
            capture_once(
                &mut store,
                &desktop_source,
                &blacklist,
                count as i64,
                None,
                None,
                &mut pending_gaps,
            )
            .expect("the desktop keeps succeeding");
            streak.record_success();

            let media_source =
                ScriptedMediaSource::new(vec![Err("busctl list failed".to_string())]);
            handle_media_wake(
                &mut store,
                &media_source,
                &blacklist,
                count as i64,
                &mut media_source_log,
                &mut media_store_streak,
            )
            .unwrap_or_else(|error| {
                panic!("a media SOURCE failure alone must never give up: {error}")
            });
        }
    }

    #[test]
    fn repeated_media_store_failures_are_not_cleared_by_media_source_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let mut store = FlakyMediaStore::new(inner, 5);
        let blacklist = Blacklist::default();
        let mut media_source_log = RateLimitedFailureLog::default();
        let mut media_store_streak = FailureStreak::default();

        for count in 1..=5 {
            // The SOURCE succeeds every time; only the STORE write fails.
            let media_source = ScriptedMediaSource::new(vec![Ok(vec![])]);
            handle_media_wake(
                &mut store,
                &media_source,
                &blacklist,
                count,
                &mut media_source_log,
                &mut media_store_streak,
            )
            .expect("below the threshold, still logging rather than giving up");
        }
        assert_eq!(
            media_store_streak.record_failure(),
            Some(6),
            "five real store failures, none of them cleared by the source succeeding each time"
        );
    }

    #[test]
    fn a_successful_media_write_does_not_reset_the_desktop_failure_streak() {
        // A successful idle write proves the store recovered but says nothing about whether the
        // compositor did; a successful media write is the same case one lane over. `streak`
        // here stands in for `run_daemon`'s own desktop streak, untouched by `handle_media_wake`
        // because the two live in separate variables with no path between them.
        let mut streak = FailureStreak::default();
        streak.record_failure();
        streak.record_failure();

        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let blacklist = Blacklist::default();
        let mut media_source_log = RateLimitedFailureLog::default();
        let mut media_store_streak = FailureStreak::default();
        let media_source = ScriptedMediaSource::new(vec![Ok(vec![])]);
        handle_media_wake(
            &mut store,
            &media_source,
            &blacklist,
            100,
            &mut media_source_log,
            &mut media_store_streak,
        )
        .expect("a successful, empty media poll");

        assert_eq!(
            streak.record_failure(),
            Some(3),
            "the desktop streak must still read as three failures in a row: a media write \
             succeeding does not touch it"
        );
    }

    /// One call, one line, in call order, shared across every fake a wake test builds. An
    /// ordering guarantee is a statement about which call happened before which other call, and
    /// this is the only thing that can tell the two apart: two rows in a real store carry no
    /// record of which write reached it first.
    type CallLog = Rc<RefCell<Vec<&'static str>>>;

    /// A desktop boundary that logs the one call a wake gives it before answering with a fixed
    /// response.
    struct LoggingWindowSource {
        log: CallLog,
        response: RefCell<Option<Result<Option<ActivitySnapshot>, String>>>,
    }

    impl LoggingWindowSource {
        fn new(log: CallLog, response: Result<Option<ActivitySnapshot>, String>) -> Self {
            Self {
                log,
                response: RefCell::new(Some(response)),
            }
        }
    }

    impl ActiveWindowSource for LoggingWindowSource {
        fn active_snapshot(
            &self,
            _blacklist: &Blacklist,
        ) -> Result<Option<ActivitySnapshot>, String> {
            self.log.borrow_mut().push("desktop_source");
            self.response.borrow_mut().take().expect("polled once")
        }
    }

    /// A media boundary that logs the one call a wake gives it, the same way.
    struct LoggingMediaSource {
        log: CallLog,
        response: RefCell<Option<Result<Vec<PlayerOutcome>, String>>>,
    }

    impl LoggingMediaSource {
        fn new(log: CallLog, response: Result<Vec<PlayerOutcome>, String>) -> Self {
            Self {
                log,
                response: RefCell::new(Some(response)),
            }
        }
    }

    impl MediaSource for LoggingMediaSource {
        fn poll(&self, _blacklist: &Blacklist) -> Result<Vec<PlayerOutcome>, String> {
            self.log.borrow_mut().push("media_source");
            self.response.borrow_mut().take().expect("polled once")
        }
    }

    /// A storage boundary that logs every call it receives instead of keeping rows: the three
    /// ordering guarantees under test are entirely about call order, which two rows sharing a
    /// timestamp cannot distinguish.
    struct LoggingStore {
        log: CallLog,
    }

    impl LoggingStore {
        fn new(log: CallLog) -> Self {
            Self { log }
        }
    }

    impl CaptureStore for LoggingStore {
        fn record_observation(
            &mut self,
            _starts_at: i64,
            _seen_at: i64,
            _snapshot: &ActivitySnapshot,
        ) -> Result<(), String> {
            self.log.borrow_mut().push("record_observation");
            Ok(())
        }

        fn record_powered_down_gap(
            &mut self,
            _started_at: i64,
            _ended_at: i64,
        ) -> Result<(), String> {
            self.log.borrow_mut().push("record_powered_down_gap");
            Ok(())
        }

        fn close_open(&mut self, _ended_at: i64, lane: &Lane) -> Result<(), String> {
            self.log.borrow_mut().push(match lane {
                Lane::Desktop => "close_open_desktop",
                Lane::Media(_) => "close_open_media",
            });
            Ok(())
        }

        fn record_media_poll(
            &mut self,
            _observed_at: i64,
            _outcomes: &[PlayerOutcome],
        ) -> Result<(), String> {
            self.log.borrow_mut().push("record_media_poll");
            Ok(())
        }

        fn close_open_media_lanes_at_last_seen(&mut self) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push("close_open_media_lanes_at_last_seen");
            Ok(())
        }
    }

    /// A session clock that always answers the same reading. A wake test seeds any gap it needs
    /// straight into `CaptureWakeState.pending_gaps`, so `PowerGapWatch` never has an earlier
    /// reading to compare this one against and this never has to fabricate a transition.
    struct FixedSessionClock {
        wall: i64,
    }

    impl SessionClock for FixedSessionClock {
        fn read(&self) -> ClockReading {
            ClockReading {
                wall: self.wall,
                monotonic: Duration::ZERO,
                boottime: Duration::ZERO,
            }
        }
    }

    /// A `Config` carrying only the fields a wake reads: the poll intervals, the idle threshold
    /// and the blacklist. The path fields are unused by a wake and stay empty.
    fn wake_config() -> Config {
        Config {
            db_path: PathBuf::new(),
            secure_data_dir: None,
            idle_after: Duration::from_secs(300),
            poll_interval: Duration::from_secs(1),
            media_poll_interval: Duration::from_secs(5),
            retention_days: 90,
            blacklist: Blacklist::default(),
        }
    }

    /// An `InputObservation` with one watcher alive, the ordinary case every wake test that is
    /// not itself about input loss wants: `last_activity_at` is trustworthy and idle is derived
    /// from it normally.
    fn observing(last_activity_at: i64) -> InputObservation {
        InputObservation {
            last_activity_at,
            watchers_alive: 1,
        }
    }

    #[test]
    fn a_media_only_wake_flushes_a_pending_gap_before_polling_media() {
        let log: CallLog = Rc::new(RefCell::new(Vec::new()));
        let mut store = LoggingStore::new(Rc::clone(&log));
        let desktop = LoggingWindowSource::new(Rc::clone(&log), Ok(None));
        let media = LoggingMediaSource::new(Rc::clone(&log), Ok(vec![]));
        let clock = FixedSessionClock { wall: 1_000 };
        let config = wake_config();

        // Desktop is not due this wake; only media is. That is exactly the wake
        // `flush_pending_gaps` documents itself as existing for: nothing else is going to flush
        // the queued gap, so this wake has to.
        let mut state = CaptureWakeState::new(Instant::now() + Duration::from_secs(60));
        state.media_deadline = Instant::now();
        state.pending_gaps.push(PoweredDownGap {
            started_at: 900,
            ended_at: 950,
        });
        let sources = WakeSources {
            desktop: &desktop,
            media: &media,
            session_clock: &clock,
        };

        run_capture_wake(
            &mut store,
            &sources,
            &config,
            observing(1_000),
            Instant::now(),
            &mut state,
        )
        .expect("wake succeeds");

        assert_eq!(
            log.borrow().as_slice(),
            [
                "record_powered_down_gap",
                "media_source",
                "record_media_poll"
            ],
            "the gap already owed must be flushed before the media source is polled, and the \
             desktop source, not due this wake, must not be touched at all"
        );
    }

    #[test]
    fn a_desktop_failure_does_not_suppress_the_media_poll_on_the_same_wake() {
        let log: CallLog = Rc::new(RefCell::new(Vec::new()));
        let mut store = LoggingStore::new(Rc::clone(&log));
        let desktop =
            LoggingWindowSource::new(Rc::clone(&log), Err("hyprctl activewindow failed".into()));
        let media = LoggingMediaSource::new(Rc::clone(&log), Ok(vec![]));
        let clock = FixedSessionClock { wall: 1_000 };
        let config = wake_config();
        // Both due, so a wake that let the desktop failure cut it short would never reach media.
        let mut state = CaptureWakeState::new(Instant::now());
        let sources = WakeSources {
            desktop: &desktop,
            media: &media,
            session_clock: &clock,
        };

        run_capture_wake(
            &mut store,
            &sources,
            &config,
            observing(1_000),
            Instant::now(),
            &mut state,
        )
        .expect("a single desktop failure, well under the giving-up threshold, is not fatal");

        assert_eq!(
            log.borrow().as_slice(),
            ["desktop_source", "media_source", "record_media_poll"],
            "the media source must still be polled on a wake where the desktop source failed"
        );
    }

    #[test]
    fn idle_is_not_recorded_while_no_input_watcher_is_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let window = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );
        let desktop = ScriptedWindowSource::new(vec![Ok(Some(window.clone()))]);
        let media = ScriptedMediaSource::new(Vec::new());
        let clock = FixedSessionClock { wall: 100_000 };
        let config = wake_config();
        let mut state = CaptureWakeState::new(Instant::now());
        state.media_deadline = Instant::now() + Duration::from_secs(3600);
        let sources = WakeSources {
            desktop: &desktop,
            media: &media,
            session_clock: &clock,
        };
        // Frozen far enough in the past that a trusted timestamp would read as idle several
        // times over; zero watchers alive is what must stop that reading from being recorded.
        let input = InputObservation {
            last_activity_at: 0,
            watchers_alive: 0,
        };

        run_capture_wake(
            &mut store,
            &sources,
            &config,
            input,
            Instant::now(),
            &mut state,
        )
        .expect("a single missing-watcher wake is well under the giving-up threshold");
        store.close_open(200_000, &Lane::Desktop).expect("close");

        let rows = store
            .timeline_between(0, 200_000, 200_000)
            .expect("timeline");
        assert!(
            rows.iter()
                .all(|row| row.snapshot != ActivitySnapshot::idle()),
            "idle must never be recorded while no input watcher can vouch for it: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.snapshot == window),
            "the desktop's own report must still be recorded instead of manufactured AFK: \
             {rows:?}"
        );
    }

    #[test]
    fn sustained_input_loss_ends_capture_without_ever_recording_a_fabricated_idle_stretch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = Store::open(dir.path().join("daytrace.db"), None).expect("store");
        let window = ActivitySnapshot::window(
            Some("ghostty".to_string()),
            Some("tmux".to_string()),
            None,
            None,
        );
        let responses = (0..MAX_CONSECUTIVE_FAILURES)
            .map(|_| Ok(Some(window.clone())))
            .collect();
        let desktop = ScriptedWindowSource::new(responses);
        let media = ScriptedMediaSource::new(Vec::new());
        let clock = FixedSessionClock { wall: 100_000 };
        let config = wake_config();
        let mut state = CaptureWakeState::new(Instant::now());
        state.media_deadline = Instant::now() + Duration::from_secs(3600);
        let sources = WakeSources {
            desktop: &desktop,
            media: &media,
            session_clock: &clock,
        };
        let input = InputObservation {
            last_activity_at: 0,
            watchers_alive: 0,
        };

        for expected in 1..MAX_CONSECUTIVE_FAILURES {
            // Forced due on every iteration: this loop is proving the input-loss streak, not
            // the desktop poll schedule, and letting the schedule gate it would starve every
            // wake after the first since none of these calls actually waits out an interval.
            state.desktop_deadline = Instant::now();
            run_capture_wake(
                &mut store,
                &sources,
                &config,
                input,
                Instant::now(),
                &mut state,
            )
            .unwrap_or_else(|error| panic!("must not give up at wake {expected}: {error}"));
        }

        state.desktop_deadline = Instant::now();
        let error = run_capture_wake(
            &mut store,
            &sources,
            &config,
            input,
            Instant::now(),
            &mut state,
        )
        .expect_err("sustained input loss must end capture rather than keep guessing");
        assert!(
            error.contains("input device"),
            "the failure must name what gave up: {error}"
        );

        store.close_open(200_000, &Lane::Desktop).expect("close");
        let rows = store
            .timeline_between(0, 200_000, 200_000)
            .expect("timeline");
        assert!(
            rows.iter()
                .all(|row| row.snapshot != ActivitySnapshot::idle()),
            "no wake in this run may have recorded idle as fact: {rows:?}"
        );
    }

    #[test]
    fn shutdown_closes_both_the_desktop_and_the_media_lanes() {
        let log: CallLog = Rc::new(RefCell::new(Vec::new()));
        let mut store = LoggingStore::new(Rc::clone(&log));

        close_capture_lanes(&mut store, 1_000).expect("shutdown succeeds");

        assert_eq!(
            log.borrow().as_slice(),
            ["close_open_desktop", "close_open_media_lanes_at_last_seen"],
            "shutdown must close the desktop lane and every open media lane, not only one of \
             the two"
        );
    }
}
