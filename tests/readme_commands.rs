//! Runs every shell command shown in README.md against the built binary.
//!
//! README.md is the only place a person operating daytrace reads before typing something, so a
//! flag renamed in the code and not in the prose is a promise the guide no longer keeps. This
//! file holds no copy of the documented commands: the source of truth is README.md itself, read
//! fresh on every run, and a line this test cannot place into `Run` or `Skip` fails outright
//! rather than being silently ignored. That is the property that matters, not any particular
//! command: a line added to the guide later cannot escape coverage without a decision.

use regex::Regex;
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

/// Why a documented line is not executed by this test.
///
/// Each variant is a structural reason (what the command needs, not what it says), so a new
/// line matching an existing reason needs no change here, while anything else falls through to
/// `classify` returning `None`, which fails the test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SkipReason {
    NeedsCompositor,
    NeedsUserSession,
    Destructive,
    NotACommand,
    SelfReferentialGate,
}

enum Verdict {
    Run,
    Skip(SkipReason),
}

fn readme_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md")
}

/// Every non-empty logical line inside every ```sh fence, grouped by fence.
///
/// Grouped rather than flattened because a fence documents a sequence a reader runs one after
/// another in the same shell: `mkdir -p ~/.config/systemd/user` has to have already run before
/// the next line's redirect into that directory can succeed. Two different fences get two
/// independent sandboxes, so nothing in one leaks into another.
fn sh_fence_blocks(readme: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut lines = readme.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "```sh" {
            continue;
        }
        let mut block = Vec::new();
        for inner in lines.by_ref() {
            if inner.trim() == "```" {
                break;
            }
            let trimmed = inner.trim();
            if !trimmed.is_empty() {
                block.push(trimmed.to_string());
            }
        }
        blocks.push(block);
    }
    blocks
}

/// Strip leading `NAME=value` assignments, for the sole purpose of finding the executable.
///
/// The stripped result is used only to classify a line; the original, unstripped line is what
/// actually gets executed, so `DAYTRACE_RETENTION_DAYS=30 daytrace prune` is recognized as a
/// `daytrace prune` call and still runs with the override in effect.
fn strip_env_assignments(line: &str) -> &str {
    static ASSIGNMENT: OnceLock<Regex> = OnceLock::new();
    let assignment =
        ASSIGNMENT.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=\S*\s+").expect("regex"));

    let mut rest = line.trim_start();
    while let Some(found) = assignment.find(rest) {
        rest = &rest[found.end()..];
    }
    rest
}

/// Whether a line carries shell syntax this test has not handled.
///
/// Checked only for lines that would otherwise run, so a known-safe-to-read line that is
/// deliberately skipped for another reason (the destructive `rm`, the bare path) never has to
/// satisfy this grammar. `daytrace today; something-destructive` must not slip through on the
/// strength of its first word: this is what stops it, by rejecting the whole line before the
/// executable name is ever consulted for a verdict.
fn contains_disallowed_syntax(line: &str) -> bool {
    const DISALLOWED: [&str; 6] = [";", "&", "|", "`", "$(", "${"];
    if DISALLOWED.iter().any(|token| line.contains(token)) {
        return true;
    }
    if line.contains('<') {
        return true;
    }
    line.matches('>').count() > 1
}

/// Classify one logical line. `None` means neither `Run` nor a recognized `Skip`, which fails
/// the test that calls this: an unclassified line is the property this file exists to guard.
fn classify(line: &str) -> Option<Verdict> {
    if line.ends_with('\\') {
        // A continued line, which this test does not join back together.
        return None;
    }

    let core = strip_env_assignments(line);
    let mut words = core.split_whitespace();
    let first = words.next()?;

    if first.starts_with('$') {
        return Some(Verdict::Skip(SkipReason::NotACommand));
    }
    if first == "rm" {
        return Some(Verdict::Skip(SkipReason::Destructive));
    }
    if first == "systemctl" || first == "journalctl" {
        return Some(Verdict::Skip(SkipReason::NeedsUserSession));
    }
    if first == "bin/install-hooks" || first == "bin/ci" {
        return Some(Verdict::Skip(SkipReason::SelfReferentialGate));
    }

    let runnable = match first {
        "daytrace" => words.next() != Some("start"),
        "mkdir" => true,
        _ => return None,
    };
    if !runnable {
        return Some(Verdict::Skip(SkipReason::NeedsCompositor));
    }
    if contains_disallowed_syntax(line) {
        return None;
    }
    Some(Verdict::Run)
}

/// One fence's worth of isolated environment: its own working directory, home, XDG data
/// directory, and throwaway database, plus the built binary reachable on `PATH` as `daytrace`.
///
/// The working directory matters on its own: `daytrace export --date 2026-07-20 >
/// 2026-07-20.json` redirects into a file, and running that from the checkout would leave it
/// there untracked. A fresh temporary directory is where it lands instead.
struct Sandbox {
    _root: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    xdg_data_home: PathBuf,
    db_path: PathBuf,
    path_var: OsString,
}

impl Sandbox {
    fn new(daytrace_bin: &Path) -> Self {
        let root = tempfile::tempdir().expect("create a sandbox directory");
        let cwd = root.path().join("cwd");
        let home = root.path().join("home");
        let xdg_data_home = root.path().join("xdg-data");
        let bin_dir = root.path().join("bin");
        for dir in [&cwd, &home, &xdg_data_home, &bin_dir] {
            fs::create_dir_all(dir).expect("create a sandbox subdirectory");
        }
        symlink(daytrace_bin, bin_dir.join("daytrace"))
            .expect("link the built binary onto the sandbox PATH");

        let mut path_var = OsString::from(bin_dir.as_os_str());
        path_var.push(":");
        path_var.push(env::var_os("PATH").unwrap_or_default());

        Self {
            db_path: root.path().join("throwaway.db"),
            cwd,
            home,
            xdg_data_home,
            path_var,
            _root: root,
        }
    }

    /// Run a vetted line through a real shell, so `~` expansion, `NAME=value` prefixes and a
    /// trailing `>` redirection behave exactly as they would for someone following the guide.
    fn run(&self, line: &str) -> Output {
        Command::new("sh")
            .arg("-c")
            .arg(line)
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.xdg_data_home)
            .env("DAYTRACE_DB_PATH", &self.db_path)
            .env("PATH", &self.path_var)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("failed to spawn a shell for `{line}`: {error}"))
    }
}

#[test]
fn every_readme_shell_command_is_classified_and_every_run_line_succeeds() {
    let readme = fs::read_to_string(readme_path()).expect("read README.md");
    let daytrace_bin = PathBuf::from(env!("CARGO_BIN_EXE_daytrace"));

    let mut seen_skip_reasons = BTreeSet::new();
    let mut ran_at_least_one = false;

    for block in sh_fence_blocks(&readme) {
        let sandbox = Sandbox::new(&daytrace_bin);
        for line in block {
            match classify(&line) {
                Some(Verdict::Run) => {
                    ran_at_least_one = true;
                    let output = sandbox.run(&line);
                    assert!(
                        output.status.success(),
                        "README command `{line}` exited {:?}\nstdout: {}\nstderr: {}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                }
                Some(Verdict::Skip(reason)) => {
                    seen_skip_reasons.insert(reason);
                }
                None => panic!(
                    "README command `{line}` is neither a recognized runnable command nor a \
                     recognized reason to skip it; classify it before it can be trusted"
                ),
            }
        }
    }

    assert!(
        ran_at_least_one,
        "no README command actually ran; this gate would pass even on an empty guide"
    );

    let expected_skip_reasons = BTreeSet::from([
        SkipReason::NeedsCompositor,
        SkipReason::NeedsUserSession,
        SkipReason::Destructive,
        SkipReason::NotACommand,
        SkipReason::SelfReferentialGate,
    ]);
    assert_eq!(
        seen_skip_reasons, expected_skip_reasons,
        "the set of reasons README.md's commands are skipped for has changed; confirm the new \
         set is intentional"
    );
}
