use std::path::Path;

/// How long a window of starts is judged over, and how many are allowed inside it.
///
/// WHY bounded rather than unlimited: the daemon exits on its own after sustained capture
/// failure, precisely so a permanently broken setup cannot masquerade as a working one. A
/// unit that restarts it forever would erase that signal. A unit that never restarts it
/// would hand back the problem this service exists to solve, since one transient fault
/// would end capture for the rest of the day in silence. Bounded starts keep both: a
/// compositor that comes back is recovered unattended, and a fault that keeps recurring
/// exhausts the budget and parks the unit in `failed`, where `systemctl --user status`
/// shows it. At this budget a genuinely broken capture surfaces within a few minutes.
///
/// The budget counts starts and not restarts, including the one the session performs and
/// every manual one, so the documented escape hatch is `systemctl --user reset-failed`.
const START_WINDOW: &str = "1h";
const STARTS_PER_WINDOW: u32 = 5;

/// Seconds to wait before restarting, long enough for a compositor to finish coming up.
const RESTART_DELAY_SECONDS: u32 = 10;

/// Render the systemd user unit that keeps capture running without anyone starting it.
///
/// `exec_path` has to be absolute, since systemd searches no PATH.
pub fn render_user_unit(exec_path: &Path) -> String {
    format!(
        "\
[Unit]
Description=daytrace desktop activity capture
# Capture is meaningless without the compositor it observes, and the query needs the
# session's instance signature, which the session publishes into the systemd user
# environment. Binding to the session also means a logout stops the daemon instead of
# leaving it to fail its way to the give-up path once per logout.
After=graphical-session.target
PartOf=graphical-session.target
# In [Unit], not [Service]: systemd drops StartLimitIntervalSec= from [Service] with only
# a log line, while accepting StartLimitBurst= there, which silently reduces the window to
# the manager default and turns the budget below into a far weaker guard than it reads as.
StartLimitIntervalSec={START_WINDOW}
StartLimitBurst={STARTS_PER_WINDOW}

[Service]
Type=simple
ExecStart={exec_path} start
Restart=on-failure
RestartSec={RESTART_DELAY_SECONDS}
# The daemon closes the segment still in progress from its signal handler, so the last
# block of the day keeps its real end only if the stop arrives as SIGTERM.
KillSignal=SIGTERM
TimeoutStopSec=10

[Install]
WantedBy=graphical-session.target
",
        exec_path = quote_exec_path(exec_path)
    )
}

/// Quote a binary path for `ExecStart`.
///
/// systemd word-splits the command line and expands `%` specifiers in it, so a path that
/// carries a space or a percent sign renders a unit that runs something other than the
/// binary that printed it: `/opt/%hbin/daytrace` resolves against the home directory, and
/// `/opt/50%off/daytrace` becomes `/opt/50<os-id>ff/daytrace`. Quoting settles the split,
/// doubling the percent escapes the specifier, and the backslash cases follow from quoting.
fn quote_exec_path(exec_path: &Path) -> String {
    let escaped = exec_path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::render_user_unit;
    use std::path::Path;

    fn unit() -> String {
        render_user_unit(Path::new("/home/user/.local/bin/daytrace"))
    }

    /// The directives systemd would actually read from one section.
    ///
    /// A substring search over the whole file is not enough to pin a directive down: a
    /// misplaced `StartLimitIntervalSec=` is dropped with only a log line, and a `WantedBy=`
    /// outside `[Install]` makes `systemctl enable` a silent no-op. Both leave the text
    /// present, so an assertion that only looks for the text passes while the unit no longer
    /// does what the assertion claims to guard.
    fn directives_in<'a>(unit: &'a str, section: &str) -> Vec<&'a str> {
        unit.lines()
            .skip_while(|line| line.trim() != section)
            .skip(1)
            .take_while(|line| !line.trim_start().starts_with('['))
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    fn assert_directive(unit: &str, section: &str, directive: &str) {
        assert!(
            directives_in(unit, section).contains(&directive),
            "{directive} belongs in {section}, where systemd will read it, but the rendered \
             unit has {section} as {:?}",
            directives_in(unit, section)
        );
    }

    #[test]
    fn the_unit_runs_the_binary_that_rendered_it() {
        assert_directive(
            &unit(),
            "[Service]",
            "ExecStart=\"/home/user/.local/bin/daytrace\" start",
        );
    }

    #[test]
    fn a_path_with_a_space_or_a_percent_still_runs_the_intended_binary() {
        let unit = render_user_unit(Path::new("/opt/50%off/my bin/daytrace"));

        assert_directive(
            &unit,
            "[Service]",
            "ExecStart=\"/opt/50%%off/my bin/daytrace\" start",
        );
    }

    #[test]
    fn the_unit_lives_and_dies_with_the_graphical_session() {
        let unit = unit();

        assert_directive(&unit, "[Unit]", "After=graphical-session.target");
        assert_directive(&unit, "[Unit]", "PartOf=graphical-session.target");
        assert_directive(&unit, "[Install]", "WantedBy=graphical-session.target");
    }

    /// These three directives are also the retry window the duplicate-start decision rests on:
    /// while a manual daemon holds the claim the unit is refused, retries inside the window, and
    /// parks in `failed` only once the budget is spent. Drift here and the unit either gives up
    /// before that daemon can exit, or stops surfacing a sustained fault.
    #[test]
    fn a_transient_failure_is_recovered_but_a_sustained_one_is_not() {
        let unit = unit();

        assert_directive(&unit, "[Service]", "Restart=on-failure");
        assert_directive(&unit, "[Unit]", "StartLimitIntervalSec=1h");
        assert_directive(&unit, "[Unit]", "StartLimitBurst=5");
        assert!(
            !unit.contains("Restart=always"),
            "restarting forever makes a permanently broken capture look like a working one"
        );
    }

    #[test]
    fn the_unit_stops_with_sigterm_so_the_open_segment_gets_a_real_end() {
        assert_directive(&unit(), "[Service]", "KillSignal=SIGTERM");
    }

    /// SETTLED DECISION: a duplicate `daytrace start` is refused with exit code 1.
    ///
    /// Combined with `Restart=on-failure` and a budget of five starts per hour, a manual
    /// daemon left running across a login makes the unit lose the race, retry inside the
    /// window, and park in `failed`. Declaring the refusal a success would silently leave
    /// capture down once the manual daemon exits, with no `systemctl --user --failed` entry
    /// to surface it. The unit itself does not change; what changes is that this behaviour
    /// stops being right by accident.
    #[test]
    fn duplicate_start_refusal_is_not_masked_as_success() {
        let unit = unit();

        assert!(
            !unit.contains("SuccessExitStatus="),
            "SuccessExitStatus= would mask the duplicate-start refusal as success"
        );
        assert!(
            !unit.contains("RestartPreventExitStatus="),
            "RestartPreventExitStatus= would stop the retry window the decision relies on"
        );
    }
}
