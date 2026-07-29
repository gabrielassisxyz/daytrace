# Roadmap

## Current State

- Rust CLI crate exists.
- `daytrace start` and `daytrace today` define the initial command surface.
- Deterministic project gates exist for Rust CI, dependency audit, secret scanning, Markdown wrapping, and public prose hygiene.

## First Milestone

- Run a local daemon that observes Hyprland active-window changes.
- Capture application or class, window title, workspace, timestamps, and duration.
- Capture idle and AFK periods.
- Store events locally with an append-friendly model.
- Print a chronological timeline for the current day with durations.

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
