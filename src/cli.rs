use crate::activity::ActivitySnapshot;
use crate::config::{Blacklist, Config};
use crate::desktop::{ActiveWindowSource, HyprlandClient};
use crate::input::InputActivity;
use crate::storage::Store;
use crate::timeline::{render_today, today_bounds, unix_now};
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
  daytrace today
  daytrace help

Commands:
  start   Start the desktop activity capture daemon.
  today   Print today's chronological activity timeline.
  help    Print this help text.

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
        [arg] if arg == "today" => {
            let config = Config::from_env()?;
            render_timeline(config)
        }
        [unknown] => Err(format!("unknown command: {unknown}")),
        _ => Err("expected exactly one command".to_string()),
    }
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

fn render_timeline(config: Config) -> Result<String, String> {
    if !config.db_path.exists() {
        return Ok("No activity events recorded for today.\n".to_string());
    }

    let store = Store::open(&config.db_path, config.secure_data_dir.clone())?;
    let now = unix_now();
    let (start, end) = today_bounds(now)?;
    let segments = store.timeline_between(start, end, now)?;
    render_today(&segments, now)
}

#[cfg(test)]
mod tests {
    use super::{FailureStreak, MAX_CONSECUTIVE_FAILURES, Observed, capture_once, run};
    use crate::activity::ActivitySnapshot;
    use crate::config::Blacklist;
    use crate::desktop::ActiveWindowSource;
    use crate::storage::Store;
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
    fn rejects_unknown_commands() {
        let error = run(["capture".to_string()]).expect_err("unknown command should fail");
        assert_eq!(error, "unknown command: capture");
    }
}
