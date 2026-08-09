//! Confirms README.md documents exactly the `DAYTRACE_*` variables the code reads.
//!
//! Neither side is a copy kept for this test. The code side comes from scanning the source
//! tree for quoted `DAYTRACE_*` string literals, the shape a name takes wherever it is actually
//! passed to `env::var` or one of its wrappers. The documented side comes from scanning
//! README.md itself. A variable added to one without the other fails here instead of drifting
//! silently until someone notices by hand.

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
fn code_declared_env_vars() -> BTreeSet<String> {
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

/// Every `DAYTRACE_*` name mentioned anywhere in README.md.
fn readme_documented_env_vars() -> BTreeSet<String> {
    let pattern = Regex::new(r"\bDAYTRACE_[A-Z0-9_]+\b").expect("valid regex");
    let readme_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    let readme = fs::read_to_string(&readme_path).expect("read README.md");

    pattern
        .find_iter(&readme)
        .map(|found| found.as_str().to_string())
        .collect()
}

#[test]
fn readme_documents_exactly_the_daytrace_env_vars_the_code_reads() {
    let code_vars = code_declared_env_vars();
    let readme_vars = readme_documented_env_vars();

    let undocumented: Vec<_> = code_vars.difference(&readme_vars).collect();
    let stale: Vec<_> = readme_vars.difference(&code_vars).collect();

    assert!(
        undocumented.is_empty() && stale.is_empty(),
        "README.md and the code disagree about DAYTRACE_* variables.\n\
         read by the code but missing from README.md: {undocumented:?}\n\
         documented in README.md but never read by the code: {stale:?}"
    );
}
