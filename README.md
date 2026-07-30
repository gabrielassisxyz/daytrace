# daytrace

`daytrace` is a local-first personal activity logger for reconstructing where time went during the day.

The first milestone is a small Rust CLI and daemon that records active-window changes on Hyprland plus idle periods, stores them locally, and prints a chronological daily timeline with durations.

## Privacy Boundary

- Local storage only.
- No cloud sync.
- No screenshots.
- No clipboard capture.
- No page-content capture.
- Sensitive URLs and tokens are redacted before storage.
- Browser window titles are redacted by default because they can contain page content.
- Browser private and incognito windows are skipped when Hyprland exposes a recognizable private-mode title marker.
- Domain and application blacklists exist from the first capture milestone.
- Logs are easy to delete and export.

## Initial Commands

```sh
daytrace start
daytrace today
daytrace today --date 2026-07-20
```

`daytrace start` runs the local capture loop. It polls Hyprland for the active window, watches `/dev/input/event*` for input activity timestamps, and stores segments in a local SQLite database.

AFK tracking requires read access to at least one `/dev/input/event*` device. If no readable input devices are available, `daytrace start` exits instead of recording misleading activity.

One capture process runs per database. A second `daytrace start` is refused and names the process already running, rather than doubling capture: each process reads its own configuration, so a blacklist set in one shell is invisible to the other, and the process without it would record what the other one exists to skip. The claim is a lock on `daytrace.db.lock`, held beside the database for as long as the daemon runs and released by the kernel when it exits, so an unclean shutdown leaves nothing to clean up. Two daemons on different databases are not duplicates and both run.

`daytrace today` prints the chronological timeline with segment durations, then totals the day by application, so the report answers both what happened in order and what consumed the day:

```text
Timeline for 2026-07-20
09:10-09:34  24m     ghostty - tmux
09:34-09:51  17m     firefox - [browser title redacted]
09:51-10:07  16m     AFK

Time per application
   24m  ghostty
   17m  firefox
   16m  AFK
```

Totals sum seconds and round once, so a minute spread over several short visits still reports as a minute even where each individual row rounds to `0m`. Absence is totalled as `AFK`, apart from any application.

A stretch the machine spent suspended is reported as `Suspended`, separately from `AFK`. The two are the same absence of input but not the same fact about the day, and merging them makes a laptop closed overnight read as eight hours away from a running desk. The stretch is recognized on the first poll after the machine comes back, and its length is the kernel's own count of suspended time, taken as the difference between the two clocks that measure time since boot: one of them counts time spent suspended and the other does not. The segment in focus when the machine stopped is closed there rather than credited with the whole gap, and the poll after the resume opens a segment of its own.

The wall clock is used only to place that stretch on the timeline, never to decide that one happened or how long it lasted. This is deliberate and it is the important part: the wall clock jumps as a matter of routine, since every boot starts it from a hardware clock that has drifted and the correction is applied as a step rather than eased in. Deriving an absence from that movement would invent segments on an ordinary morning, and an invented segment is worse than a missing one, because the report and the export state it as fact and nothing downstream can tell it from a real gap.

What that leaves. Suspend and hibernate are not told apart, because the kernel counts both the same way and both mean the machine was not running. A stretch during which the daemon itself was not running stays an ordinary gap in the day, whether the machine was off or the daemon was merely stopped: the clocks restart at boot and a fresh process has nothing earlier to compare against. Each endpoint is accurate to within one polling interval, and a suspend shorter than five seconds is left with whatever segment was open rather than breaking the day into three rows.

`--date YYYY-MM-DD` reports any other local day, which is what a review of the past week needs once midnight has passed. Day boundaries come from the local calendar day, so a day that a clock change shortens or lengthens still meets its neighbours exactly.

The first milestone does not use a browser extension, so browser private/incognito detection is best-effort from the Hyprland window title. Browser titles are still redacted before storage.

By default the database is stored at:

```sh
${XDG_DATA_HOME:-~/.local/share}/daytrace/daytrace.db
```

Useful environment overrides:

- `DAYTRACE_DB_PATH`: SQLite database path.
- `DAYTRACE_IDLE_AFTER_SECONDS`: AFK threshold, default `300`.
- `DAYTRACE_POLL_SECONDS`: desktop polling interval, default `1`.
- `DAYTRACE_BLACKLIST_APPS`: comma-separated application class substrings to skip. Matching is by substring so that a short entry such as `keepassxc` covers the reverse-DNS class `org.keepassxc.KeePassXC` that a compositor actually reports.
- `DAYTRACE_BLACKLIST_TITLES`: comma-separated title substrings to skip.
- `DAYTRACE_BLACKLIST_DOMAINS`: comma-separated URL or domain substrings to skip.

## Running as a User Service

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

A daemon started by hand is what the unit then collides with. The unit is refused while that process holds the claim, and the refusal spends a start from the same budget, so a manual daemon left running across a login ends with the unit in `failed`. That is the intended outcome: capture is happening, from a process with whatever configuration that shell had, and the state a human has to look at should say so rather than hide it. Stop the manual daemon, then `systemctl --user reset-failed daytrace.service`.

Configuration goes in a drop-in rather than an edit of the generated unit, so re-running the command does not discard it:

```sh
systemctl --user edit daytrace.service
```

```ini
[Service]
Environment=DAYTRACE_IDLE_AFTER_SECONDS=180
```

## Export and Deletion

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

Deleting is removing the database, since nothing is kept anywhere else:

```sh
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/daytrace"
```

Stop the daemon first. A running process holds the database open, so deleting the file under it leaves the process writing to a file that no longer has a name.

## Development

```sh
bin/install-hooks
bin/ci
```

`bin/ci` runs formatting, linting, tests, dependency audit, Markdown wrap checks, and public prose checks.
