# Changelog

All notable changes to The Crowded Room will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.25.3] - 2026-08-06

### Added

- `crowded control ROOM model M effort E` sets model and effort together in one restart instead of two. `crowded roster` now reports each room's current `model` and `effort`, read live from its launch arguments.

### Changed

- `ControlAction::SetModel`/`SetEffort` merged into a single `ControlAction::Configure { model, effort }` on the Doorbell wire protocol.

## [0.25.2] - 2026-08-06

### Fixed

- Toolbox stale resync now preserves the original snapshot for surviving paths and restores orphaned targets before deleting state, so `crowded toolbox remove` remains reliable after rooms are added or removed. Deduplicate stale check in `native_files_are_active_at`. Add regression tests for stale resync.

## [0.25.1] - 2026-08-04

### Changed

- Raise Doorbell message bodies from 4 KiB to 1 MiB.
- Wait longer before submitting pasted raw messages on Windows so Codex
  receives the final Enter key.

## [0.25.0] - 2026-08-04

### Added

- Configure shared remote MCP servers over Streamable HTTP or legacy SSE.

## [0.24.6] - 2026-08-04

### Fixed

- Fall back to copying OpenCode adapter files when Windows denies symbolic-link
  creation.

## [0.24.5] - 2026-08-04

### Fixed

- Generate PowerShell `commandWindows` entries for Codex plugin adapter hooks.

## [0.24.4] - 2026-08-04

### Fixed

- Generate PowerShell `commandWindows` pulse commands in Codex hook files.
- Fall back to Windows directory junctions when creating plugin skill links is
  blocked by local symbolic-link privilege policy.

## [0.24.3] - 2026-08-03

### Fixed

- Remove Windows directory-symlinked plugin skills with the directory API so
  `crowded plugin disable` no longer fails with Access Denied.

## [0.24.2] - 2026-08-03

### Fixed

- Let setup installers declare the executable they provide so `crowded init`
  skips tools that are already available instead of replacing running files.
- Complete Windows room startup by answering cursor-position queries and
  placing guest processes in their cleanup job before they spawn descendants.
- Wait for Windows raw guests to settle and send Claude prompts as bracketed
  paste so automatic introductions arrive complete before submission.

## [0.24.1] - 2026-08-03

### Fixed

- Resolve Windows room and setup commands through PATH and PATHEXT; execute
  native programs directly and contain batch-shim process trees during cleanup.

## [0.24.0] - 2026-08-03

### Added

- Add a Windows named-pipe Doorbell client and server behind the existing
  platform boundary, preserving the authenticated JSON protocol.

## [0.23.3] - 2026-08-03

### Changed

- Use Windows-native file and directory symlink APIs for plugin installation
  and adapter links, allowing the full crate to cross-compile for Windows.

## [0.23.2] - 2026-08-03

### Changed

- Move Doorbell command-client Unix socket I/O behind a focused private module
  boundary without changing command or JSON behavior.
- Select the Doorbell local server implementation by platform while preserving
  its application-facing API and Unix behavior.

## [0.23.1] - 2026-08-02

### Changed

- Move shared MCP configuration adaptation and vendor-specific pane controls
  behind focused private module boundaries without changing behavior.

## [0.23.0] - 2026-08-02

### Added

- `crowded check` validates the current directory's `crowded.toml` without
  running initialization, setup actions, or synchronization.

### Changed

- Plugin adapter planning, hooks, links, persisted state, and rollback now live
  behind one private module boundary.

## [0.22.0] - 2026-08-01

### Added

- Add a normalized room `vendor` identity to configuration and live roster
  output so orchestration can select genuinely cross-vendor agents.
- Infer `anthropic` for Claude and `openai` for Codex; ambiguous guests remain
  `unknown` unless configured explicitly.

## [0.21.1] - 2026-08-01

### Changed

- Split the Doorbell into focused command, protocol, and socket-server modules
  without changing its public behavior.

## [0.21.0] - 2026-08-01

### Added

- Authenticated `crowded roster` discovery reports every room's numeric ID,
  name, guest, transport, live state, and peer-control availability as JSON.

## [0.20.4] - 2026-07-31

### Fixed

- Codex deliveries now use bracketed paste, so long final-result envelopes are
  submitted instead of leaving an extra newline in the prompt.

## [0.20.3] - 2026-07-31

### Changed

- The starter recipe now enables Code4Me Next Gen's reviewed vendor adapters,
  activating its advisory toolbox and Basic Memory nudges during initialization.

## [0.20.2] - 2026-07-31

### Fixed

- CCC's tool environment now constrains `mcp<2`, preventing the incompatible
  MCP SDK 2.0 release from removing the FastMCP import path CCC still uses.

## [0.20.1] - 2026-07-31

### Fixed

- Basic Memory is installed once with `uv tool install`; its MCP and project
  setup now call the persistent `basic-memory` executable instead of creating
  separate `uvx` environments.

## [0.20.0] - 2026-07-31

### Added

- MCP declarations can target selected `claude`, `codex`, or `opencode`
  clients, and project-local OpenCode npm plugins can be declared with
  `[[opencode_plugin]]`.
- Plugin declarations can enable their reviewed vendor adapters during
  `crowded init` with `adapters = true`; Claude plugins using the standard
  `hooks/hooks.json` layout are now recognized without extra manifest fields.
- The starter recipe gives Claude and Codex the Context Mode MCP and native
  hooks, while OpenCode receives its native Context Mode package without the
  conflicting duplicate MCP.

## [0.19.1] - 2026-07-31

### Fixed

- CCC is installed once with `uv tool install`; its MCP, initialization, and
  indexing now call the persistent `ccc` executable directly instead of
  creating `uvx` environments for every invocation.
- Setup executable checks now happen in sequence, allowing one setup action to
  install a command consumed by the next action.

## [0.19.0] - 2026-07-31

### Added

- The starter `crowded.toml` now declares pinned MCP runners for Basic Memory,
  CodeGraph, CocoIndex Code, and Context Mode.
- Workspace setup creates a shared local Basic Memory project, initializes
  CodeGraph, and initializes and indexes CocoIndex Code.

## [0.18.0] - 2026-07-31

### Added

- `crowded init` creates a starter `crowded.toml`, installs missing declared
  plugins, synchronizes the native toolbox, and runs declared setup commands
  once after validating the workspace configuration.
- `[[plugin]]` and `[[setup]]` workspace declarations, with successful setup
  markers stored under the ignored `.crowded/init/` directory.

## [0.17.0] - 2026-07-31

### Added

- `crowded plugin update PLUGIN [--ref REF]` refreshes an installed plugin from
  its recorded Git source while preserving skill, adapter, and plugin-data
  state.

## [0.16.1] - 2026-07-31

### Changed

- Startup house rules now say explicitly that Doorbell targets are numeric room
  numbers, identify `$CROWDED_ROOM` as the return address, and explain how
  delegated replies reuse the task ID.

## [0.16.0] - 2026-07-30

### Added

- The Conductor: authenticated peer requests for clearing context, selecting a
  model, and selecting reasoning effort.
- Per-room `allow_control` opt-in for destructive peer controls.
- Native launch adapters for Claude and Codex model/effort settings and
  OpenCode model settings.

### Security

- Control requests travel as structured Doorbell events and cannot be triggered
  by terminal output or ordinary whispers.
- Context clearing removes configured resume arguments before restarting the
  target CLI; unsupported vendor capabilities are rejected.

## [0.15.1] - 2026-07-30

### Fixed

- `plugin disable` now removes shared skill links from Claude, Codex, and
  OpenCode as well as disabling vendor adapters; `plugin enable` restores both.
- `plugin list` reports skill and adapter activation separately, making partial
  legacy state visible.

## [0.15.0] - 2026-07-30

### Added

- `crowded plugin preview|enable|disable` for explicit, reversible activation
  of installed plugins' vendor-native components.
- Native hook adapters for Claude and Codex, plus project-local OpenCode plugin
  and command links.

### Changed

- Room Pulse and plugin hooks now coexist through per-handler ownership instead
  of requiring Crowded to own an entire hook section.
- Removing a plugin first disables its enabled vendor adapters.

### Fixed

- Shared skills use OpenCode's project-local `.opencode/skills/` discovery path.

### Security

- Adapter previews show the exact executable hook commands before opt-in.
- Crowded records every inserted hook handler and symbolic link, refuses to
  replace existing paths, and removes only entries it owns.

## [0.14.1] - 2026-07-30

### Fixed

- Skills-only installation accepts native `.codex-plugin/plugin.json` and
  `.claude-plugin/plugin.json` manifests and reports missing manifests or skill
  directories explicitly.

## [0.14.0] - 2026-07-30

### Added

- `crowded plugin add|list|remove` for reversible, project-local installation
  of skills-only plugins from Git sources.
- Shared skill discovery through `.agents/skills/` for Codex and OpenCode and
  `.claude/skills/` for Claude, backed by one pinned local plugin checkout.

### Security

- Plugin manifests, skill names, frontmatter, Git references, symlinks, and
  destination ownership are validated before installation or removal.
- Downloaded executable hooks, MCP servers, and vendor-native plugin code are
  not installed by the initial skills-only slice.

## [0.13.0] - 2026-07-30

### Added

- A Room Pulse sidebar showing the latest lifecycle state reported by each
  agent without collecting prompts, commands, or tool output.
- Authenticated `crowded pulse` events and project-local Claude, Codex, and
  OpenCode hook adapters.

### Changed

- Shared Toolbox synchronizes native pulse hooks even when no shared MCP is
  declared.

## [0.12.0] - 2026-07-30

### Added

- `crowded toolbox preview|sync|remove` for reversible project-local Claude,
  Codex, and OpenCode MCP configuration.
- Private toolbox state that tracks and safely removes Crowded-managed
  configuration.

### Changed

- Synced projects load their native MCP files instead of receiving ephemeral
  command-line or environment adapters.
- `shell` transport identifies terminal-only rooms, which Shared Toolbox skips;
  `raw` rooms continue to require a supported native agent adapter.

### Fixed

- PTY guests now receive Crowded's launch directory explicitly; `portable-pty`
  otherwise defaults commands without a configured `cwd` to the user's home.
- OpenCode may add schema metadata or reformat `opencode.json` without forcing
  a toolbox remove-and-sync cycle.
- OpenCode introductions and peer messages now wait for its visible normal-mode
  idle prompt; automated delivery allows only one in-flight message per room.
- Terminal-only rooms skip the agent introduction and accept peer messages as
  soon as their shell is ready.

## [0.11.0] - 2026-07-30

### Added

- Shared stdio MCP declarations in `crowded.toml`.
- Ephemeral Claude, Codex, and OpenCode launch adapters that merge shared MCPs
  with each guest's normal configuration.

## [0.10.0] - 2026-07-30

### Added

- Local `crowded.toml` room profiles with names, commands, arguments,
  transports, and optional working-directory overrides.

### Changed

- Command-line room specifications override `crowded.toml`; rooms without a
  configured `cwd` inherit Crowded's launch directory.

## [0.9.0] - 2026-07-30

### Added

- Live room rosters in guest introductions.
- `F4` reintroduction for the focused room.

## [0.8.0] - 2026-07-30

### Added

- Any number of explicitly configured rooms, arranged in an automatic grid.

### Changed

- House rules now advertise the complete room-number range.

## [0.7.0] - 2026-07-30

### Added

- Optional `--task ID` and `--role ROLE` Doorbell metadata for temporary,
  per-message agent hats.
- Task and requested-role context in receiving prompts and Mailroom entries.

### Changed

- House rules now teach agents to reuse task IDs in replies and clarify that
  requested roles never become permanent room state.

## [0.6.0] - 2026-07-30

### Added

- A visible 20-message automatic-delivery fuse that pauses the room before a
  slow agent-to-agent loop can consume an unbounded amount of model usage.
- An explicit F3 fuse reset that resumes queued delivery within a fresh budget.

### Changed

- Mailroom queue entries now record whether delivery was manually paused or
  stopped by the fuse.

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
