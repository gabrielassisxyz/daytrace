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
        let is_idle =
            observed_at - input_activity.last_activity_at() >= config.idle_after.as_secs() as i64;

        match capture_once(
            &mut store,
            &hyprland,
            &config.blacklist,
            observed_at,
            is_idle,
        ) {
            Ok(()) => streak.record_success(),
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

fn capture_once(
    store: &mut Store,
    source: &dyn ActiveWindowSource,
    blacklist: &Blacklist,
    observed_at: i64,
    is_idle: bool,
) -> Result<(), String> {
    let snapshot = if is_idle {
        Some(ActivitySnapshot::idle())
    } else {
        source.active_snapshot(blacklist)?
    };

    match snapshot {
        Some(snapshot) => store.record_observation(observed_at, &snapshot),
        None => store.close_open(observed_at),
    }
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
    use super::{FailureStreak, MAX_CONSECUTIVE_FAILURES, capture_once, run};
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

        capture_once(&mut store, &source, &blacklist, 100, false)
            .expect_err("the compositor query failed");
        capture_once(&mut store, &source, &blacklist, 110, false)
            .expect("the recovered query is recorded");
        store.close_open(120).expect("close");

        let rows = store.timeline_between(0, 200, 200).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].started_at, 110);
        assert_eq!(rows[0].snapshot, window);
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
