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

Configuration goes in a drop-in rather than an edit of the generated unit, so re-running the command does not discard it:

```sh
systemctl --user edit daytrace.service
```

```ini
[Service]
Environment=DAYTRACE_IDLE_AFTER_SECONDS=180
```

## Development

```sh
bin/install-hooks
bin/ci
```

`bin/ci` runs formatting, linting, tests, dependency audit, Markdown wrap checks, and public prose checks.
