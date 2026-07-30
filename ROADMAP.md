# Roadmap

## Current State

- Rust CLI crate exists.
- `daytrace start`, `daytrace today`, and `daytrace export` define the initial command surface.
- `daytrace start` records Hyprland active-window segments and AFK segments.
- A stretch the machine spent suspended is stored as its own kind of segment, so a powered-down gap is not reported as time spent away from a running machine.
- Activity segments are stored locally in SQLite.
- `daytrace today` prints a chronological daily timeline with durations, followed by per-application totals.
- `daytrace today --date` and `daytrace export --date` report and export any past local day.
- `daytrace export` emits one day of stored activity as JSON on standard output.
- `daytrace service unit` prints a systemd user unit that runs the capture daemon for the desktop session.
- One capture daemon runs per database. A second `daytrace start` is refused and names the process already running.
- Application, title, and domain blacklist environment variables exist.
- Browser titles are redacted by default and private/incognito browser windows are skipped when Hyprland exposes a recognizable private-mode title marker.
- Deterministic project gates exist for Rust CI, dependency audit, secret scanning, Markdown wrapping, and public prose hygiene.

## First Milestone

- Harden daemon lifecycle around startup recovery and shutdown behavior.
- Add a documented deletion command for local logs.
- Add configuration file support if environment variables become insufficient.
- Add focused integration coverage around Hyprland command failures.
- Settle how screen lock and unlock are observed. Neither the compositor nor the logind locked hint reports them on a Hyprland session whose locker never publishes that hint, so the remaining source is a session-manager subscription and the dependency it costs.
- Make browser private/incognito detection reliable through the browser extension milestone.

## Later Milestones

- Add MPRIS media playback capture after the desktop layer works.
- Add browser tab capture through a browser extension and Native Messaging.
- Add URL and token redaction before browser activity is stored.
- Add domain and application blacklist configuration.
- Add `daytrace eod-pack` to emit a compact activity package for an external end-of-day summarizer.
- Integrate with external `llm-workflow` end-of-day tooling without making this repository depend on it.

## Out of Scope

- Dashboard UI.
- Browser extension in the first milestone.
- AI-generated narrative inside `daytrace`.
- Multi-user or platform design.
- Cloud sync.
- Tracking beyond this desktop.
