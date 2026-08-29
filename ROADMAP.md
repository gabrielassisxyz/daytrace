# Roadmap

The desktop and media layers are complete. Everything below this line is either what those layers already do, what a later layer would add, or something deliberately left undone with the reason it was left.

## What Exists

### Capture

- `daytrace start` records Hyprland active-window segments and AFK segments into a local SQLite database.
- Idle is dated from the last input rather than from the moment it was detected, so the idle threshold is not credited to whichever window still held focus.
- A stretch the machine spent suspended is stored as its own kind of segment, measured from the kernel's own count of suspended time rather than from the wall clock, so a powered-down gap is neither reported as time away from a running machine nor invented out of a clock correction at boot.
- A segment survives a crash. Its progress marker advances on every observation, so an interrupted stretch keeps the length it was last seen with instead of collapsing to nothing.
- A failed compositor query is a skipped sample rather than the end of the daemon. Only sustained failure stops capture, which keeps a permanently broken setup from looking like a working one.
- One capture daemon runs per database. A second `daytrace start` is refused and names the process already running.
- `daytrace service unit` prints a systemd user unit that runs the daemon for the desktop session.

### Media

- `daytrace start` polls the user D-Bus for whichever MPRIS player is playing, on its own interval, separate from the desktop poll, since a track change matters far less often than a window change.
- A played track's title, artist, album and address are stored in a lane of its own rather than folded into the desktop timeline, because media overlaps the desktop and can overlap itself.
- Without `busctl` or a reachable user bus, media polling fails, a rate-limited warning is logged, and desktop capture continues unaffected.

### Reading it back

- `daytrace today` groups the day's desktop activity into blocks, consecutive activity that stayed on one application, and prints them as a daily narrative with durations, then totals the day by application from those blocks. A foreign focus shorter than five seconds is credited to the block around it rather than reported as an interruption of its own. `daytrace today --raw` prints the pre-aggregation report instead, one row per stored segment unchanged from before this layer existed, which is what the swallowing rule is checked against by hand.
- A block names whichever media player overlapped it for at least a minute, but media never holds time in the timeline: the desktop lane alone decides where the day's seconds go, and `Media playing` remains the only place a media duration is stated.
- `daytrace today --date` and `daytrace export --date` report and export any past local day. A day that a clock change shortens or lengthens still meets its neighbours exactly.
- `daytrace export` emits one day as JSON on standard output, with a stable shape and instants carrying the local offset.
- Both commands report media apart from the desktop timeline, as a section and an array of their own; the two are never summed together, since media overlaps the desktop and itself and a combined total would claim more time than the day held.
- A usage error prints the usage block and exits 2; a failure while running prints only what went wrong and exits 1. A caller can tell a refused duplicate start from a mistyped flag.

### Privacy

- Storage is local and nothing leaves the machine.
- No screenshot, no clipboard content, no page content, and no keystroke: an input device is read for the timestamp of an event, never for a key, button or pointer value.
- Browser window titles are stored through the same scan as any other title: addresses and `keyword=value` secrets are redacted, and the page name is kept. Private and incognito browser windows are skipped where Hyprland exposes a recognizable private-mode marker. The detector is best-effort and desktop-only, so a missed private window writes the page name into the store; that is one residual, accepted rather than hidden. Media carries a second one: MPRIS has no private-window signal, so a media row from a browser names what was playing regardless of which window it played in, and the blacklists are how a user closes it.
- Application, title and domain blacklists are configurable, and a blacklisted class matches by substring so a short entry covers the reverse-DNS class a compositor reports.
- Stored activity has a documented retention window, applied by `daytrace prune` and by nothing else. A prune makes the deleted activity unreadable rather than merely unlisted, which takes rebuilding the file and checkpointing the log, both measured rather than assumed.

### Gates

- Formatting, linting with warnings denied, the full test suite, a dependency audit, secret scanning, Markdown wrapping and public prose hygiene all run from one command.
- Every command shown in a shell block in `README.md` is executed against a throwaway database and required to exit zero. Each command line is classified as runnable or skipped for a named reason, and an unclassified line fails the run, so a command added later cannot quietly go uncovered.
- The `DAYTRACE_*` variables the source reads are compared against the ones `README.md` documents, so a variable cannot be added without being documented.

## What Is Missing

Two layers, none started, each usable on its own once it exists. None is queued: they are one-line intentions rather than specifications, and a task with no acceptance criteria is one an implementer and a reviewer can disagree about with both being defensible. Each needs a design pass before it becomes work.

- **Browser.** A light extension sending the active tab, tab switches, a normalized domain and per-tab media state to the daemon over Native Messaging, with blacklist and redaction for sensitive domains. The extension is what would make private-window detection reliable, rather than the best-effort title marker used today. It would also add per-tab detail and domain normalization; the desktop layer already names the browser window by its title, using the same redaction scan as every other title. Once it lands, aggregation gains a third source to reconcile: an active tab and an active window are two claims on the same instant that can disagree, which today's single desktop-lane-holds-the-clock rule does not need to resolve.
- **End-of-day package.** A compact local activity package for an external summarizer to consume, keeping this repository independent of whatever consumes it.

## Settled, And Not Being Done

- **Screen lock and unlock are not captured.** Three sources were checked on the target desktop rather than assumed: the compositor exposes no lock state and a lock surface is neither a window nor a layer; a session query fails outright from a process that is not in a session cgroup, which is where the user unit runs; and the locked hint the remaining query would read is only set by a locker that publishes it, which the one in use does not, so it reads unlocked for the whole time the screen is locked. What is left is subscribing to session-manager signals over DBus, a large asynchronous dependency for a signal that says a lock was requested rather than that the screen is locked. Idle already records stepping away and a suspended stretch already records the machine being down, so the marginal fact is small. Aggregation was the other condition that would have brought this back: its design pass checked whether the narrative needed a locked desk told apart from an empty one and found that it did not, so both still read as `AFK`. This is revisited if a locker that publishes the hint is adopted.
- **Nothing prunes automatically.** Irreversible deletion of activity nobody asked to lose, at a moment nobody was present for, is the class of surprise this project treats as privacy-sensitive. The accepted cost is that an installation where the command is never run keeps everything, which a user timer answers on a schedule its owner chose.
- **No configuration file.** Environment variables carry the whole surface, and a file is added when they stop being enough rather than in anticipation.
- **Capture polls rather than subscribes.** A poll is a sample the daemon can miss; a subscription is a stream it can fall behind on. The interval is configurable and the reasoning is recorded where the loop lives.

## Out of Scope

- Dashboard UI.
- AI-generated narrative inside `daytrace`.
- Multi-user or multi-platform design.
- Cloud sync.
- Tracking beyond this desktop.
- Screenshots, clipboard capture, page content, and keystroke capture, which are out of scope permanently rather than pending.
