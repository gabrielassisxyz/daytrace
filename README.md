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
```

`daytrace start` runs the local capture loop. It polls Hyprland for the active window, watches `/dev/input/event*` for input activity timestamps, and stores segments in a local SQLite database.

AFK tracking requires read access to at least one `/dev/input/event*` device. If no readable input devices are available, `daytrace start` exits instead of recording misleading activity.

`daytrace today` prints today's chronological timeline with segment durations.

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

## Development

```sh
bin/install-hooks
bin/ci
```

`bin/ci` runs formatting, linting, tests, dependency audit, Markdown wrap checks, and public prose checks.
