# daytrace

`daytrace` is a local-first personal activity logger for reconstructing where time went during the day.

It is a small Rust CLI and daemon that records active-window changes on Hyprland, idle periods, and stretches the machine spent suspended, stores them locally, and prints a chronological daily timeline with durations. That is the whole of it today: the desktop is the only source, so what the day says about a browser is what a window title says about it, and nothing reports what was playing behind the window in focus.

## Privacy Controls

- Local storage only.
- No cloud sync.
- No screenshots.
- No clipboard capture.
- No page-content capture.
- No keystroke capture: an input device is read only for the timestamp of an event, to detect idle and AFK, never for a key, button, or pointer value.
- Sensitive URLs and tokens are redacted before storage.
- Browser window titles are redacted by default because they can contain page content.
- Browser private and incognito windows are skipped when Hyprland exposes a recognizable private-mode title marker.
- Domain and application blacklists exist from the first capture milestone.
- Stored activity has a retention window, the last `90` days by default, applied on demand by `daytrace prune` and never automatically.
- Logs are easy to delete and export.

## Installing

`daytrace` is not distributed as a package or a prebuilt binary; it is built from a checkout of this repository. `cargo build --release --locked` produces `target/release/daytrace`; put that binary, or a symlink to it, on `PATH`.

## Running by Hand

```sh
daytrace start
```

`daytrace start` runs the local capture loop. It polls Hyprland for the active window, watches `/dev/input/event*` for input activity timestamps, and stores segments in a local SQLite database.

AFK tracking requires read access to at least one `/dev/input/event*` device. If no readable input devices are available, `daytrace start` exits instead of recording misleading activity.

One capture process runs per database. A second `daytrace start` is refused and names the process already running, rather than doubling capture: each process reads its own configuration, so a blacklist set in one shell is invisible to the other, and the process without it would record what the other one exists to skip. The claim is a lock on `daytrace.db.lock`, held beside the database for as long as the daemon runs and released by the kernel when it exits, so an unclean shutdown leaves nothing to clean up. Two daemons on different databases are not duplicates and both run.

`daytrace` uses two exit codes. `2` means the invocation itself was bad: unknown command, mistyped flag, or invalid `--date` argument; stderr includes the usage block. `1` means the invocation was valid but failed while running: a duplicate `start`, no readable input device, an invalid environment value such as `DAYTRACE_IDLE_AFTER_SECONDS=abc`, or sustained capture failure. `0` means the command produced its output. Callers that need to distinguish a benign duplicate start from a bad invocation can branch on the code.

## Running from the systemd User Unit

Nothing is recorded while the daemon is not running, so a login that does not start it loses the day without saying so. `daytrace service unit` prints a systemd user unit for the current installation:

```sh
mkdir -p ~/.config/systemd/user
daytrace service unit > ~/.config/systemd/user/daytrace.service
systemctl --user daemon-reload
systemctl --user enable --now daytrace.service
```

`ExecStart` carries the fully resolved path of the binary that printed the unit, which is what the running process reports about itself. A binary installed as a symlink therefore renders the link target rather than the link, so read the printed `ExecStart` before enabling the unit, and render it again after the binary moves.

Inspect it, follow its output, and stop it with:

```sh
systemctl --user status daytrace.service
journalctl --user -u daytrace.service -f
systemctl --user stop daytrace.service
systemctl --user reset-failed daytrace.service
systemctl --user disable --now daytrace.service
```

The unit is wanted by `graphical-session.target`, so it starts with the desktop session and stops at logout. That is deliberate: the daemon reads the compositor, and the compositor is only reachable from inside that session. A session that never activates `graphical-session.target`, such as Hyprland launched directly without a session manager like `uwsm`, will not start the service on its own. Start it from the compositor configuration instead:

```ini
exec-once = systemctl --user start daytrace.service
```

Sustained capture failure ends the daemon on purpose, so that a permanently broken setup cannot look like a working one. The unit allows five starts per hour, so four automatic restarts follow the one the session performs. That recovers a compositor which comes back, without hiding a fault that keeps recurring: once the budget is spent the unit stays in `failed` state, where `systemctl --user status` reports it. The budget counts every start, manual ones included, so `systemctl --user reset-failed daytrace.service` is what clears it once a fault is fixed.

A daemon started by hand is what the unit then collides with. The unit is refused while that process holds the claim, and the refusal exits with code `1`, the runtime failure code, not the usage code `2` and not zero. The refusal also spends a start from the same budget, so a manual daemon left running across a login ends with the unit in `failed`. That is the intended outcome: capture is happening, from a process with whatever configuration that shell had, and the state a human has to look at should say so rather than hide it. Stop the manual daemon, then `systemctl --user reset-failed daytrace.service`.

Configuration goes in a drop-in rather than an edit of the generated unit, so re-running the command does not discard it:

```sh
systemctl --user edit daytrace.service
```

```ini
[Service]
Environment=DAYTRACE_IDLE_AFTER_SECONDS=180
```

## Where the Database Lives

By default the database is stored at:

```sh
${XDG_DATA_HOME:-~/.local/share}/daytrace/daytrace.db
```

- `DAYTRACE_DB_PATH`: SQLite database path.

## Environment Variables

Useful environment overrides:

- `DAYTRACE_IDLE_AFTER_SECONDS`: AFK threshold, default `300`.
- `DAYTRACE_POLL_SECONDS`: desktop polling interval, default `1`.
- `DAYTRACE_RETENTION_DAYS`: days before today that `daytrace prune` keeps, default `90`, so the default keeps 91 calendar days. An empty value reads as unset; `0` is refused rather than read as "keep only today", since the difference between those two is a whole history.
- `DAYTRACE_BLACKLIST_APPS`: comma-separated application class substrings to skip. Matching is by substring so that a short entry such as `keepassxc` covers the reverse-DNS class `org.keepassxc.KeePassXC` that a compositor actually reports.
- `DAYTRACE_BLACKLIST_TITLES`: comma-separated title substrings to skip.
- `DAYTRACE_BLACKLIST_DOMAINS`: comma-separated URL or domain substrings to skip.

## Reading a Day

```sh
daytrace today
daytrace today --date 2026-07-20
```

`daytrace today` prints the chronological timeline with segment durations, then totals the day by application, so the report answers both what happened in order and what consumed the day:

```text
Timeline for 2026-07-20
09:10-09:34  24m     ghostty - tmux
09:34-09:51  17m     firefox - [browser title redacted]
09:51-09:52  12s     ghostty - tmux
09:52-10:08  16m     AFK
23:40-24:00  20m     ghostty - tmux

Time per application
   44m  ghostty
   17m  firefox
   16m  AFK
```

A span shorter than a minute reports the seconds it lasted. Rounding it to the nearest minute called it `0m`, and with one-second polling a stretch of rapid window switching became a column of identical zeroes crowding out the blocks that held the day. A segment that lasted no time at all reads as `0s`, which says what happened instead of looking like a duration lost to rounding: a focus change with no input during an idle wait closes the displaced window at the instant it opened, and startup recovery does the same to a segment the daemon only ever observed once, which is the last application focused before it died.

A stretch the machine spent suspended is reported as `Suspended`, separately from `AFK`. The two are the same absence of input but not the same fact about the day, and merging them makes a laptop closed overnight read as eight hours away from a running desk. The stretch is recognized on the first poll after the machine comes back, and its length is the kernel's own count of suspended time, taken as the difference between the two clocks that measure time since boot: one of them counts time spent suspended and the other does not. The segment in focus when the machine stopped is closed there rather than credited with the whole gap, and the poll after the resume opens a segment of its own.

The wall clock is used only to place that stretch on the timeline, never to decide that one happened or how long the machine was down. This is deliberate and it is the important part: the wall clock jumps as a matter of routine, since every boot starts it from a hardware clock that has drifted and the correction is applied as a step rather than eased in. Deriving an absence from that movement would invent segments on an ordinary morning, and an invented segment is worse than a missing one, because the report and the export state it as fact and nothing downstream can tell it from a real gap.

What that leaves. Suspend and hibernate are not told apart, because the kernel counts both the same way and both mean the machine was not running. A stretch during which the daemon itself was not running stays an ordinary gap in the day, whether the machine was off or the daemon was merely stopped: the clocks restart at boot and a fresh process has nothing earlier to compare against. Each endpoint is placed by the wall clock at the poll that noticed the resume, so it carries up to a polling interval of error and, on a boot whose clock has not yet been corrected, whatever error that clock still holds: a stretch of the right length can land at the wrong time, or on the wrong day. A stored stretch can also come out shorter than the kernel counted, where it would otherwise reach back over activity the daemon had already observed, which is the right way to lose the argument. A suspend shorter than five seconds is left with whatever segment was open rather than breaking the day into three rows.

Totals sum seconds and round once, so a minute spread over several short visits still reports as a minute rather than inheriting each row's rounding. Absence is totalled as `AFK`, apart from any application.

`--date YYYY-MM-DD` reports any other local day, which is what a review of the past week needs once midnight has passed. Day boundaries come from the local calendar day, so a day that a clock change shortens or lengthens still meets its neighbours exactly. A segment reaching the end of the reported day ends at `24:00`, which names the boundary: the instant it is clipped to belongs to the following day, so a clock would call it `00:00` and a whole day would read as beginning and ending at the same time.

There is no browser extension, so browser private and incognito detection is best-effort from the Hyprland window title alone, and a browser that does not mark a private window in its title is not detected at all. Browser titles are redacted before storage either way, which is what keeps a missed detection from costing anything beyond a row that reads as an ordinary browser block.

## Exporting a Day

```sh
daytrace export
daytrace export --date 2026-07-20 > 2026-07-20.json
```

`daytrace export` prints one local day as JSON on standard output, defaulting to today, so a day can leave the tool without reaching into SQLite by hand.

```json
{
  "date": "2026-07-20",
  "segments": [
    {
      "started_at": "2026-07-20T09:10:00-03:00",
      "ended_at": "2026-07-20T09:34:00-03:00",
      "duration_seconds": 1440,
      "kind": "window",
      "app_class": "com.mitchellh.ghostty",
      "title": "tmux",
      "workspace": "3",
      "monitor": 1
    }
  ]
}
```

Every segment carries the same keys, with an absent value written as `null` rather than dropped, so a consumer can rely on the shape. Instants are RFC 3339 with the local offset, which keeps an exported day readable on a machine that does not share this one's timezone. `duration_seconds` is included so that summing a day does not require parsing two timestamps per segment. `kind` is `window`, `idle`, or `suspended`. Titles are exported as they were stored, which means already redacted: the export applies no further filtering and performs no further capture.

A segment still in progress has no end yet, and is exported with `ended_at` at the last moment it was observed. Exporting today twice therefore gives the final segment a later end the second time, while any completed day is stable.

## Deleting Data

Deleting everything is removing the database, since nothing is kept anywhere else:

```sh
systemctl --user stop daytrace.service
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/daytrace"
```

Stop the daemon first for this one. Removing the file is not something SQLite is told about, so a running process goes on writing to a file that no longer has a name, and recreates the store at the next window change. Deleting part of the history rather than all of it is `daytrace prune`, described below, which needs no such thing and is meant to run alongside the daemon.

## Retention

Capture writes at most one segment a second and never expires anything on its own, so the store is a permanent record of every window that held focus until something removes part of it. The retention window is how much of that history is meant to be kept: the last `90` days plus today by default, and `DAYTRACE_RETENTION_DAYS` sets it.

```sh
daytrace prune --dry-run
daytrace prune
DAYTRACE_RETENTION_DAYS=30 daytrace prune
```

```text
Retention window: 90 days plus today, keeping activity from 2026-04-30 onwards.
Deleted 412 activity segments.
```

Both forms print the window and the first day it keeps before saying what happened, so the policy being applied is visible in the output of the command that applies it. `--dry-run` reports how many segments are outside the window and deletes nothing, which is worth doing first: there is no undo, and `daytrace export` is the only way to keep a copy of a day that is about to go.

**Nothing prunes automatically.** No command deletes as a side effect, and neither the daemon nor the reporting commands apply the window. Deleting activity that nobody asked to lose, at a moment nobody was present for, is not something a background process should decide, and an activity log that quietly loses days is worse than one that grows. The cost of that choice is that an installation where `daytrace prune` is never run keeps everything, so a systemd user timer calling `daytrace prune` is what makes retention continuous, on a schedule its owner chose and can read.

The window opens at a local midnight that many days back, not at this time of day that many days back, so pruning twice in one day removes nothing the second time, and a day the clock shortened, lengthened or opened without a midnight still counts as one day. Moving the machine to another timezone, or a clock correction that crosses midnight, does move the boundary by a day, because there is no record of which zone a segment was recorded in. A segment that straddles the boundary is kept whole rather than trimmed, since cutting it would rewrite a span that was actually observed. A segment left open by a crash ages out by the last moment it was seen, and one with no observation recorded at all is kept until a daemon start writes it a real end: nothing a report still shows is deleted by the window.

### What deleting guarantees

After a successful prune, the deleted activity is no longer readable in the store, and the space it used is back with the filesystem. Both take more than a `DELETE`, and both were measured rather than assumed. Removing rows leaves their bytes in the pages they freed, so the database is rebuilt from the rows that survived, which also gives the space back: 400 half-hour segments deleted took a store from 49152 bytes with 468 readable copies of their window titles to 12288 bytes with none, with the capture daemon attached throughout. The write-ahead log is then copied into the file and reset to nothing, because until that happens the file still holds the pages as they were, the log holds a copy of everything the prune touched, and the store has grown rather than shrunk.

The file does not shrink below the size its busiest window needs, and it does not have to: a day recorded after a prune reuses the space, so a store that is pruned regularly settles at the size of its window instead of growing forever.

The rebuild is written to a scratch copy of the database first. SQLite would place that in the system temporary directory, which on a Linux desktop is `/var/tmp`, so it is pointed at the store's own directory instead: a full plaintext copy of the activity log should not land outside the `0700` directory the store is kept in, on a filesystem nobody chose for it. The copy is created `0600` and unlinked as soon as the rebuild ends, and pruning therefore needs free space beside the database roughly equal to its size.

`daytrace prune` can run while the capture daemon runs, which is the arrangement a timer produces. Writes wait for each other rather than failing, and the segment currently being recorded can never fall outside the window, because the daemon moves its progress marker every poll. If something is reading the store from an older snapshot at that moment, the rewrite cannot start, and the command says so rather than reporting a clean deletion:

```text
Deleted 412 activity segments.
The deleted activity is still readable in the database file, because another process is reading it. Running prune again finishes clearing it.
```

The rows are gone either way. What is left is the copy in the file, which the next `daytrace prune` clears even when it deletes nothing.

## Development

```sh
bin/install-hooks
bin/ci
```

`bin/ci` runs formatting, linting, tests, dependency audit, Markdown wrap checks, and public prose checks.
