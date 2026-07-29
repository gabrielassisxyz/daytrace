use std::env;
use std::process::ExitCode;

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
";

fn main() -> ExitCode {
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
            Ok("daytrace start: desktop capture is not implemented yet.\n".to_string())
        }
        [arg] if arg == "today" => Ok("No activity events recorded for today.\n".to_string()),
        [unknown] => Err(format!("unknown command: {unknown}")),
        _ => Err("expected exactly one command".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::run;

    #[test]
    fn prints_help_without_args() {
        let output = run(Vec::<String>::new()).expect("help should succeed");
        assert!(output.contains("daytrace start"));
        assert!(output.contains("daytrace today"));
    }

    #[test]
    fn exposes_today_command() {
        let output = run(["today".to_string()]).expect("today should succeed");
        assert_eq!(output, "No activity events recorded for today.\n");
    }

    #[test]
    fn rejects_unknown_commands() {
        let error = run(["capture".to_string()]).expect_err("unknown command should fail");
        assert_eq!(error, "unknown command: capture");
    }
}
