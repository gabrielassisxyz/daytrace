//! Confirms README.md documents exactly the `DAYTRACE_*` variables the code reads.
//!
//! Neither side is a copy kept for this test. The code side comes from scanning the source
//! tree for quoted `DAYTRACE_*` string literals, the shape a name takes wherever it is actually
//! passed to `env::var` or one of its wrappers. The documented side comes from scanning README.md
//! itself. A variable added to one without the other fails here instead of drifting silently
//! until someone notices by hand.

mod _support;

use _support::code_declared_env_vars;
use regex::Regex;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

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
