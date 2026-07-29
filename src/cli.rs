use crate::activity::ActivitySnapshot;
use crate::config::Config;
use crate::desktop::HyprlandClient;
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

    while running.load(Ordering::Relaxed) {
        let observed_at = unix_now();
        let snapshot = if observed_at - input_activity.last_activity_at()
            >= config.idle_after.as_secs() as i64
        {
            Some(ActivitySnapshot::idle())
        } else {
            hyprland.active_snapshot(&config.blacklist)?
        };

        match snapshot {
            Some(snapshot) => store.record_observation(observed_at, &snapshot)?,
            None => store.close_open(observed_at)?,
        }

        wait_for_next_poll(&running, config.poll_interval);
    }

    store.close_open(unix_now())
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
    use super::run;

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
