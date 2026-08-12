//! Confirms the built-in help lists every `DAYTRACE_*` variable the code reads.
//!
//! The source side reuses the same scanner as the README gate, so the two cannot disagree
//! about what the supported set is. The help side is extracted from the real rendered help
//! output, so removing a name from the Environment block fails the test.

use std::process::{Command, Stdio};

mod _support;

use _support::code_declared_env_vars;

/// Every `DAYTRACE_*` name the rendered help lists under the Environment block.
///
/// The block is delimited by the `Environment:` header and a blank line, so names that appear
/// elsewhere in the help text (for example in future command examples) do not count toward the
/// Environment block contract.
fn help_documented_env_vars() -> Vec<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_daytrace"))
        .arg("help")
        .stdin(Stdio::null())
        .output()
        .expect("run daytrace help");

    let stdout = String::from_utf8_lossy(&output.stdout);

    let block = stdout
        .split("Environment:")
        .nth(1)
        .expect("help output contains an Environment block")
        .split("\n\n")
        .next()
        .expect("the Environment block ends before the next blank line");

    let pattern = regex::Regex::new(r"\bDAYTRACE_[A-Z0-9_]+\b").expect("valid regex");
    pattern
        .find_iter(block)
        .map(|found| found.as_str().to_string())
        .collect()
}

#[test]
fn help_lists_exactly_the_daytrace_env_vars_the_code_reads() {
    let code_vars = code_declared_env_vars();
    let help_vars: std::collections::BTreeSet<String> =
        help_documented_env_vars().into_iter().collect();

    let undocumented: Vec<_> = code_vars.difference(&help_vars).collect();
    let stale: Vec<_> = help_vars.difference(&code_vars).collect();

    assert!(
        undocumented.is_empty() && stale.is_empty(),
        "The built-in help and the code disagree about DAYTRACE_* variables.\n\
         read by the code but missing from the help Environment block: {undocumented:?}\n\
         listed in the help Environment block but never read by the code: {stale:?}"
    );
}
