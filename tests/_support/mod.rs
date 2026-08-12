//! Shared helpers for finding the DAYTRACE_* environment variables the source reads.
//!
//! Kept in one place so every gate that compares the code against a published surface
//! uses the same definition of "supported variable" and cannot drift apart.

use regex::Regex;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Every `DAYTRACE_*` name the source tree passes as a literal argument.
///
/// A name only counts when it is immediately quote-delimited (`"DAYTRACE_FOO"`), which is what
/// an argument at a call site looks like, in `env::var("DAYTRACE_DB_PATH")` and equally in
/// `duration_from_env("DAYTRACE_POLL_SECONDS", 1)`. It excludes a name that merely appears
/// inside a longer string, such as the CLI's built-in help text, where the name is not
/// individually quoted and reading it as "code that reads this variable" would be wrong.
pub fn code_declared_env_vars() -> BTreeSet<String> {
    static PATTERN: &str = r#""(DAYTRACE_[A-Z0-9_]+)""#;
    let pattern = Regex::new(PATTERN).expect("valid regex");
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    let mut vars = BTreeSet::new();
    collect_env_vars(&src_dir, &pattern, &mut vars);
    vars
}

/// Walk into subdirectories rather than reading only the top level.
///
/// `src` is flat today, so a listing of it would find every variable. It is not required to stay
/// flat: a module that grows past what one file should hold becomes a directory, and a scan that
/// stopped at the top level would go on passing while every name inside it went undocumented.
/// A gate whose coverage silently depends on the shape of the tree is the kind that reports green
/// for the wrong reason.
fn collect_env_vars(directory: &Path, pattern: &Regex, vars: &mut BTreeSet<String>) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect_env_vars(&path, pattern, vars);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for capture in pattern.captures_iter(&source) {
            vars.insert(capture[1].to_string());
        }
    }
}
