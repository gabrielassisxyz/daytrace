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
- **Development tracker:** `local/DEVELOPMENT.md` maps the layered plan to the beads graph and carries the queue order and the open decisions. Read it before picking up work, and update it in the same session that closes a bead, opens one, or settles a decision. It is maintainer-facing, so bead IDs belong there and never in `ROADMAP.md`. If `local/` is absent the checkout simply has no maintainer notes, which is normal.

## Scope (current)

- **Current scope:** local Hyprland desktop activity capture plus idle periods, stored locally in SQLite and printed as a daily timeline. Browser window titles are still replaced wholesale before storage. That is a faithful implementation of the rule that used to stand where the Privacy section below now states its reversal, so it is queued to be undone rather than defended, and it is not a defect to be fixed on sight. Browser private/incognito detection is best-effort until a browser extension provides a structured signal. Do not add cloud sync, screenshots, clipboard capture, page-body capture, dashboard UI, AI narrative generation, multi-user design, or non-desktop tracking without a present need and explicit decision.

## Privacy & Security

- Store data locally only.
- Never capture screenshots, clipboard content, keystrokes, or the body of a page. These are out of scope permanently, not pending.
- Record what held the attention **by name**: window titles, media track titles and artists, and the address of what was playing. A tool that cannot say what consumed the largest block of the day does not answer the question it exists for, and for an ordinary window the same facts already sit in the browser's own history in more detail. They do not for a private window that best-effort detection failed to skip, and that residual is accepted rather than unnoticed.
- Two scans, one per kind of field, and they are not interchangeable. **Free text** (a title, an artist) keeps the existing one: an address inside it is replaced whole, and a `keyword=value` secret loses its value, prefixed spellings included. A field that **is** an address keeps the address and loses only the sensitive parameters (`token`, `key`, `secret`, `password`, `code`), in the query or the fragment. Running the free-text scan over an address would replace the whole value and refuse to say what was played, which is the point of storing it.
- Keep blacklist support for domains and applications. It is the mechanism that answers "not this one", open by default and closed case by case, and it is what a user reaches for instead of a redaction rule nobody can opt out of.
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

## Parallel Work

- **One bead, one worktree, one branch, one pull request.** Create the worktree with `bin/worktree new <type>/<desc>`, never with a bare `git worktree add`: only the tool links the git-ignored paths a worktree needs, `.beads` among them, and `br` does not search upwards, so a worktree made by hand carries no bead database and every command in the next bullet fails inside it. The branch is still named the Conventional Branch way, with the bead id riding in the description (`feature/daytrace-42-idle-gap`). The basename has to be unique across the machine, because `bin/ci` derives `CARGO_TARGET_DIR` from `basename "$PWD"` whenever the environment has not already set it: two worktrees that share a basename share a build directory, which reintroduces the cross-branch contamination that block exists to prevent.
- **Claim the bead before the first write.** `br coordination status` shows which beads are already held and which claims are stale; `br update <id> --claim` takes one atomically, setting assignee and `in_progress` in a single step. Spelling that out as a separate `--assignee` and `--status` pair leaves a window where two sessions both read the bead as free, which is the one case a claim exists for. The claim is attributed to the configured actor, so pass `br --actor <name> update <id> --claim` while more than one session is open, or every claim comes back under the same name. What the next session needs to know goes in `br comments` on the bead, never in a file in the tree.
- **Merges are serial, and the gate is re-run after the rebase.** A green `bin/ci` proves the branch against the default branch as it stood, so another branch landing in between invalidates it. Rebase onto the current default branch and run `bin/ci` again before merging.

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

- Functions: 4–40 lines, one thing each (SRP). Files: under ~500-750 lines of production code, split by responsibility. Rust keeps unit tests in the file they test, inside `#[cfg(test)] mod tests`, because a child module is what can reach the parent's private items without making them public for a test's sake. Those lines do not count against the guideline, or it would price a language convention as a defect and push tests away from what they test. Moving them is not even available here: a binary crate has no lib target, so nothing under `tests/` can import this crate, and the files there drive the built executable instead. A test module that becomes hard to scroll past moves to a sibling file with `#[cfg(test)] mod tests;`, which keeps the same access.
- Names specific and unique — avoid `data`, `handler`, `Manager`, `util`.
- Explicit types. Early returns over nested ifs; max ~2 levels of indentation.
- Inject dependencies; wrap third-party libs behind a thin interface this project owns.
- No duplication — but don't extract *too early*. Tolerate duplication while the pattern is still forming; extract the abstraction *from* proven, repeated code, never ahead of it.
- **Refactoring is not automatic.** After a large feature, list refactoring candidates (files > ~500 lines, duplicated logic, long functions, hardcoded config) and ask before pruning — the human decides, the tests are the safety net. Consolidate when the thing works and the seams are obvious, not before.
<!-- END universal-principles v3 -->

## Common Hurdles

- **A shared cargo target directory makes one branch's build answer for another's.** Where `build.target-dir` points every checkout at one directory, two worktrees of this package write the same artifact paths and the fingerprints do not tell them apart, so `cargo test` can pass without compiling the current sources and `CARGO_BIN_EXE_daytrace` can start a binary another branch built. `bin/ci` now builds under its own path, so the gate is safe. Anything run by hand is not: pass `CARGO_TARGET_DIR` yourself before trusting a bare `cargo test`, `cargo run`, or a binary out of `target/`.

- **`.beads` is one shared database, not per-branch state.** Every worktree reaches it through a symlink chain that ends in `local/`, so a claim, a comment or a close made inside a worktree is visible to every other session the moment it happens, and that immediacy is what makes it usable as the coordination channel. It is git-ignored, so none of it travels in the branch: nothing has to be merged, and equally a bead closed on a branch that never lands stays closed. It also means a checkout without `local/`, which is a normal checkout, has no database at all, so the claim protocol is maintainer-side and the project does not depend on it.

When a gotcha appears, add it here only if no deterministic gate already catches it. A hurdle promoted to a gate is deleted from this section, not duplicated.
