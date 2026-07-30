# Changelog

All notable changes to The Crowded Room will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.1] - 2026-07-30

### Fixed

- Forwarded Shift+Tab to guests using the terminal `CSI Z` back-tab sequence.
- Deferred house rules until each guest's startup output has been quiet for two
  seconds, so initialization cannot consume the message or its Enter key.

## [0.5.0] - 2026-07-30

### Added

- Automatic, vendor-neutral house rules teaching each guest its room number,
  Doorbell command, and peer-message trust boundary at startup and restart.

## [0.4.0] - 2026-07-30

### Added

- An authenticated, process-local Unix-socket Doorbell for guest-originated
  envelopes.
- Per-room capability tokens and a `crowded send ROOM MESSAGE` helper exposed
  inside each guest.
- Target, size, hop, duplicate, queue, and per-room rate-limit checks.
- An `F3` pause switch that queues Doorbell deliveries until resumed.

### Changed

- Doorbell envelopes now enter the Mailroom audit log and deliver automatically
  by default.

## [0.3.0] - 2026-07-30

### Added

- A bounded in-memory Mailroom with structured envelopes and monotonic IDs.
- Honest `Queued`, `Injected`, and `Failed` delivery states.
- An `F2` message-log overlay.

### Changed

- Routed all cross-room writes through the Mailroom.

## [0.2.1] - 2026-07-30

### Fixed

- Submitted notes to raw TUI guests as separate text and Enter writes, with a
  short settling delay for Codex CLI paste-burst protection.

## [0.2.0] - 2026-07-30

### Added

- Two independently managed PTY rooms with keyboard focus switching.
- Composed notes routed between rooms.
- Direct-program guest specifications using `shell:` and `raw:` transports.
- Offline room detection and focused-room restart.
- A status bar with controls, compose state, and lifecycle notices.

### Changed

- Split the application into focused `app` and `pane` modules.
- Made the target room's transport responsible for encoding incoming notes.

### Fixed

- Restored the parent terminal after normal exits and early errors.
- Reaped child processes even when status polling fails.
- Protected `vt100` from tiny-terminal row and column underflow.

## [0.1.0] - 2026-07-29

### Added

- Initial Ratatui interface containing one directly spawned embedded PTY.
- Keyboard input, child output, terminal resize, and safe `Ctrl+Q` shutdown.
