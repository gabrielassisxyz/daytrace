# daytrace

`daytrace` is a local-first personal activity logger for reconstructing where time went during the day.

It is a small Rust CLI and daemon that records active-window changes on Hyprland, idle periods, stretches the machine spent suspended, and what was playing behind the focused window, stores them locally, and prints a chronological daily narrative with durations. `daytrace today` groups the desktop activity into blocks and names whichever media held the background on each one; `daytrace export` keeps the desktop segments and the media segments as two separate arrays instead. Media never joins a block's own time: it overlaps the desktop and itself, so a shared total would claim more time than the day held, and only `Media playing` ever states a media duration.

## Privacy Controls

- Local storage only.
- No cloud sync.
- No screenshots.
- No clipboard capture.
- No page-content capture.
- No keystroke capture: an input device is read only for the timestamp of an event, to detect idle and AFK, never for a key, button, or pointer value. These are permanently out of scope, not pending.
- Free text such as a window title, a track title or an artist goes through one redaction scan: an address inside it is replaced whole, and a `keyword=value` secret loses its value.
- A field that is itself an address, such as a played track's URL, goes through a different scan: the address is kept, and only its sensitive query or fragment parameters (`token`, `key`, `secret`, `password`, `code`) are stripped. Running the free-text scan over an address would replace the whole value and refuse to say what was played.
- Browser window titles are stored like other titles: the page name is kept, redacted by the same free-text scan as every other title.
- Browser private and incognito windows are skipped when Hyprland exposes a recognizable private-mode title marker.
- Media is recorded by name: title, every artist, album and the address of what was playing, read from whichever player is playing over the user D-Bus (MPRIS), and stored in a lane of its own rather than folded into the desktop timeline.
- Domain and application blacklists exist from the first capture milestone, and cover media too: an application entry matches the player, and a domain entry matches the address. They are how a user excludes what they do not want recorded, open by default and closed case by case.
- Stored activity has a retention window, the last `90` days by default, applied on demand by `daytrace prune` and never automatically.
- Logs are easy to delete and export.

## Installing

`daytrace` is not distributed as a package or a prebuilt binary; it is built from a checkout of this repository. `cargo build --release --locked` produces `target/release/daytrace`; put that binary, or a symlink to it, on `PATH`.

## Running by Hand

```sh
daytrace start
```

`daytrace start` runs the local capture loop. It polls Hyprland for the active window, watches `/dev/input/event*` for input activity timestamps, polls the user D-Bus for whichever MPRIS player is playing, and stores segments in a local SQLite database.

AFK tracking requires read access to at least one `/dev/input/event*` device. If no readable input devices are available, `daytrace start` exits instead of recording misleading activity.

The set of watched devices is not fixed at start: every 30 seconds the daemon re-enumerates `/dev/input` for anything that was not present, or not readable, the last time it looked, and starts watching it. A watcher that hits a read error retires rather than looping on a dead device, but its device is not abandoned: the same rescan reopens it once it is readable again, which is what a Bluetooth keyboard's transport dropping and reconnecting, or a USB dock being unplugged and replugged, needs. While no watcher is alive nothing is treated as idle on trust: a desktop poll during that stretch records whatever the compositor still reports instead of manufacturing AFK, and the condition is logged as a rate-limited warning rather than one line per poll, the same way a media source failure already is. Only once no input can be observed for 60 consecutive polls, the same sustained-failure threshold and mechanism the compositor query below already uses, does the daemon give up and exit non-zero rather than keep recording a day that never happened.

Media polling has two runtime prerequisites: `busctl`, the systemd D-Bus client, on `PATH`, and a reachable user D-Bus session. Nothing changes at the Cargo level; the dependency is a subprocess call, not a linked crate. It runs on its own interval, separate from the desktop poll, because a track change matters far less often than a window change and the check costs a subprocess per player on the bus. Without either prerequisite, media polling fails outright; a machine with both but no player ever running simply finds nothing. Either way the failure is logged as a rate-limited warning rather than one line per poll, and the desktop side of capture keeps running unaffected.

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
- `DAYTRACE_MEDIA_POLL_SECONDS`: media polling interval, default `5`. Separate from `DAYTRACE_POLL_SECONDS`, since a track change costs a `busctl` call per player and does not need the desktop's own cadence to be caught.
- `DAYTRACE_RETENTION_DAYS`: days before today that `daytrace prune` keeps, default `90`, so the default keeps 91 calendar days. An empty value reads as unset; `0` is refused rather than read as "keep only today", since the difference between those two is a whole history.
- `DAYTRACE_BLACKLIST_APPS`: comma-separated application class substrings to skip. Matching is by substring so that a short entry such as `keepassxc` covers the reverse-DNS class `org.keepassxc.KeePassXC` that a compositor actually reports.
- `DAYTRACE_BLACKLIST_TITLES`: comma-separated title substrings to skip.
- `DAYTRACE_BLACKLIST_DOMAINS`: comma-separated URL or domain substrings to skip.

## Reading a Day

```sh
daytrace today
daytrace today --date 2026-07-20
daytrace today --raw --date 2026-07-20
```

`daytrace today` groups the day's desktop activity into blocks and prints them in order, then totals the day by application from those blocks, so the report answers both what happened in order and what consumed the day. A block is a run of consecutive desktop activity that stayed on one application; a foreign focus shorter than five seconds is folded into the block around it rather than reported as an interruption, so a quick alt-tab does not break a forty-minute block of writing into three rows. A block with more than one distinct title lists its five longest beneath it, longest first, with anything past the fifth rolled into one remainder line. A foreign focus swallowed into the block around it keeps its own title as one of these sub-lines rather than disappearing: three seconds spent in a launcher between two long stretches of a terminal still names the launcher, one line beneath the terminal's own block instead of on a row of its own. A block line names the application and, where a media player overlapped it for at least a minute, what was playing behind it; the workspace and monitor a raw row carries are not shown, since a block can span several of each, and they come back in `daytrace today --raw` and in `daytrace export`:

```text
Timeline for 2026-07-20
09:10-09:34  24m     ghostty - tmux
09:34-10:14  40m     firefox, spotify playing in the background
             18m       GitHub pull request
             11m       tokio docs
             6m        Inbox - Brave
             3m        Rust changelog
             1m        Cargo book
             1m        other (2 titles)
10:14-10:30  16m     AFK
10:30-10:33  3m      zed - notes, brave playing in the background and 1 more

Time per application
   40m  firefox
   24m  ghostty
   16m  AFK
    3m  zed

Media playing
09:35-09:40  5m      spotify - Track title
10:30-10:32  2m      brave - Some video
10:30-10:31  1m      mpv - Another stream

    7m  Total
```

A block that a media player overlapped by at least a minute carries the player's name as a suffix, `, spotify playing in the background`, with `and N more` appended when other players also cleared that floor; a track heard for a few seconds explains nothing about the block and is left out of the suffix, though it still gets its own row in `Media playing` below. Naming a player never moves a second between blocks: the desktop lane alone decides where the day's time went, and media is a fact riding along rather than a second claim on it.

`daytrace today --raw` prints the report exactly as it read before blocks existed: one row per stored desktop segment, and `Time per application` totalled from those rows rather than from the blocks above. It is the record a block's own totals can be checked against by hand, and it is where a foreign focus a block folded into its neighbour comes back as a row of its own, workspace and monitor included:

```text
Timeline for 2026-07-20
09:10-09:24  14m     ghostty - tmux workspace 3, monitor 1
09:24-09:24  3s      rofi - quick check
09:24-09:34  10m     ghostty - tmux workspace 3, monitor 1
09:34-09:50  16m     firefox - Inbox - Brave
09:50-10:06  16m     AFK

Time per application
   24m  ghostty
   16m  AFK
   16m  firefox
    3s  rofi
```

`Time per application` can therefore disagree with the aggregated default by at most the seconds a swallowed foreign focus moved to the block around it: here `rofi` holds three seconds of its own instead of vanishing into `ghostty`'s twenty-four minutes. `Media playing` and its union total are identical on both paths; `--raw` changes the timeline and the per-application totals only.

A span shorter than a minute reports the seconds it lasted. Rounding it to the nearest minute called it `0m`, and with one-second polling a stretch of rapid window switching became a column of identical zeroes crowding out the blocks that held the day. A segment that lasted no time at all reads as `0s`, which says what happened instead of looking like a duration lost to rounding: a focus change with no input during an idle wait closes the displaced window at the instant it opened, and startup recovery does the same to a segment the daemon only ever observed once, which is the last application focused before it died.

A stretch the machine spent suspended is reported as `Suspended`, separately from `AFK`. The two are the same absence of input but not the same fact about the day, and merging them makes a laptop closed overnight read as eight hours away from a running desk. The stretch is recognized on the first poll after the machine comes back, and its length is the kernel's own count of suspended time, taken as the difference between the two clocks that measure time since boot: one of them counts time spent suspended and the other does not. The segment in focus when the machine stopped is closed there rather than credited with the whole gap, and the poll after the resume opens a segment of its own.

The wall clock is used only to place that stretch on the timeline, never to decide that one happened or how long the machine was down. This is deliberate and it is the important part: the wall clock jumps as a matter of routine, since every boot starts it from a hardware clock that has drifted and the correction is applied as a step rather than eased in. Deriving an absence from that movement would invent segments on an ordinary morning, and an invented segment is worse than a missing one, because the report and the export state it as fact and nothing downstream can tell it from a real gap.

What that leaves. Suspend and hibernate are not told apart, because the kernel counts both the same way and both mean the machine was not running. A stretch during which the daemon itself was not running stays an ordinary gap in the day, whether the machine was off or the daemon was merely stopped: the clocks restart at boot and a fresh process has nothing earlier to compare against. Each endpoint is placed by the wall clock at the poll that noticed the resume, so it carries up to a polling interval of error and, on a boot whose clock has not yet been corrected, whatever error that clock still holds: a stretch of the right length can land at the wrong time, or on the wrong day. A stored stretch can also come out shorter than the kernel counted, where it would otherwise reach back over activity the daemon had already observed, which is the right way to lose the argument. A suspend shorter than five seconds is left with whatever segment was open rather than breaking the day into three rows.

Totals sum seconds from the blocks, not the raw rows behind them, and round once, so a minute spread over several short visits still reports as a minute rather than inheriting each row's rounding. A foreign focus swallowed into a block moves its seconds to whichever application the block belongs to rather than dropping them, so the totals still close to the same number of seconds the desktop lane recorded. Absence is totalled as `AFK`, apart from any application.

`--date YYYY-MM-DD` reports any other local day, which is what a review of the past week needs once midnight has passed. Day boundaries come from the local calendar day, so a day that a clock change shortens or lengthens still meets its neighbours exactly. A segment reaching the end of the reported day ends at `24:00`, which names the boundary: the instant it is clipped to belongs to the following day, so a clock would call it `00:00` and a whole day would read as beginning and ending at the same time.

There is no browser extension, so browser private and incognito detection is best-effort from the Hyprland window title alone, and a browser that does not mark a private window in its title is not detected at all. A missed private window writes the page name into the store, the same as any other window; that is the one case where the browser kept no history of its own to compare against, and it is accepted rather than hidden.

Media has no equivalent detector at all. MPRIS carries no private-window signal, so a media row from a browser names what was playing regardless of which window it played in, whether or not the window title detector recognized that window as private. The window side is unaffected by this: a recognized private window is still skipped whole, and only a missed one writes its title into the store. Media does not get that protection either way, which is the residual this section is about. The blacklists are how a user closes it: an application entry excludes a player, and a domain entry excludes a site, private window or not.

A day that held media gains a section below the desktop timeline and its totals:

```text
Media playing
09:10-09:34  24m     spotify - Track title - Artist
```

The section carries its own total, at the same column widths as the timeline above it, so the two read as one report. Every number the report prints, in either section, stays at or under the length of the day: media overlaps the desktop and can overlap itself, and a total that summed across an overlap would claim more time than the day held, which is why the two sources are never added together. The artist and its separator are dropped when there is no artist; a player with no title falls back to the address; with neither, the row reads as `unknown media`. A day with media and no desktop activity at all still prints the dated header and the media section, rather than the empty-day line below, and a day with neither source keeps that line exactly as it always has:

```text
No activity events recorded for 2026-07-20.
```

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
  ],
  "media": [
    {
      "started_at": "2026-07-20T09:10:00-03:00",
      "ended_at": "2026-07-20T09:34:00-03:00",
      "duration_seconds": 1440,
      "player": "spotify",
      "title": "Track title",
      "artist": "Artist",
      "album": "Album",
      "item_url": "https://open.spotify.com/track/1"
    }
  ]
}
```

Every segment carries the same keys, with an absent value written as `null` rather than dropped, so a consumer can rely on the shape. Instants are RFC 3339 with the local offset, which keeps an exported day readable on a machine that does not share this one's timezone. `duration_seconds` is included so that summing a day does not require parsing two timestamps per segment. `kind` is `window`, `idle`, or `suspended`. Titles are exported as they were stored; the export applies no further filtering and performs no further capture.

`media` is a list of its own, beside `segments` rather than folded into it: a consumer summing `segments` for a day's total gets exactly what it got before media existed, because a media entry never appears there. The array is present and empty on a day with nothing played, never omitted, so a consumer never has to tell an empty day apart from a daytrace too old to have recorded media at all. A media entry carries `started_at`, `ended_at`, `duration_seconds`, `player`, `title`, `artist`, `album` and `item_url`; the internal lane a player is stored under, and the fact that the row is media rather than a window, stay out of the export.

A segment still in progress has no end yet, and is exported with `ended_at` at the last moment it was observed. Exporting today twice therefore gives the final segment a later end the second time, while any completed day is stable. The same holds for a media entry still playing.

## Deleting Data

Deleting everything is removing the database, since nothing is kept anywhere else:

```sh
systemctl --user stop daytrace.service
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/daytrace"
```

Stop the daemon first for this one. Removing the file is not something SQLite is told about, so a running process goes on writing to a file that no longer has a name, and recreates the store at the next window change. Deleting part of the history is `daytrace prune`, by date, or `daytrace forget`, by content: both are described below, and neither needs the daemon stopped.

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

## Forgetting Specific Activity

Retention deletes by date; `daytrace forget` deletes by content, for a row that should never have been stored and does not deserve to wait out its whole day. It matches the same fields a blacklist entry would: the application, the window title, and, for a played track, its artist, album, and address, all as a case-insensitive substring.

```sh
daytrace forget --matching keepassxc --dry-run
daytrace forget --matching keepassxc
```

```text
1 activity segment is matched by it. Nothing was deleted.
Deleted 1 activity segment.
```

`--dry-run` and the deletion run the same query, so the preview cannot name a count the deletion would not produce. There is no undo: `daytrace export` is the only way to keep a copy of what is about to go, and a pattern that matches nothing deletes nothing and says so rather than failing.

`daytrace forget` gives the same on-disk guarantee `daytrace prune` does, because it is the same rewrite: the file is rebuilt from the rows that survive and the write-ahead log is checkpointed, so the matched text stops being readable in the database file rather than merely unlisted. It can run beside the capture daemon the same way, and an incomplete checkpoint is reported beside the deletion rather than instead of it, worded the way `daytrace prune`'s own report is.

## Development

```sh
bin/install-hooks
bin/ci
```

`bin/ci` runs formatting, linting, tests, dependency audit, Markdown wrap checks, and public prose checks.
