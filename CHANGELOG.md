# Changelog

All notable changes to The Crowded Room will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.50.0] - 2026-11-27

### Added
- A native OpenCode room whose owned server dies is noticed within a few seconds: Crowded reaps its wrapper, shows one notice, takes the room offline and holds its mail. Ctrl+R revives the room on a new server and session; the house rules and the held mail are delivered once. A failed revive leaves the room offline with its reason. Only a refused connection or an exited wrapper counts as a dead server, so a busy server is never taken down.
- Plugin installs report, per plugin, which linked files OpenCode loads as entries, and warn about missing or dangling links.
- A plugin that ships a native OpenCode package gets a notice to declare it as `[[opencode_plugin]]`, and a warning when it would load twice (from the declaration and from a linked file or any OpenCode config, including `opencode.jsonc`, `OPENCODE_CONFIG` and `OPENCODE_CONFIG_CONTENT`). Crowded never writes the user's `opencode.json`.

### Changed
- `crowded init` writes a starter config that pins ponytail and code4me-ntg to commits, enables ponytail's adapters, and declares context-mode v2.2.4 as an OpenCode package (Claude and Codex keep v1.0.169). Plugin commit refs are cloned and checked out.

### Fixed
- Clearing a native OpenCode room now interrupts the previous session's run. Before, the old session kept working, editing files and sending mail after the clear. A failed interrupt is reported and does not undo the clear.
- `crowded plugin disable`, `update` and `remove` work after another tool rewrote a hook entry Crowded added, including Windows path spellings, and never remove a user's own command, even in the same handler.
- The native outbox reports why it could not open, instead of "the native outbox stopped", when its worker exits first.
- Launching a guest no longer fails at random with "Text file busy" on Linux; the spawn retries briefly.
- Several timing-sensitive tests now wait for the state they check, so CI fails far less often on unrelated runs.

## [0.49.0] - 2026-11-26

### Added
- The native outbox keeps outcome counters (accepted, failed, retired stale mail, unconfirmed, late-corrected) that survive the 100-row history limit, so a delivery success rate can be measured.

### Changed
- A native message whose delivery stays unconfirmed for 10 minutes now ends as `FAILED: delivery was not confirmed; it may have arrived; not resent`, with one notice, instead of staying uncertain forever. It is never resent. A late authenticated receipt still corrects it to accepted.
- Mail saved for an earlier launch of a room, which can never be delivered, now ends as `FAILED: sent for an earlier launch of this room; not delivered` instead of piling up as held. Existing rows are settled the first time the new version runs.
- Settling runs while delivery is paused or fused, and each change of state gives exactly one notice.

### Fixed
- A cleared native OpenCode or Claude room is taught the house rules again. Before, only the first launch and F4 sent them. Codex rooms already received them as the new session prompt.

## [0.48.0] - 2026-11-25

### Added
- CI runs the test suite and the Claude mod delivery tests on Windows (`windows-latest`) as well as Linux and macOS. Tests that cannot run on Windows are skipped one by one with a stated reason.
- On Windows, closing the console window, logging off, shutting down or pressing Ctrl+Break makes Crowded exit the same way as Ctrl+Q, so its owned servers are stopped. Ctrl+C stays a key for the guest and never quits Crowded.

### Changed
- On Windows an OpenCode native room's server, and the Headroom wrapper when there is one, run inside Crowded's Job Object. Crowded accepts the server only when its process is in that job and still running, stops the server by ending the job, and reports a failed stop. The job closes when Crowded exits, even abruptly, so no OpenCode or Headroom process is left behind. The job is named, so membership proves the process belongs to this Crowded's lifecycle, not that no other program of the same user could join it.
- OpenCode native rooms with Headroom are no longer refused on Windows.
- The CI actions run on Node 24 (`actions/checkout`, `actions/setup-node` and `actions/upload-artifact` v7); GitHub no longer warns about Node 20.

### Fixed
- On Linux, closing a pane whose guest had stopped reading no longer leaks a writer thread and a terminal descriptor: the writer stops within 200 ms. This happened, for example, on every restart of a busy or wedged room.
- On Windows, an OpenCode plugin file link pointed at a relative path that Windows could not follow; links now use the absolute path.
- Several timing-sensitive tests that failed at random on slow CI runners now wait for the state they check instead of a fixed time.

## [0.47.0] - 2026-11-23

### Added
- An OpenCode native room can be cleared in place (`crowded control N clear`): its owned server stays up, a new session is created on it, the terminal reopens on that session and the room is rebound under a new launch generation. A failed restart rolls the room back to its previous session, and a double failure says to clear again or restart Crowded.
- CI gates `cargo fmt`, `cargo clippy` (correctness group), the tests and the Claude mod delivery tests on every change, with a pinned toolchain and one script (`scripts/check.sh`) that runs the same commands locally.

### Changed
- A plain `crowded` launch starts native Codex and Claude rooms in a new conversation. Only `crowded resume` reopens a room's stored session, and a stored Codex session is reopened only when its rollout file is found under `~/.codex/sessions` for the same working directory; otherwise the room starts fresh with one notice. OpenCode native rooms already started fresh; reopening them on `crowded resume` is not supported yet.

### Fixed
- Mail to a native room is no longer held behind one uncertain message: an uncertain message stays visible and is never resent, and later mail is still delivered. An uncertain message from an earlier launch no longer holds the new launch either.
- A Claude message whose receipt arrives after the 2 s wait (the room was busy) is settled from that authenticated receipt and shows `ACCEPTED (submitted)` instead of staying uncertain. A held receipt settles it as held. A restart before the receipt leaves it uncertain.
- Mail to a busy Claude room no longer wedges. A message refused because the mod was not polling is offered again, in order, once it polls; a first attempt is not made while the mod cannot receive; and later mail shows a visible hold instead of a silent `QUEUED`. A message the mod itself held, a message too large for the mod and one whose save was not confirmed stay visible and no longer block later mail.
- A reason sent by the Claude mod with a held prompt is shown as `Claude mod: ...`, so it can never be mistaken for one of Crowded's own retryable holds and a claimed prompt is never sent twice.
- A Claude room whose live session differs from the id Crowded bound (for example after a resume) still connects: its per-room bootstrap connection file is kept for the whole launch.

### Known limits
- A busy Claude room shows `no Claude mod connected` while its own turn is running; Crowded cannot yet tell a busy mod from a stopped one.
- Mail held by an earlier build before an upgrade is marked `sent for an earlier launch of this room; not delivered` after the restart, and is not delivered.

## [0.46.1] - 2026-11-20

### Fixed
- Native Claude rooms no longer resume a session that does not exist. Crowded stopped choosing session ids with `claude --session-id`: a fresh start passes neither `--session-id` nor `--resume` and learns the real id from the room's own SessionStart hook. A reported id is resumed only when its session file exists under `<CLAUDE_CONFIG_DIR or ~/.claude>/projects/<cwd with every non-alphanumeric character replaced by ->/`; otherwise the room starts fresh and keeps the stored id.
- The Claude delivery mod works in Claude Code 2.1.328: it reads the room with `$.env.get` instead of `process.env`, which the mod sandbox does not provide, so it no longer reports "Crowded inbox not connected" and delivers nothing.
- Fresh Claude rooms sharing a working directory each get their own authenticated bootstrap connection (`_bootstrap.<room>.json`), which the first session report replaces with the session's file; the mod refuses a connection file that belongs to another room and does nothing without a numeric `CROWDED_ROOM`. Stale session files are removed on the next report and on exit.
- Startup mail for a Claude room is delivered after its mod registers. Held mail stays inside its own launch: the first session report of a launch keeps the launch generation, while a relaunch or a later session switch takes a new one and leaves earlier mail held. A held message is never overtaken by newer mail, and mail behind an uncertain message shows a visible hold.

### Known limits
- A `CLAUDE_CONFIG_DIR` exported inside a launcher wrapper, rather than in Crowded's own environment, is not seen when checking for a session file; the room starts fresh.
- A resumed Claude room is sent its welcome message again.

## [0.46.0] - 2026-11-19

### Added
- Native Claude rooms have a lifecycle ahead of delivery: Crowded chooses the session id with `claude --session-id`, saves the binding before the terminal starts, restarts with `claude --resume`, clears through the durable marker into a new id, and rebinds when the room's own SessionStart hook reports a new one.
- Native Claude rooms take mail: Crowded extracts its embedded delivery mod into `<cwd>/.crowded/claude-mod/` and launches `claude` with `--plugin-dir` for it on every start, restart, clear and resume. A submitted prompt shows `ACCEPTED by Claude (submitted; not proof it was acted on)`; a refused one stays held, and a collected-but-unreceipted one stays uncertain and is never resent. The mod is inert outside Crowded, and the connection file under `.crowded/claude-inbox/` is removed when its launch is replaced and on exit. Needs a Claude Code that loads `--plugin-dir` plugins (validated on 2.1.322); brand-new-user prompts, Windows and a real TUI `/clear` in a Crowded pane are not verified.
- A Claude room's `.claude/settings.local.json` gets a `SessionStart` hook entry that runs `crowded session-report`, next to the pulse entry, as a Codex room's hooks already had. A report from a Claude room rebinds it under a new launch generation, pending first like the other native rooms.
- A native Codex room can be restarted (F5, or Ctrl+R while it is offline), cleared and resumed in place. Restart and resume bring back the session Crowded stored for the room; clear durably marks the old thread cleared, then mints a new one with `codex exec` and reopens the room in it. Each relaunch binds the room under a new generation, so mail saved for the earlier launch stays `HELD`. Changing a native room's model or effort is still refused, and OpenCode native rooms still cannot be restarted, cleared or resumed in place.
### Fixed
- Clearing a native Codex room fails with a visible error when its cleared marker cannot be saved, and starts no new thread. Two overlapping clears can no longer leave the stored session older than the one the room is bound to.

## [0.45.0] - 2026-11-05

### Added

- Native delivery for OpenCode rooms, opt-in per room with `native_delivery = true`. The room runs its own Crowded-owned `opencode serve` on loopback with a random per-room password, and its terminal attaches to it. The intro, Doorbell mail, Ctrl+W messages and F4 reintroductions are saved first in `.crowded/native-outbox.json`, then handed to OpenCode's prompt queue, one at a time per room and in order, instead of being pasted into the terminal. See the README for the statuses shown in the mail log (F2) and the Doorbell answers (`accepted`, `queued`, `held`, `uncertain`).
- A lost reply is looked up in OpenCode's inbox with a read-only request and settled without resending. Mail saved for an earlier launch stays visible as held and is never delivered to a new session.
- Native rooms may use Headroom.

### Changed

- The Room Pulse shows a native room's real state, as for any other room.
- The welcome message ends by telling a room to check the roster and stand by.
- Crowded leaves the same way on SIGHUP, SIGINT and SIGTERM as after Ctrl+Q, and the owned servers go with it. SIGKILL still leaves them running.

### Fixed

- Delivered native mail was shown as failed when a room hook appended text to the prompt; the stored text now only has to start with the sent text.
- Accepted native mail no longer counts toward the outbox capacity, and a message whose save was not confirmed is held instead of being sent later.

## [0.44.1] - 2026-11-04

### Changed

- `tempfile` moved from a dev-dependency to a runtime dependency.

### Fixed

- OpenCode rooms receive whispers as a bracketed paste.
- Plugins whose `plugin.json` lists `hooks` as an array of files (ponytail 5.3.x) now install and update. A string, an array, or no `hooks` value is accepted, and the entries of every listed file are merged into one hooks target per tool. A file with no hook entries adds nothing.
- Crowded no longer freezes while delivering a message to a room that is slow to read its input. Every write to a room's terminal now goes through a bounded per-room writer thread, so a large message to a busy OpenCode room can no longer block drawing, the Doorbell, or other rooms. A delivery acknowledgement now means the message was accepted into the room's write queue. A full queue rejects the write instead of blocking, the app reports a rejected keystroke as a notice, and a queued Doorbell envelope that meets a full queue stays queued and is retried in order instead of being dropped.
- OpenCode rooms launched through Headroom keep their own configured model. Headroom replaces `OPENCODE_CONFIG_CONTENT`, so each wrapped room now gets a private `OPENCODE_CONFIG` file carrying its model and MCP servers instead of falling back to the shared recent-model list. A plain-JSON `OPENCODE_CONFIG` already set for the room is merged as the base; a JSONC or unreadable one fails the launch with an error naming the path.
- OpenCode v2 rooms are introduced automatically. The readiness check now ignores blank screen rows, and Room Pulse hook events for Thinking and Working count as the busy cycle that the delivery gate waits for.

## [0.44.0] - 2026-10-02

### Breaking

- OpenCode rooms now target OpenCode v2; OpenCode 1.x is no longer supported.

### Changed

- OpenCode room models are passed through per-room config instead of a root command-line flag, and rooms start with `--standalone`; the existing `--yolo` option remains accepted.
- Changing the model of a resumed OpenCode room updates its saved session through OpenCode's API before relaunch. Slash-containing model IDs and `#variant` values are supported.
- OpenCode session capture, resume repair, model checks and cost reporting now read the v2 session store, including model variants.
- OpenCode Room Pulse details now come from v2 session messages. The generated pulse plugin uses the v2 plugin API, and `crowded toolbox sync` upgrades the managed legacy plugin in place.
- The starter configuration now delivers context-mode to OpenCode rooms as an MCP server. Its currently published plugin uses the v1 API, and its plugin hooks require an upstream v2-compatible plugin.

### Fixed

- OpenCode's v2 idle home screen is recognized as ready for automated input.
- `crowded toolbox preview` no longer changes files when saved toolbox state is stale.

### Upgrade notes

- In an existing `crowded.toml`, add `opencode` to the context-mode MCP `clients` list and remove its `[[opencode_plugin]]` entry, then run `crowded toolbox sync` once after installing 0.44.0 to upgrade the managed OpenCode pulse plugin.
- Existing OpenCode sessions keep their current model until it is changed with the model control.

## [0.43.4] - 2026-09-04

### Fixed

- A Headroom-wrapped room no longer receives its house-rules intro while the wrapper is still bootstrapping: its intro now waits for genuine guest readiness instead of being forced by the generic fixed readiness ceiling, so no fixed timeout pastes into a wrapper that is still launching its guest. Non-Headroom rooms keep the existing ceiling fallback for stuck guests.
- On Windows, Headroom rooms now bootstrap through one serial lane: a second Headroom room does not begin bootstrap until the prior Headroom room reaches the same ready boundary used for intro delivery, so two wrappers never start their proxies at once. Non-Headroom rooms and every room on non-Windows start concurrently, exactly as before.

## [0.43.3] - 2026-09-03

### Fixed

- A room whose child process is alive but unresponsive can now be force-restarted with F5; `Ctrl+R` alone could never recover it because it only revives an already-exited child.
- A Doorbell whisper that lands unsubmitted in a guest room's composer is now automatically resubmitted instead of being silently dropped; a related bug that resent a stray Enter after a normally consumed whisper was also fixed.

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
