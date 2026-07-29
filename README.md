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
- Browser private and incognito windows are not recorded.
- Domain and application blacklists exist from the first capture milestone.
- Logs are easy to delete and export.

## Initial Commands

```sh
daytrace start
daytrace today
```

The current build exposes the command surface, but desktop capture is still planned work.

## Development

```sh
bin/install-hooks
bin/ci
```

`bin/ci` runs formatting, linting, tests, dependency audit, Markdown wrap checks, and public prose checks.
