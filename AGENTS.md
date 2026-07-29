# daytrace Agent Briefing

> Read before every interaction. Living spec: short, imperative. On every gotcha or decision, append one line here.

> **What it is:** `daytrace` is a local-first personal activity timeline logger for reconstructing where time went during the day. **Calibration:** Tier 2 · Phase: work. The project has sensitive local activity data and is intended to grow, but the current system is a single-desktop Rust CLI and daemon. **Review gate:** standard, implemented simply: before push or PR, spawn one fresh-context subagent to review the full branch diff once. No review panel, no multi-model quorum, no TDD phase checkpoints.

## Stack & Commands

- **Stack:** Rust 1.96, single binary crate.
- **Run:** `cargo run -- today`
- **Build:** `cargo build`
- **Test:** `cargo test`
- **Full gate:** `bin/ci`
- **Planning:** Use `br` for maintainer issues and `bv --robot-*` for graph triage. Never run bare `bv`. Keep the public planning surface in `ROADMAP.md`.

## Scope (current)

- **Current scope:** local Hyprland desktop activity capture plus idle periods, stored locally and printed as a daily timeline. Do not add cloud sync, screenshots, clipboard capture, browser content capture, dashboard UI, AI narrative generation, multi-user design, or non-desktop tracking without a present need and explicit decision.

## Privacy & Security

- Store data locally only.
- Do not capture screenshots, clipboard content, page content, or browser private/incognito windows.
- Redact sensitive URLs and tokens before storing browser-derived activity.
- Keep blacklist support for domains and applications in the first capture milestone.
- Make logs easy to delete and export.
- When touching capture, storage, redaction, filesystem paths, or browser/native messaging, flag the security risk and add a testable guard.

## Tests (TDD)

- Every feature is born with a test; every bugfix with a regression test.
- Tests run with one command, no manual setup, no secret credential. If it cannot run headless, it is wrong.
- Mock external desktop, browser, media, filesystem, and clock boundaries with named fakes, not inline stubs.
- Before saying done, run `bin/ci` and report the result.

## Small Releases

- Every commit on the default branch passes `bin/ci` and is ready to release.
- Closed work is committed before switching tasks; flag it if it has not been.
- Before push or PR, run the standard review gate: one fresh-context subagent reviews the full branch diff once.

## Public Text

- This repo is public from day one. Published files, commit messages, PR text, comments, and docs stay impersonal and do not name a person outside git author metadata.
- No assistant attribution, generated-by signatures, session narration, task-process narration, or internal tool/process references in public text.
- `bin/slop-guard` checks the tracked tree and PR text for machine-checkable prose leaks.
- `scripts/md-unwrap.py --check` enforces soft-wrapped Markdown.

## Git & Secrets

- Before any commit, show `git status` and `git diff --cached`; confirm no secret is staged. If one appears, stop and report it.
- Real secrets stay out of git. Commit examples only, with fake values.
- Run `bin/install-hooks` once after clone so `.githooks/pre-commit` and `.githooks/commit-msg` are active.

## Post-implementation Checklist

1. Commits small and well-described.
2. Refactoring candidates listed if the change was large.
3. Security risks flagged if capture, storage, redaction, filesystem, browser, or native messaging changed.
4. This spec updated if behavior, setup, or release flow changed.

<!-- BEGIN universal-principles v3 -->
## Working principles

- **The human defines the WHAT; the agent decides the HOW.** Don't wait for line-by-line dictation. Plan first for non-trivial tasks: show the plan + to-do list, wait for approval.
- **Think before coding — don't assume, don't hide confusion.** State assumptions explicitly; if multiple interpretations exist, present them — don't pick silently. If a simpler approach exists, say so and push back. If a task is impossible under the stated constraints, or info is missing, say so — don't guess. (For trivial tasks, use judgment; this is bias, not ritual.)
- **Surgical changes — touch only what you must.** Every changed line traces to the task. Don't "improve" adjacent code, reformat, or refactor what isn't broken; match existing style even if you'd do it differently. Flag unrelated dead code — don't delete it. Remove only the imports / variables / functions your own change orphaned.
- **Chesterton's Fence — find the problem before undoing the decision.** A config, a flag, a workaround that looks arbitrary is a **fence**: someone put it there, probably to fix something that is invisible to you *because the fence is working*. You arrive with no history, so absence of a visible reason is evidence of your ignorance, not of its uselessness. When your fresh measurement contradicts what the human vaguely remembers ("I changed this once, because of some problem"), **your measurement is the suspect first** — it may be measuring the case that *isn't* failing. Go find the original problem, then decide. *(A CIFS share was benchmarked with a big sequential `dd`, looked fast, and the local-disk download dir was "fixed" away — while the actual failure was random writes: par2, unrar, torrent piece-writes. Two wrong commits.)*
- **Goal-driven execution — define the success check, then loop to it.** Turn the task into something verifiable before coding: "add validation" → write tests for invalid inputs, then pass them; "fix the bug" → write a failing repro test, then pass it; "refactor X" → tests green before and after. For multi-step work, state a brief plan with a verify step each.
- **"Flaky" is not a diagnosis — test in the environment the thing actually runs in.** A component that fails *consistently* under automation is being **mis-invoked**, not being unreliable; "it works when I run it by hand" is not evidence that it works. The shell you test in has a TTY, a `$HOME`, an `ssh-agent`, an interactive stdin — the systemd unit, the CI job and the scripted harness have none of those, so a passing manual run can be testing a different program. Reproduce it *there* (start the unit, `env -u SSH_AUTH_SOCK`, `</dev/null`, `--dry-run` to print the real command line) before accepting "unstable" as a cause. **When a fix doesn't change the symptom, stop fixing and go look at what is actually being executed.** *(An interactive-mode flag with no TTY made one harness fail every review panel for weeks, written off as "flaky"; it was the wrong flag.)*
- **KISS — don't solve a problem you don't have yet.** Simplicity isn't "write less code"; it's not building for a need that doesn't exist. Let structure emerge from the code.
- **YAGNI & flat.** No preventive abstractions, no single-use interfaces. Interfaces for real boundaries only. Architecture is *extracted* once a pattern proves itself in real use — never designed up front for a user who doesn't exist yet. Need pulls architecture.
- **Order: make it work → make it right → make it fast** (Kent Beck), in that order. Most over-engineering is doing "right"/"fast" before a working thing exists to justify it.
- **Flag scope creep — a standing duty, not a suggestion.** When a solo tool starts being framed as a public / multi-user / multi-tenant / plugin-system / configurable-N-backends platform before a real, present need exists, STOP and ask: "Is this needed now?" Justify future-proofing against a need that exists *today*.
- **No silent decisions (comprehension debt).** Never make a silent architectural or design call — state it and record the rationale, so the reasoning is recoverable later.
- **Real decisions are presented in the chat, in isolation — never via popup.** When a design/architecture/scope/trade-off decision arises, surface it on its own: the options, what each means, pros/cons/trade-offs, and a recommendation — then decide together. Don't bury it mid-text or bundle it with other topics, and don't compress it into a quick-pick widget (e.g. AskUserQuestion) — the widget skips the reasoning and overlays the explanation. Widgets are for trivial short-answer picks only.
- **Long answers are written to be scanned, not read twice.** For recaps, status reports, batch reviews, plans, and any comparison of options: lead with the outcome in one line, then break the body into bullets and **bold** the load-bearing terms. Options are always a list — one option per bullet, the recommended one marked — never a paragraph the reader has to parse to find the choices. Reserve unbroken prose for short arguments; a wall of paragraphs costs more in re-reading than the structure would have cost in words.

## Git: branches, commits, PRs, comments

- **Ask the repo for its default branch; never assume one.** Repos differ — `master` and `main` are both common, often in the same person's account — and a wrong guess sends a PR to a branch that does not exist, or, worse, has you "fixing" a URL that was right all along. `git symbolic-ref --short refs/remotes/origin/HEAD | sed 's|^origin/||'`, or `gh repo view --json defaultBranchRef -q .defaultBranchRef.name`. Never commit directly to it: branch, then PR.
- **A new repo starts on `main`.** That is the preferred name, and `init.defaultBranch` is set to it, so `git init` produces it without anyone choosing. It settles new repos only: an existing one keeps the branch it has, because renaming breaks open PRs, CI filters, deploy hooks and every permalink into the tree, and buys nothing. The rule above still governs everything already in existence — ask, never assume.
- **Branches** — Conventional Branch (conventionalbranch.org): `<type>/<kebab-description>`, types `feature/`, `bugfix/`, `hotfix/`, `chore/`, `release/`, `docs/`.
- **Commits** — Conventional Commits (conventionalcommits.org): `<type>(scope): <description>`, types `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `ci`, `build`, `perf`, `style`. Breaking change → `!` after the type or a `BREAKING CHANGE:` footer.
- **Atomic commits** — one logical change per commit, each independently green and revertible. Never `git add .` blind; split unrelated changes.
- **Always work in your own worktree — mandatory, not conditional.** Parallel sessions are opened freely and nothing signals their existence to you, so a "check whether another session is here first" step can never be reliable — the honest answer is always "maybe". The only collision-proof arrangement is structural: keep the main working tree on the default branch as a clean reference and **never work in it** — before your first write (commit, branch, rebase, stash; read-only exploration is exempt), create your own worktree and do everything there: `git worktree add ../<repo>-<task> -b <your-branch> <origin>/<default-branch>`. Do this **whether or not** you believe another agent is running — that belief is exactly what you cannot verify. Report which worktree/branch you used; remove it once merged. Only the human can see all the open sessions.
- **Pull requests** — describe **what + why**. *What*: a 1–3 line summary. *Why* (the bulk): decisions, trade-offs, rejected alternatives. The diff shows the what; the PR explains why.
- **Comments** — always **WHY, not WHAT**: explain intent, never restate the obvious mechanics. Keep existing comments; they carry intent.

## Code style (baseline)

- Functions: 4–40 lines, one thing each (SRP). Files: under ~500 lines, split by responsibility.
- Names specific and unique — avoid `data`, `handler`, `Manager`, `util`.
- Explicit types. Early returns over nested ifs; max ~2 levels of indentation.
- Inject dependencies; wrap third-party libs behind a thin interface this project owns.
- No duplication — but don't extract *too early*. Tolerate duplication while the pattern is still forming; extract the abstraction *from* proven, repeated code, never ahead of it.
- **Refactoring is not automatic.** After a large feature, list refactoring candidates (files > ~500 lines, duplicated logic, long functions, hardcoded config) and ask before pruning — the human decides, the tests are the safety net. Consolidate when the thing works and the seams are obvious, not before.
<!-- END universal-principles v3 -->

## Common Hurdles

No ungated common hurdles are recorded yet.

When a gotcha appears, add it here only if no deterministic gate already catches it. A hurdle promoted to a gate is deleted from this section, not duplicated.
