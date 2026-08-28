# Changelog

All notable changes to The Crowded Room will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.43.2] - 2026-08-28

### Fixed

- Unix Doorbell requests now accumulate partial writes under a bounded deadline, so valid room-to-room envelopes are not dropped at the former one-second read boundary.
- `crowded check` now rejects raw guests that shared MCP injection would make launch reject, before a room is started.

## [0.43.1] - 2026-08-27

### Added

- `crowded toolbox resync` refreshes managed native-toolbox files after configuration changes without a manual `toolbox remove` followed by `toolbox sync`.

### Fixed

- Claude task and todo detail signals are installed as `PreToolUse` tool matchers instead of unavailable top-level hook events.
- Room Pulse routes detail-only handling through a single safe helper when no supported adapter detail is available.

## [0.43.0] - 2026-08-27

### Added

- `crowded.toml` room entries' scheduling fields (`model_tier`, `cost_tier`, `capabilities`) are now editable directly in the F1 config overlay, alongside the existing `fuse_size` field.
- Room Pulse now shows each room's cross-vendor sub-agent and todo detail inline beneath its pulse state, replacing the old modal Detail overlay and its `d` binding, which could steal terminal input mid-session. Detail collection (Claude hook accumulation, OpenCode/Codex artifact collectors) runs on background threads with a bounded 2s refresh cache and never blocks the draw loop; completed todos are filtered out and entries render concisely within the panel.

## [0.42.0] - 2026-08-26

### Added

- A config overlay (`F1`) shows the loaded `crowded.toml` rooms and lets `fuse_size` be edited inline. Saving parses the input, persists it back to `crowded.toml` with `toml_edit` (preserving every other section), and applies the new limit to the running delivery fuse immediately, with no restart. Invalid input shows an inline error and leaves the file untouched. First slice of a Configuration UI; more fields and sections follow in later work.

## [0.41.0] - 2026-08-25

### Added

- `crowded roster --json` and the welcome roster line now show a room's self-reported model, even when the operator never ran `control model`. The hook pulse wire carries an optional model field, `crowded pulse <state> --model` forwards it, and a fresh self-report fills an unconfigured value while `control model` still wins as an explicit override.
- Real Claude, Codex, and OpenCode hooks now populate that self-report automatically: `crowded pulse <state> --hook-stdin` reads the vendor hook's stdin JSON and extracts a model field when present (Codex: every hook event; Claude: `SessionStart` only, not guaranteed; OpenCode: the plugin `chat.message` hook), degrading cleanly to a plain pulse otherwise.

### Fixed

- A whisper injected into a room whose CLI had not finished starting could land as typed-but-unsubmitted text: the injection path wrote the body then submitted after a flat delay with no readiness check. Mailroom injection now resends exactly one Enter once the target becomes ready or a bounded ceiling is reached, and never double-submits an already-ready pane.
- Windows plugin skill links (`.claude/skills/`, `.agents/skills/`, `.opencode/skills/`) are now always created as junctions with an absolute target. The previous logic attempted a plain symbolic link first and only fell back to a junction on a symlink-privilege failure, so whether a plugin's skills were discoverable by Claude Code and Codex CLI on Windows depended on the host machine's ambient privilege state at install time rather than being reliable by design.

## [0.40.0] - 2026-08-25

### Added

- `crowded.toml` room entries can now declare optional scheduling metadata: `model_tier` (`fast`, `balanced`, or `deep`), `cost_tier` (`low`, `medium`, or `high`), and `capabilities` (any of `produce`, `implement`, `validate`, `qa`, `audit`). Values are validated at config load; omitting them is fully backward compatible. `crowded roster --json` exposes the resolved values per room under a new `scheduling` object, kept separate from the existing `capabilities` adapter-feature-matrix field.

## [0.39.0] - 2026-08-25

### Added

- The Room Pulse panel now colors each room's state word by its resolved state: offline/error render red, ready renders green, and thinking/working/starting render yellow. The focused room's title line keeps its existing cyan highlight, unaffected by the new state coloring.

## [0.38.1] - 2026-08-24

### Fixed

- `claude_project_directory` still failed to match Claude CLI's real on-disk session directory on Windows for any cwd containing a `.` (e.g. a dotted username like `Bruno.O`): the prior fix only added `:` to a hand-picked separator list. Verified directly against a Windows machine's `~/.claude/projects` and `~/.claude.json`, the sanitizer now replaces every non-alphanumeric UTF-16 code unit with `-`, matching Claude CLI's own rule byte-for-byte (including astral characters, which Claude CLI's `u`-flag-less regex sanitizes per surrogate half).

## [0.38.0] - 2026-08-22

### Added

- `crowded mcp list|add|remove` manages `[[mcp]]` servers in `crowded.toml` directly, the first slice of a full configuration editor. `add`/`remove` edit the file via `toml_edit`, preserving all existing comments and formatting, validate against the existing MCP rules (name charset/length, duplicates, command/url/transport combinations), and immediately re-sync the native per-client MCP files.

## [0.37.1] - 2026-08-21

### Fixed

- A resumed room never showed a usage cost in the Room Pulse panel, even though its exact session id was already known before the process started: the cost gate read a per-process capture cell that only the intro-triggered capture flow wrote, and a resumed room skips that intro. The cell is now seeded from the persisted session-state store at spawn whenever a valid session mapping already exists, so any room with a known session shows its cost immediately.

## [0.37.0] - 2026-08-21

### Changed

- The Room Pulse panel is easier to scan: each room's title now carries compact `H`/`S` badges instead of bracketed `[headroom]`/`[session]` tags, the state line drops the diagnostic pulse-source label (still available via `crowded roster --json`), a blank line separates consecutive rooms, and the panel is 36 columns wide instead of 30. A `Total:` line now sums every room's known usage cost, noting the known/total count when some rooms don't have one yet.

## [0.36.1] - 2026-08-21

### Fixed

- Claude and Codex usage-cost figures were roughly 1,000,000x too large: `token_pricing.toml` rates are USD per 1,000,000 tokens (the standard vendor convention), but the cost calculation multiplied raw token counts by the configured rate with no division by 1,000,000. OpenCode was unaffected, since it reports the cost its own session already tracks rather than using the pricing table.

## [0.36.0] - 2026-08-21

### Changed

- Operator-supplied token pricing now lives exclusively in the optional sibling `token_pricing.toml`; a `[[token_pricing]]` table left in `crowded.toml` is rejected as invalid configuration.

## [0.35.0] - 2026-08-21

### Added

- The Doorbell roster now reports a per-room usage-cost estimate: Claude and Codex costs are computed from their own transcript token usage against an operator-configured `token_pricing` table in `crowded.toml`, while OpenCode rooms report the cost their own session already tracks. A room with no configured rate, or an unavailable/unparsable transcript, reports `"unknown"` rather than a fabricated figure.
- The Room Pulse panel now shows that same cost alongside each room's state, once the room has captured its session, refreshed on an interval rather than recomputed every frame.

## [0.34.0] - 2026-08-21

### Added

- `crowded.toml` now supports a top-level `fuse_size` field controlling the Doorbell automatic-delivery fuse: omitted keeps the default of 20, and `0` disables the fuse so automatic delivery never pauses.

## [0.33.0] - 2026-08-21

### Added

- `crowded --help` and `crowded -h` now print a usage summary listing every subcommand (send, control, resume, pulse, roster, check, init, plugin, toolbox) and exit 0, instead of falling through to the GUEST parser's error.

## [0.32.1] - 2026-08-20

### Fixed

- Clearing a room now persists a fresh-session marker, so a later resume cannot restore the pre-clear session or fall back to the most recent one.

## [0.32.0] - 2026-08-12

### Changed

- The welcome roster now names each room's configured model and effort, read from the live pane at intro time instead of frozen at startup, so a room reconfigured by a peer control announces what the Doorbell roster JSON reports now. Rooms with no configured value say so explicitly: "unconfigured" when the adapter accepts the control but nothing was set, "unsupported" when it cannot accept one.
- House rules now ask each room to announce its configured model and effort in its first response, and replace the untrusted-input warning with guidance to ask the originating room when a delegated task is unclear.

## [0.31.0] - 2026-08-12

### Changed

- Room Pulse now highlights the currently focused room in cyan and uses a 30-column panel, making focus clearer and leaving more room for status details.

## [0.30.2] - 2026-08-12

### Fixed

- Scrolling no longer occasionally types the text of a mouse report, such as `[<65;176;43M`, into the focused room's prompt. A wheel report split across two terminal reads was being mistaken for an Esc keypress followed by ordinary typing and forwarded to the guest; such a run is now recognized and dropped, while anything that turns out to be real typing is passed on in the order it was pressed.
- The wheel and the page keys now scroll a Codex room. A guest that renders inline inside a scroll region anchored to the top of the screen, as Codex does, builds room history again instead of having every line that leaves the viewport discarded, so there is something to scroll back through.
- Two OpenCode rooms sharing a working directory no longer resume each other's conversation, and a room no longer resumes a conversation that belongs to a different model. Which room owns which session is now decided once across the whole room slate, and a room whose recorded session is rejected starts fresh instead of continuing the newest conversation in the directory.

## [0.30.1] - 2026-08-11

### Fixed

- Wheel scrolling no longer stalls or overshoots in terminals that report a single notch as a long burst of identical mouse events. Each burst now produces one scroll action, and Crowded requests button reporting and SGR encoding directly instead of also enabling the drag and any-motion reporting it never reads.

## [0.30.0] - 2026-08-10

### Added

- Roster capability output now lists each supported control explicitly while retaining the legacy control summary for backward compatibility.

### Changed

- Room Pulse now uses human-readable source labels and shows the age of hook-backed states in the TUI.

## [0.29.0] - 2026-08-10

### Added

- Room Pulse and roster output now share a freshness-aware state resolver, expose the state source and hook age, and recover from stale transient hook reports when delivery readiness is demonstrated.
- Roster output now reports adapter-derived control capabilities and supported effort levels without claiming an available model catalogue or unsupported OpenCode effort control.

## [0.28.3] - 2026-08-10

### Fixed

- Welcome delivery now retries failed writes, uses a bounded readiness fallback, and follows the same capture path for fresh starts, room reintroduction, and non-resume respawns.
- Codex session capture now waits for delayed rollout creation and accepts current metadata containing both `id` and `session_id`.
- Persisted session state now follows stable numeric room identity, collapsing stale entries when a room changes guest or vendor while preserving sibling rooms and other working directories.

## [0.28.2] - 2026-08-07

### Fixed

- The Room Pulse panel no longer gets stuck on "starting" for a resumed room. It was rendering the room's raw, self-reported hook state directly; a resumed room skips the intro whisper, so no later Stop hook ever runs to self-report "ready", and the panel had no way to notice the room had become deliverable. The panel now cross-checks the same `roster_state` (delivery gate + live input-ready reading) that `crowded roster --json` already used, so both agree instead of only the JSON roster ever recovering.
- `opencode_input_ready` no longer matches on the OpenCode idle prompt phrase when it's wrapped across a line boundary by a narrow pane, and its busy-marker check is confined to a bounded tail window so resumed conversation history can't be mistaken for the current prompt.

## [0.28.1] - 2026-08-07

### Fixed

- `headroom_args` now land after the wrapped program name instead of before it (`headroom wrap <program> <headroom_args...> <args...>`), matching `headroom wrap`'s actual CLI contract: the tool name is `wrap`'s own subcommand (e.g. `headroom wrap claude`), and headroom's own flags belong to that subcommand, not before it.

## [0.28.0] - 2026-08-07

### Added

- `crowded resume` launches the whole room layout with every supported guest resumed from the start, instead of requiring a peer room to send a control message first.

## [0.27.0] - 2026-08-07

### Added

- `crowded control ROOM resume` restarts the target CLI with each vendor's "continue the most recent conversation" flag (Claude and OpenCode: `--continue`; Codex: `resume --last`), mirroring `clear`'s restart but keeping context instead of dropping it.
- Room config gains `headroom_args`: extra flags for the `headroom` wrapper itself, inserted after the wrapped program name and before the program's own args (`headroom wrap <original-command> <headroom_args...> <original-args...>`). Ignored when `use_headroom` is off or the binary is missing.

## [0.26.1] - 2026-08-06

### Fixed

- Headroom-wrapped rooms now get extra quiet-time grace before the house-rules intro is delivered, so the `headroom wrap` spawn-then-exec hop's own startup pause is no longer mistaken for the guest CLI being ready.

## [0.26.0] - 2026-08-06

### Added

- Room config gains `use_headroom`: wraps a room's launch through the `headroom` wrapper binary (`headroom wrap <original-command> <original-args...>`) when installed on `PATH`; falls back to the unwrapped launch silently otherwise. `crowded roster` and the Room Pulse sidebar report which rooms are actually running under Headroom via a live `headroom` field / `[headroom]` tag.

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
