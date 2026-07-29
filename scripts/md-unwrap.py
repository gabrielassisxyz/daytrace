#!/usr/bin/env python3
"""Collapse hard-wrapped Markdown prose to one line per paragraph.

WHY this exists: hard-wrapped prose is a per-file accident, not a convention. It
survives because every editor and every agent inherits the wrap of whatever it is
editing, so the style spreads by contact and never gets decided. The convention here
is soft wrap — one paragraph, one line — and this script is the one-time sweep that
makes the existing tree match it, so the check that enforces it afterwards starts
from a clean baseline instead of failing on every file.

WHY a script and not a regex: line breaks are semantic in more Markdown constructs
than they are decorative. A fenced block, a table row, a YAML frontmatter key and a
two-trailing-space hard break all look like "a short line" to a naive rewrap, and
joining any of them changes what the document *means*. Every one of those is a
guard below, and the guards are the actual content of this file.

Dry-run is the default; nothing is written without --write.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys

# Paths that are Markdown but not ours to reformat. Third-party corpora (the RAG
# clone tree), vendored skill libraries and their `references/` dumps are copies whose
# value depends on staying diffable against upstream — rewrapping them destroys that
# and buys nothing, since nobody writes in them.
SKIP_PARTS = {
    ".git", "node_modules", "_cloned", "archive", "vendor", ".venv",
    "dist", ".output", ".nuxt", "target", "pkg",
    "repos_raw", "repos_organized",
    "skills", "references", "research",
    ".claude", ".opencode", ".agents", ".codex", ".gemini",
    # Test fixtures are inputs to assertions, not prose. Reformatting one does not
    # "clean it up", it changes what the test tests — caught when a BOM fixture in
    # kernl re-parsed as a setext heading after being joined.
    "testdata", "test-data", "fixtures", "__fixtures__", "golden", "snapshots",
}

# A fork or vendored upstream is third-party: rewrapping its docs buys nothing and
# destroys the ability to diff against upstream. Origin URL cannot detect one — a fork
# sits under the same account as an original — so authorship is the signal.
#
# WHY the bar is this low. The question being answered is "has the user touched this
# repo at all", NOT "did they write most of it", and those need very different
# thresholds. A private notes repo with three commits, one of them under a second
# commit address that appears in no git config, sits at 33% and is unmistakably the
# user's; a fork of someone else's tool sits at 4% and an upstream OSS project with
# four figures of contributors sits at ~0%. Anything from a few percent up to a third
# is the same answer, so the cut goes in the empty range below it. At 0.5 the notes
# repo was silently skipped — the failure this constant is calibrated against.
OWN_COMMIT_THRESHOLD = 0.1

FENCE_RE = re.compile(r"^\s*(```|~~~)")
# A line that opens a block whose breaks are structural: heading, table row, list
# item, blockquote, thematic break, HTML, link-reference definition, footnote.
BLOCK_START_RE = re.compile(
    r"^\s*("
    r"#{1,6}\s"          # heading
    r"|\|"               # table row
    r"|[-*+]\s"          # bullet
    r"|\d+[.)]\s"        # ordered item
    r"|>"                # blockquote
    r"|(-{3,}|\*{3,}|_{3,})\s*$"   # thematic break
    r"|<"                # html block
    r"|\[[^\]]+\]:"      # link reference definition
    r")"
)
# Four-space (or tab) indent is an indented code block only outside a list; inside a
# list the same indent is a continuation. Handled at the call site, not here.
INDENTED_CODE_RE = re.compile(r"^(\t| {4,})")
# Two trailing spaces or a trailing backslash is an explicit <br>. Joining it would
# delete a line break the author asked for.
HARD_BREAK_RE = re.compile(r"(  +|\\)$")
# A run of `=` or `-` alone on the line under a paragraph is a setext heading
# underline — the paragraph above it is an <h1>/<h2>. It reads as prose to any rule
# based on the line's own content, so it needs its own guard: joining it silently
# demotes a heading to body text.
SETEXT_RE = re.compile(r"^\s*(=+|-+)\s*$")
# A lone `>` is the blank line *inside* a blockquote: it separates two paragraphs of
# the quote. It matches every "is this quote content" test by prefix, so without its
# own guard a multi-paragraph quote collapses into a single run-on paragraph.
QUOTE_BLANK_RE = re.compile(r"^\s*>\s*$")
QUOTE_MARKER_RE = re.compile(r"^\s*>\s?")
# GitHub renders `> [!NOTE]` and friends as a callout ONLY when the marker sits alone
# on its line, with the body on the lines below. Fold the body up onto it and GitHub
# stops rendering the callout entirely: it degrades to a plain blockquote with a
# literal "[!NOTE]" in the text. Nothing in the CommonMark spec says so and no local
# renderer reproduces it — this is GitHub-specific behaviour, which is exactly why it
# has to be encoded here rather than trusted to a render comparison.
ALERT_RE = re.compile(r"^\s*>\s*\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]\s*$", re.IGNORECASE)


def quote_body(line: str) -> str:
    """The content of a quoted line with its `>` marker removed."""
    return QUOTE_MARKER_RE.sub("", line).strip()


def is_prose(line: str) -> bool:
    """A line that carries no structural meaning of its own, so it may be joined."""
    return bool(line.strip()) and not BLOCK_START_RE.match(line) and not INDENTED_CODE_RE.match(line)


def unwrap(text: str) -> str:
    """Join consecutive prose lines within each block into a single line.

    Three block kinds wrap differently and each needs its own continuation rule:

    - a plain paragraph continues on any following prose line;
    - a *list item* continues on a following line that is indented under it and does
      not itself open a new item — that continuation must be folded INTO the item,
      never emitted on its own, or the text silently leaves the bullet it belongs to;
    - a *blockquote* continues on a following `>` line, whose marker is dropped when
      folding so the quote does not gain a stray `>` mid-sentence.

    The first line of every block keeps its original indentation. That is what makes
    a paragraph nested inside a list item stay nested instead of being flattened to
    column zero.
    """
    lines = text.split("\n")
    out: list[str] = []
    buf: list[str] = []
    kind = "plain"
    item_indent = 0
    in_fence = False
    fence_marker = ""
    # YAML frontmatter: only when `---` is the very first line of the file.
    in_frontmatter = bool(lines) and lines[0].strip() == "---"
    seen_frontmatter_start = False

    def flush(keep_tail: bool = False) -> None:
        """Join the buffered block. `keep_tail` preserves the last line's trailing
        hard-break marker: stripping it would turn an explicit <br> into a plain
        newline, which Markdown renders as a space — a silent content change."""
        if not buf:
            return
        joined = buf[0].rstrip()
        for extra in buf[1:]:
            body = quote_body(extra) if kind == "quote" else extra.strip()
            joined += " " + body
        if keep_tail:
            marker = HARD_BREAK_RE.search(buf[-1])
            if marker and not joined.endswith(marker.group(0)):
                joined += marker.group(0)
        out.append(joined)
        buf.clear()

    def indent_of(line: str) -> int:
        return len(line) - len(line.lstrip())

    def continues(line: str) -> bool:
        """Whether `line` belongs to the block currently being buffered."""
        if not buf or not line.strip():
            return False
        if kind == "quote":
            # A `>` prefix alone does not make the line joinable prose: a quote can
            # contain a list, a heading or a table, and those keep their own line
            # breaks. Strip the marker and judge what is actually underneath it —
            # without this, `> - item` folds into the quoted paragraph and the whole
            # list stops being a list.
            return bool(quote_body(line)) and is_prose(quote_body(line))
        if kind == "list":
            # A marker at ANY depth opens its own item — test the stripped line, not
            # the raw one. A nested item indented four spaces or a tab also matches
            # the indented-code pattern, so an earlier version treated it as code,
            # fell through to the indent test, and folded whole sub-lists into their
            # parent bullet.
            if BLOCK_START_RE.match(line.lstrip()):
                return False
            # Indented far below the marker is a code block or a deeper structure
            # inside the item, not wrapped prose.
            if indent_of(line) - item_indent >= 4:
                return False
            # An unindented prose line is Markdown's "lazy continuation" and still
            # belongs to the item.
            return indent_of(line) > item_indent or is_prose(line)
        return is_prose(line)

    for line in lines:
        if in_frontmatter:
            out.append(line)
            if line.strip() == "---":
                if seen_frontmatter_start:
                    in_frontmatter = False
                else:
                    seen_frontmatter_start = True
            continue

        fence = FENCE_RE.match(line)
        if fence:
            marker = fence.group(1)
            if not in_fence:
                flush()
                in_fence, fence_marker = True, marker
            elif marker == fence_marker:
                in_fence = False
            out.append(line)
            continue

        if in_fence:
            out.append(line)
            continue

        # A setext underline terminates the paragraph above it and stays on its own
        # line; the heading it forms depends on that break existing.
        if buf and SETEXT_RE.match(line):
            flush()
            out.append(line)
            continue

        # An alert marker keeps its own line, and so does whatever follows it.
        if ALERT_RE.match(line):
            flush()
            out.append(line)
            continue

        if continues(line):
            buf.append(line)
            if HARD_BREAK_RE.search(line):
                flush(keep_tail=True)
            continue

        flush()

        # Not a continuation, so this line opens something. Only the kinds that can
        # be wrapped get buffered; everything else passes through untouched.
        stripped = line.lstrip()
        if not line.strip() or FENCE_RE.match(line) or INDENTED_CODE_RE.match(line):
            out.append(line)
        elif QUOTE_BLANK_RE.match(line):
            out.append(line)
        elif stripped.startswith(">") and is_prose(quote_body(line)):
            kind, buf[:] = "quote", [line]
        elif re.match(r"([-*+]\s|\d+[.)]\s)", stripped):
            kind, item_indent, buf[:] = "list", indent_of(line), [line]
        elif is_prose(line):
            kind, buf[:] = "plain", [line]
            if HARD_BREAK_RE.search(line):
                flush(keep_tail=True)
        else:
            out.append(line)

    flush()
    return "\n".join(out)


def is_work_tree(path: str) -> bool:
    """True for any git working tree, including a linked worktree.

    Testing for a `.git` DIRECTORY silently excludes linked worktrees, where `.git`
    is a file pointing back at the real git dir. That made a batch sweep run against
    a temporary worktree report "0 files" for every repo and do nothing at all — the
    worst failure shape, since it looks like a clean result.
    """
    try:
        return subprocess.run(
            ["git", "-C", path, "rev-parse", "--is-inside-work-tree"],
            capture_output=True, text=True, timeout=30,
        ).stdout.strip() == "true"
    except (subprocess.TimeoutExpired, OSError):
        return False


def tracked_markdown(repo: str) -> list[str]:
    """Tracked .md paths only — anything git-ignored is out of scope by definition."""
    try:
        listed = subprocess.run(
            ["git", "-C", repo, "ls-files", "-z", "*.md"],
            capture_output=True, text=True, timeout=60, check=True,
        ).stdout
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError):
        return []
    out = []
    for rel in listed.split("\0"):
        if not rel.endswith(".md") or SKIP_PARTS & set(rel.split("/")):
            continue
        path = os.path.join(repo, rel)
        # A symlinked doc points at a file owned by another repo; editing through it
        # writes somewhere this run was never asked to touch.
        if os.path.islink(path) or not os.path.isfile(path):
            continue
        out.append(path)
    return out


def git_identities(repos: list[str]) -> set[str]:
    """Every commit address this machine's git is configured to sign as.

    Read from git rather than hardcoded, for two reasons: a person commonly commits
    under more than one address, and a repo authored under the second one would
    otherwise read as third-party and be skipped in silence. It also keeps a real
    address out of this file. The union spans every repo's local config, not just the
    global one — the second identity typically exists *only* as a per-repo override,
    so looking at one repo at a time cannot see it.
    """
    identities: set[str] = set()
    cmds = [["git", "config", "--global", "--get-all", "user.email"]]
    cmds += [["git", "-C", r, "config", "--local", "--get-all", "user.email"] for r in repos]
    for cmd in cmds:
        try:
            identities.update(
                subprocess.run(cmd, capture_output=True, text=True, timeout=30).stdout.split()
            )
        except (subprocess.TimeoutExpired, OSError):
            continue
    return identities


def is_own_repo(repo: str, identities: set[str]) -> bool:
    """True when this repo's history is predominantly the user's own work."""
    try:
        authors = subprocess.run(
            ["git", "-C", repo, "log", "--format=%ae"],
            capture_output=True, text=True, timeout=120, check=True,
        ).stdout.split()
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError):
        return False
    if not identities or not authors:
        return False
    mine = sum(1 for a in authors if a in identities)
    return mine / len(authors) >= OWN_COMMIT_THRESHOLD


def is_clean(repo: str) -> bool:
    try:
        dirty = subprocess.run(
            ["git", "-C", repo, "status", "--porcelain"],
            capture_output=True, text=True, timeout=60, check=True,
        ).stdout.strip()
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError):
        return False
    return not dirty


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("repos", nargs="*", help="repo paths (default: the current directory)")
    ap.add_argument("--write", action="store_true", help="apply the change (default: report only)")
    ap.add_argument("--allow-dirty", action="store_true",
                    help="write even if the repo has uncommitted changes")
    ap.add_argument("--include-forks", action="store_true",
                    help="also sweep repos whose history is mostly third-party")
    ap.add_argument("--check", action="store_true",
                    help="exit non-zero if any tracked Markdown is still hard-wrapped (for CI)")
    args = ap.parse_args()

    if args.check and args.write:
        ap.error("--check reports; --write changes. Pick one.")

    repos = args.repos or ["."]
    identities = git_identities(repos)
    total_files = total_paras = 0

    for repo in repos:
        if not is_work_tree(repo):
            print(f"skip {repo}: not a git working tree", file=sys.stderr)
            continue
        # --check never applies the fork heuristic. That heuristic answers "is this repo
        # mine to sweep?", which is the right question for a multi-repo --write pass and
        # the wrong one for a gate: --check runs against the single repo under test, and
        # its identity is not in doubt.
        #
        # It also could not answer it in CI even in principle. is_own_repo compares commit
        # authors against `git config user.email`, and a CI runner configures no identity —
        # so `identities` is empty, every repo reads as third-party, and the gate skipped
        # itself and reported success. It was an author-side check wearing a pipeline's
        # clothes: two hard-wrapped files reached master through a green tick.
        if args.check:
            pass
        elif not args.include_forks and not is_own_repo(repo, identities):
            print(f"skip {repo}: mostly third-party history (--include-forks to override)", file=sys.stderr)
            continue
        # WHY refuse a dirty tree: the sweep produces a diff touching every doc. Mixed
        # with uncommitted work it becomes unreviewable, and unreviewable is how a bad
        # join reaches master.
        if args.write and not args.allow_dirty and not is_clean(repo):
            print(f"skip {repo}: uncommitted changes (commit them, or pass --allow-dirty)", file=sys.stderr)
            continue

        changed_here = 0
        for path in tracked_markdown(repo):
            original = open(path, encoding="utf-8", errors="strict").read()
            rewritten = unwrap(original)
            if rewritten == original:
                continue
            joined = len(original.split("\n")) - len(rewritten.split("\n"))
            changed_here += 1
            total_files += 1
            total_paras += joined
            if args.write:
                open(path, "w", encoding="utf-8").write(rewritten)
            else:
                print(f"  {path}  (-{joined} lines)")
        if changed_here:
            print(f"{repo}: {changed_here} files")

    if args.check:
        if total_files:
            print(f"\n{total_files} file(s) still hard-wrapped — run scripts/md-unwrap.py --write")
            return 1
        print("markdown is soft-wrapped")
        return 0

    verb = "rewrote" if args.write else "would rewrite"
    print(f"\n{verb} {total_files} files, joining {total_paras} wrapped lines")
    if not args.write and total_files:
        print("dry run — pass --write to apply")
    return 0


if __name__ == "__main__":
    sys.exit(main())
