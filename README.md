# Crowded Room

Crowded Room runs terminal agents together in one Ratatui interface. It keeps room
configuration, shared tools, and room-to-room messages in the workspace rather
than in each agent's global configuration.

Crowded Room is intentionally opinionated about multi-agent work. Its starter
configuration recommends a small working stack:

- [**Code4Me**](https://github.com/indie-hub/code4me-ntg) turns a request into
  a scoped task with independent validation.
- [**Ponytail**](https://github.com/DietrichGebert/ponytail) keeps the
  implementation to the smallest solution that works.
- [**Context Mode**](https://github.com/mksglu/context-mode) keeps large
  inspection output out of an agent's active context.
- [**Headroom**](https://github.com/headroomlabs-ai/headroom) is recommended
  for supported agent rooms; it optionally wraps a CLI to compress agent
  context and tool output.
- [**CocoIndex Code**](https://github.com/cocoindex-io/cocoindex-code) indexes
  the codebase for structural and semantic discovery.
- [**Basic Memory**](https://github.com/basicmachines-co/basic-memory) keeps
  durable workspace knowledge shared across rooms.

These plugins are configurable, but together they are the recommended way to
plan, build, and validate work in a Crowded room. Planned work lives on the
project Trello board.

See [CHANGELOG.md](CHANGELOG.md) for released changes.

## Install

Crowded is built from source. Install a current Rust toolchain, then:

```console
git clone https://github.com/indie-hub/crowded.git
cd crowded
cargo install --path .
```

The rest of this guide uses the installed `crowded` command. From an uninstalled
checkout, prefix a command with `cargo run --`; for example,
`cargo run -- init`.

## Start a workspace

In the project directory where the rooms should run:

```console
crowded init
```

The first run writes `crowded.toml`, adds `/.crowded/` to `.gitignore`, and
stops for review. Run `crowded init` again after review to install declared
plugins, synchronize the shared toolbox, and complete pending setup actions.

Check the reviewed configuration without changing the workspace:

```console
crowded check
```

The starter configuration includes Claude and Codex rooms plus optional
Code4Me, Ponytail, Context Mode, CocoIndex Code, CodeGraph, and Basic Memory
integration. The first setup of CocoIndex Code can download several GB and
asks you to select an embedding model.

## Configure rooms

Crowded has native raw-room support for Claude Code (`claude`), Codex (`codex`),
and OpenCode (`opencode`). OpenCode rooms require OpenCode v2; OpenCode 1.x is
no longer supported. Any terminal command can instead run as a
`transport = "shell"` room. Set `use_headroom = true` to wrap a supported room
with Headroom when its `headroom` executable is on `PATH`. Pass Headroom flags
with `headroom_args`, for example `headroom_args = ["--no-serena"]`.

`crowded.toml` lives in the directory where you launch Crowded. A minimal
configuration is:

```toml
[[rooms]]
name = "Claude"
command = "claude"
vendor = "anthropic"
transport = "raw"
allow_control = true
capabilities = ["implement", "validate"]

[[rooms]]
name = "Codex"
command = "codex"
vendor = "openai"
transport = "raw"
allow_control = true
```

Use `transport = "raw"` for supported agent terminal interfaces. A normal
terminal can use `transport = "shell"`:

```toml
[[rooms]]
name = "Terminal"
command = "/bin/zsh"
args = ["-l"]
transport = "shell"
```

Optional room fields include `args`, `cwd`, `model_tier` (`fast`, `balanced`,
or `deep`), `cost_tier` (`low`, `medium`, or `high`), and `use_headroom = true`
when the `headroom` executable is on `PATH`.

### Native OpenCode rooms (opt-in)

An OpenCode room with `native_delivery = true` runs its own Crowded-owned
`opencode serve` on `127.0.0.1` (OpenCode 2.2.5 or newer) and attaches its
terminal to it with `--server` and `--session`, instead of OpenCode's private
`--standalone` server. The server password is random per launch and reaches
OpenCode only through the `OPENCODE_PASSWORD` environment variable of the server
and the terminal; it is never put on a command line or saved. Both processes
keep your normal environment so your OpenCode login still works. OpenCode has no
option to hide the variable from the tools it runs, so a tool the agent runs can
read it and call the owned server directly; treat a native room's agent as
trusted with its own server.

The setting needs `transport = "raw"` and cannot be combined with
`--standalone`, `--server`, `--session`, `--continue`, `--fork` or `--port`.
The server gets the same OpenCode environment as the terminal (the room's
configuration, model override and Crowded's hook variables), because the server
is what runs the agent. Servers start in parallel before Crowded takes over the
screen, so a slow start shows a one-line message instead of a frozen terminal. A
native room that cannot start its server stops Crowded at startup with an error;
it never falls back to another launch path.

`native_delivery` can also use `use_headroom = true`. Crowded then runs the
owned server as `headroom wrap opencode <headroom_args> serve --hostname
127.0.0.1 --port 0`, so the agent's model traffic goes through Headroom; the
terminal only attaches to that server and is not wrapped a second time. Headroom
rewrites the OpenCode configuration file it is given (it adds its provider and a
`.headroom-backup` file), exactly as it already does for other Headroom rooms, so
the room's own configuration is handed to it as a private file. Crowded still
verifies the server: the authenticated `/api/info` must report a loopback
server, and its pid must be the process Crowded spawned or its direct child (the
wrapper is the parent in that case). This combination is refused on Windows,
where the parent of the reported pid cannot be checked.

When a Headroom native room stops, Crowded sends SIGTERM to the wrapper (which
stops OpenCode) and then stops the server pid if it survived; the Headroom proxy
is deliberately left running because later Headroom rooms reuse it. As with
SIGKILL on Crowded itself, a hard kill of a wrapper can leave its OpenCode server
behind.

Mail to a native room never goes to its terminal. The intro, doorbell mail,
Ctrl+W messages and F4 reintroductions are saved first in
`.crowded/native-outbox.json` (private, restored on restart), then handed to
OpenCode's own prompt queue, one at a time per room and in order. A message that
arrives while the agent is working waits its turn; an idle agent starts a turn.
Typing in the room works as usual. The mail log (F2) shows each message's
status:

| Status | Meaning |
| --- | --- |
| `QUEUED` | Saved here, not sent yet (waiting its turn, or delivery is paused or the fuse has tripped). |
| `SENDING` | A request is in flight. |
| `ACCEPTED by OpenCode` / `ACCEPTED by Codex` | The room's own CLI admitted it into its queue. This is not proof the agent read it. |
`ACCEPTED by Claude (submitted; not proof it was acted on)` | Crowded's own mod submitted the prompt to the Claude session. Submission is not proof the agent acted on it. |
| `UNCERTAIN` | The reply was lost, so it may have been accepted. It is never resent. Crowded looks for it in OpenCode's inbox with a read-only request and settles it if it is found; until then later mail to that room waits behind it. |
| `HELD` | Saved but not sent (the request could not be made, or the mail belongs to an earlier launch of the room). It is kept and is never moved to another session. |
| `FAILED` | Rejected, for example by a wrong login (HTTP 401) or because OpenCode holds different content under the message ID. |

The doorbell answers a send with how far the message got within about a second
and a half: `accepted`, `queued` (saved, still waiting), `held`, `uncertain`, or a
rejection with the reason when it could not be saved. `queued` means saved
locally, not delivered.

A restart of Crowded gives each native room a new server and session. Mail
saved for an earlier launch stays in the log as `HELD` or `UNCERTAIN`; it is not
delivered to the new session and does not block it. A native OpenCode room still
cannot be restarted, cleared, resumed or reconfigured in place, and `crowded
resume` starts it a new session and says so. There is not yet a way to release or
discard held mail from inside Crowded.

When Crowded is hung up on, interrupted or terminated (SIGHUP, SIGINT,
SIGTERM) it leaves the same way as after Ctrl+Q, and its owned servers go with
it. SIGKILL cannot be caught, so `kill -9` on Crowded (or a crash) leaves its
OpenCode servers running. They are the `opencode serve --hostname 127.0.0.1
--port 0` processes and can be stopped by hand; the next start never reuses them.

```toml
[[rooms]]
command = "opencode"
transport = "raw"
native_delivery = true
```

### Native Codex rooms (opt-in)

A Codex room with `native_delivery = true` runs in two phases. Crowded first
creates the room's session with `codex exec --json` and the room's initial
instructions (the house-rules prompt), reads the new thread UUID from the CLI's
`thread.started` event, and then starts the room's terminal with
`codex resume <UUID>` and the room's usual arguments. That session is the room's
permanent session, not a throwaway. The cost is one model turn per room launch.
Each mint runs on its own bounded worker, so a room becomes usable the moment
its own session exists: other rooms and the terminal do not wait for a slow one,
and a room still minting shows `starting session...` while its mail is held.

The binding is exactly the UUID Crowded minted for that room, and it is saved
before the room is allowed to start; if saving fails the room is not started and
the error names the UUID. A stored session is resumed only when Crowded minted
it: an id from an older capture or another tool is ignored and the room mints a
fresh session. Nothing scans the rollout directory, chooses the newest session,
or matches a working directory, so two Codex rooms in one directory each get
their own thread and cannot adopt each other's or an unrelated process's
session.

The exec phase gets the room's launch options that `codex exec` accepts, such as
`-c`, `-m` and `-s`, and leaves interactive-only options such as `--search`,
`--ask-for-approval` and `--no-alt-screen` on the resumed terminal. When
Headroom wraps the room, the exec phase runs through the same wrapper, arguments
and environment as the terminal.

If the exec phase fails, times out, or reports no valid UUID, the room is not
started at all: no interactive Codex runs without a session, the reason is
shown in the room and reported at startup, and its mail is held.

On a later Crowded start the room resumes the session stored for it, with no
second exec. A room with no stored session mints one on that launch, and mail
saved for an earlier launch stays held for the old one.

Mail is delivered through Codex's own queue command:

```console
codex queue --thread <UUID> --message <text>
```

The command is built as an argument array and launched through Crowded's
platform-aware launcher, so the exact text — newlines and Unicode included —
arrives as one argument and the envelope is not rewritten. Exit status alone is
not acceptance: the message is `ACCEPTED by Codex` only when the command exits
successfully and prints `Queued message {id} for thread <UUID>.` for that exact
thread. A matching line with a failed exit may still have submitted, so it
becomes `UNCERTAIN`; a rejection with no submission evidence is `FAILED` with a
short reason. Process time and captured output are bounded; if the command has
to be killed after it may have submitted, the message becomes `UNCERTAIN` and is
never queued again. Codex has no caller-chosen idempotency key, so Crowded
offers no reconciliation for it: `uncertain` stays uncertain for review.

`accepted` means Codex queued the message; it does not mean the agent consumed
or acted on it. A Codex build without the `queue` command is reported as a
failure with a message to update it; Crowded never falls back to paste.
`native_delivery` needs `transport = "raw"`. If the bound room's process exits,
new mail is refused and saved mail is held with that reason; it is never queued
into the old thread. Each launch binds with its own generation, so mail saved
for an earlier launch can never match a later session. A native Codex room can be
restarted (F5, or Ctrl+R while it is offline), cleared and resumed in place.
Restart and resume bring back the stored thread; clear durably marks the old
thread cleared, then mints a new one with `codex exec` and reopens the room in
it. Every relaunch binds under a new generation, so mail saved for the earlier
launch stays `HELD`. If the cleared marker cannot be saved, the clear fails with
a visible error and starts nothing, so the previous session stays the one a later
resume uses. A native Codex room's model and effort still cannot be changed in
place, and the delivery pause and fuse still apply.

```toml
[[rooms]]
command = "codex"
transport = "raw"
native_delivery = true
```

### Native Claude rooms (opt-in)

Crowded never guesses a Claude session id. A fresh room starts with no
`--session-id` and no `--resume`, waits for the room's own hook to report the
real session, and only then binds native delivery to that reported session.
`control N restart` and `control N resume` resume a hook-reported session with
`claude --resume <id>`; `control N resume` without a reported session fails
with a visible reason and starts nothing. `control N clear` writes the durable
cleared marker first and then starts fresh without guessing a session. A
user-supplied `--resume`, `--continue`, `--session-id` or `--fork-session` is
refused for such a room, because the session is learned from the room's hook.

With `native_delivery = true` the launch also passes `--plugin-dir
<cwd>/.crowded/claude-mod`: the delivery mod ships inside the Crowded binary
and is extracted there at launch, rewritten only when its content digest
differs. Both the directory and the files are private to the user where the
platform supports it, and a room whose mod cannot be written does not start.
Every launch passes `--plugin-dir` exactly once; a user-supplied
`--plugin-dir` is left alone and gets no Crowded mod.

A Claude relaunch binds the room before it starts the terminal and does not
roll either back. If the start fails after that, the room is reported with
`control N clear` and may stay recorded or bound on a session that did not
start.

Mail to a Claude room is saved first in `.crowded/native-outbox.json`, then
handed to the room's loopback inbox, which its mod polls every 500 ms and
submits with the prompt API. A receipt of `started` means the prompt was
submitted, not that the agent acted on it: the mail log shows `ACCEPTED by
Claude (submitted; not proof it was acted on)`. A refused submission stays
`HELD`; a collected-but-unreceipted one stays `UNCERTAIN`, never resent. Mail
saved for an earlier generation stays held, never delivered to a new session.
A stored reported id is resumed only when its exact session file exists: Claude
keeps sessions at `<config>/projects/<cwd-encoded>/<id>.jsonl`, where
`<cwd-encoded>` is the cwd with every character that is not a letter or digit
replaced by `-` (`/Users/bruno/.meridian` becomes `-Users-bruno--meridian`).
If that file is absent, or the cwd encodes differently (a very long path Claude
may truncate, for instance), the room starts fresh and the hook reports the new
session. No id is guessed and no stale id is retried. A reported id whose file
cannot be found is kept, not erased: that launch skips `--resume` and the next
report replaces the id, so a check that can only fail safe never loses what the
room reported. The directory that is checked is `CLAUDE_CONFIG_DIR` (or `HOME`)
from the room's launch environment; Crowded reads the room's variables, but
`crowded.toml` has no per-room environment key, so set `CLAUDE_CONFIG_DIR` in
the environment Crowded itself runs in. A launcher wrapper that exports
`CLAUDE_CONFIG_DIR` inside the child only is invisible to the check: the stored
id is kept and the launch starts fresh, which is the fail-safe result.

Before its session is known, a fresh room's mod reads the per-room bootstrap
connection file `<cwd>/.crowded/claude-inbox/_bootstrap.<room>.json` (named by
`CROWDED_ROOM`), holding the inbox URL and the room's token. Once the hook
reports the real session, the bootstrap file is replaced by
`<cwd>/.crowded/claude-inbox/<session id>.json` and the same receiver stays
bound. Connection files are removed when their launch is replaced and on exit.
The mod is inert outside Crowded: with no live file it registers and submits
nothing. Both live under `.crowded/`, which `crowded init` already adds to the
project's `.gitignore`.

This needs a Claude Code that loads `--plugin-dir` plugins and exposes the mod
prompt API (validated on 2.1.322). Not verified: approval or trust prompts for
a brand-new user, Windows, and a real TUI `/clear` in a Crowded pane.

### Session reports: following /clear and session switches

When a human clears a native room (`/clear`) or its CLI starts a new
session, Crowded only learns the new session id if the room's own hook
tells it. A hook or TUI plugin calls:

```console
crowded session-report --session <id>
crowded session-report --pending
crowded session-report --session <id> --pending
```

The id may also arrive as JSON on stdin with `session_id` (Codex) or
`sessionID` (OpenCode). Stdin is only read when no `--session` was given
and stdin is not a terminal, and the read is always bounded. With neither
`--session` nor `--pending` stdin is the only place a report can come
from, so the command waits up to **5 seconds** for a payload that arrives
late and stops as soon as the payload is complete or the pipe reaches
EOF. With `--pending` the bound stays at **500 ms**, so a hook always
answers promptly. An open pipe that never delivers anything never hangs
the command. A session id key that is present with any value that is not
a string -- a `null`, a number, a boolean, an array or an object, under
either spelling -- is rejected before anything is contacted, even with
`--pending`; only a missing key counts as absent, so `--pending` still
reports on its own when neither key is there. The room
is never taken from an argument: the report is
authenticated by the calling room's own token, and a bad token, an unknown
room, a non-native room, or a vendor other than OpenCode, Codex or Claude is
rejected or ignored with a short reason that changes nothing.

The command is silent on stdout in every outcome (hook output can become
model context); failures get at most one short stderr line. Without `--ack`
it always exits 0: a hook must never break its CLI, so the Codex
`SessionStart` hook passes no flag. The id is validated before anything is
contacted (non-empty, at most 128 bytes, letters, digits, `-`, `_` or `.`);
anything else is ignored on the spot.

Crowded writes the same `SessionStart` entry for a Claude room, next to the
pulse entry, in `.claude/settings.local.json`. A report from a Claude room
rebinds it under a new launch generation, pending first like the other
vendors.

Pass `--ack` when the caller needs proof of delivery (the OpenCode plugin
does, on every report). With it the exit status is 0 only when Crowded
accepted the report, and a failure exits with its own code:

| Exit | Meaning |
| --- | --- |
| 0 | Crowded accepted the report |
| 2 | usage error, or nothing to report |
| 3 | invalid session id (rejected before contacting Crowded) |
| 4 | not configured: `CROWDED_TOKEN` is not set, or the environment is unusable |
| 5 | Crowded is unreachable, or did not answer within 4 seconds |
| 6 | Crowded rejected the report |

The same codes are in the command's usage text.

The whole exchange with Crowded (connect, write, read) is bounded to 4 seconds, with or
without `--ack`: a Crowded that accepts the request and never answers costs the
command 4 seconds, then it exits 5 under `--ack` and silently 0 without it. A
reply that arrives within the bound still counts. The bound is a constant in the
command (it is not a setting), is applied by the command itself so the other
Doorbell commands are unchanged, and works the same on the Windows pipe. It is
below the OpenCode plugin's own 5 second kill bound.

`--pending` means the room is between sessions (for example the OpenCode
plugin saw the home flip before the next prompt created a session). While
pending, new mail is still saved, but the outbox holds it with the reason
`session pending after clear` instead of sending it; nothing is pasted and
no retry can start. Whether mail counts as before or after the report is
decided by arrival order at Crowded, not by timestamps, so mail saved in
the same millisecond still lands on the right side. A later report with an
id rebinds the room and adopts that held mail onto the new session. Mail
still queued unheld when the pending command itself is processed belongs
to the ended session: it is classified as stale right there, in command
order, so a later report for that same session finds nothing to release
and it is never retargeted. That classification reaches memory before the
disk write, so the pending command fails closed: if the write fails the
command reports it as a notice naming that room and the reason, the mail
stays held and blocked from sending anyway, and the write is retried on
every later pass until it lands. That failure belongs to the pending
command alone: a message queued after it still gets its own result and is
never blamed for it. Every answer from the outbox is matched to the command
that asked for it, so no reply is ever taken by a different command. Notices
are kept as a bounded list of at most 16: the oldest is dropped and counted,
identical repeats collapse into one with a repeat count, and a failure is
never dropped to make room for a routine status. Every notice carries its
severity from the code that produced it -- routine statuses such as
`Queued delivery: 3 injected, 0 failed` never count as failures, and
failures such as `Could not teach Room 2 the house rules: ...` always do,
regardless of wording. The one-row status line
always leads with the newest failure behind a count such as `2 failures: `,
clipped with an ellipsis to the width it has, so an earlier routine status
can never hide it; when the width cannot fit a usable reason it names the
mailroom instead (such as `1 failure: see mail…`), never just a bare
count; a lone notice is shown exactly as it was produced, while a repeated
one keeps its repeat count both in the status row (such as
`1 notices: … (x3)`) and in the popup. The
mailroom view lists the notice history first with failures before routine
statuses, then the envelopes, plus how many older ones were dropped; the
popup scrolls (Up/Down, PageUp/PageDown, wheel), so every failure stays
reachable no matter how many envelopes precede it. Opening the mailroom
(F2) never clears the history; leaving it (F2 again or Esc) dismisses the
history, as do switching room (Tab), starting a whisper, and opening or
leaving settings. A pending
room has no timeout; a report
with the same id as the current binding clears the pending mark without a
generation bump and releases the held mail onto that session.

A different id rebinds only that room, with a new launch generation from
the room's allocator; every other room keeps its binding and adapter.
Mail not yet accepted for the old session stays held for explicit
disposition and is never moved to the new session; mail already accepted
by the old session is left alone and never resent; mail enqueued after
the pending signal is delivered to the new session once it is reported;
mail enqueued after the rebind goes straight to the new session.

For Codex, a reported thread id is persisted next to the minted one
(provenance `reported`, distinct from `minted`; legacy generic entries
are never trusted), so a later Crowded restart resumes it with no second
exec. A failed save holds the room with a reason naming the id, and a
malformed or unsupported state file is left byte-for-byte intact. For
OpenCode the report is trusted on the room token and moved onto the same
owned server the room already runs on (no new server, no credentials leave
the room); existence is not re-checked against the server.

No path in this feature writes to a PTY or retries an accepted message:
pending, failed, uncertain, stale and unbound mail never becomes a paste.
A report from a Claude room rebinds it like the other native rooms. The first
report of a fresh launch turns that launch's unreported placeholder into the
real session and keeps the launch's generation, so mail saved against the
placeholder still belongs to that launch; a later report is a session switch
and takes a new generation, which holds older mail as stale.

## Run rooms

Start the configured room set:

```console
crowded
```

Or start explicit guests without using the configured room list:

```console
crowded raw:claude raw:codex
```

Resume supported agent sessions from the same configuration:

```console
crowded resume
```

Run `crowded --help` for the full command list.

## Work across rooms

From inside a room, Crowded exposes `CROWDED_BIN` and `CROWDED_ROOM`. Use the
live roster instead of guessing room numbers:

```console
"$CROWDED_BIN" roster --json
```

Send a message to a room:

```console
"$CROWDED_BIN" send 2 -- 'Please review this change.'
```

Rooms that set `allow_control = true` can be restarted with a fresh or resumed
context, model, or effort:

```console
"$CROWDED_BIN" control 2 clear
"$CROWDED_BIN" control 2 resume
"$CROWDED_BIN" control 2 model gpt-5 effort high
```

`crowded pulse` is the hook-facing command that updates the room status display.

## Share plugins and tools

Declare plugins in `crowded.toml`; plugins are optional and remain pinned to
their configured Git reference until you update them:

```toml
[[plugin]]
name = "code4me-ntg"
source = "https://github.com/indie-hub/code4me-ntg.git"
adapters = true

[[mcp]]
name = "basic-memory"
command = "basic-memory"
args = ["mcp"]
```

Manage installed plugins with:

```console
crowded plugin list
crowded plugin add OWNER/REPOSITORY --ref TAG
crowded plugin preview NAME
crowded plugin enable NAME
crowded plugin disable NAME
crowded plugin update NAME
crowded plugin remove NAME
```

Declare a local stdio or remote HTTP Model Context Protocol (MCP) server once
and Crowded can expose it to the configured native clients. Manage entries with
`crowded mcp list`, `crowded mcp add`, and `crowded mcp remove`.

Preview or synchronize the project-local native files that carry shared MCPs
and room-status hooks:

```console
crowded toolbox preview
crowded toolbox sync
crowded toolbox resync
crowded toolbox remove
```

Crowded only manages project-local files such as `.claude/settings.local.json`,
`.codex/hooks.json`, and `.opencode/plugins/crowded-pulse.js`. Review Codex
project hooks with `/hooks` after synchronization.

Codex approves project hooks by content hash, so **any** change to
`.codex/hooks.json` - a Crowded `sync` or `resync`, a `remove`, or an edit by
hand - invalidates the approval the file had, and Codex then skips the
untrusted hooks silently: the pulses and the session report stop until the
file is approved again. Run `/hooks` after the first synchronization and after
every later change to that file, and approve only the entries you recognise.
Crowded never launches Codex with `--dangerously-bypass-hook-trust`, so an
unapproved hook is never forced through.

`resync` refuses to overwrite a managed file you edited by hand: it fails with
a message naming the file and leaves the file exactly as it is. Hook files are
the exception - there Crowded replaces only the entries it owns and keeps
yours. `remove` works the same way, taking away only what Crowded wrote.

## Session reporting after /clear

A human `/clear` starts a fresh CLI session that Crowded otherwise never
learns. Three emitters close that gap for native OpenCode, Codex and Claude
rooms:

- Codex rooms get a second `SessionStart` hook entry in `.codex/hooks.json`
  that runs `crowded session-report`, which reads the new thread id from the
  hook JSON on stdin. The existing `pulse` entry is untouched.
- OpenCode rooms get a Crowded-owned TUI plugin directory,
  `.opencode/plugins/crowded-tui/tui.js`, alongside the unchanged
  `crowded-pulse.js` server file. After a `/clear` the plugin sees the home
  route flip, reports `--pending`, creates a session, navigates the TUI to
  it, and reports `--session <id>`. If auto-create is off it only reports
  `--pending` and then the id of whatever session appears next.

The plugin is one small state machine; every report, create and navigation is
an effect issued by a transition and by nothing else. Its states are `idle` (no
session seen yet), `bound` (Crowded was told this session), `pending` (between
sessions) and `creating` (a clear was seen and auto-create is running). A
`creating` attempt moves through three steps held in fields of that state: it
waits for the `--pending` report to be delivered, then holds one unresolved
`session.create` call, then navigates to the result.

A session is reported only when it is on screen (`router.current()` shows it)
or is the one the plugin created and navigated to. A `session.created` for a
session that is not on screen - a subagent, another client - is ignored; the
route poll reports it when the route later shows it.

| State | Event | Next state | Effects |
| --- | --- | --- | --- |
| idle | route session X, or session.created X while X is on screen | bound X | report `--session X` |
| bound X | route session X, session.created X | bound X | none (not repeated) |
| bound X | route session Y, or session.created Y while Y is on screen | bound Y | report `--session Y` |
| any | session.created X while X is not on screen | unchanged | none |
| bound | route home | creating (pending when auto-create is off or a create call is still unresolved) | report `--pending` |
| pending | route session X, or session.created X while X is on screen | bound X | report `--session X` (also when X is the session reported before the clear) |
| creating, waiting for the pending report | that report is delivered | creating, call open | `session.create` |
| creating | route session X (no call open) | bound X | report `--session X`; the attempt is dropped |
| creating, call open | create resolves with R, route not on a session | creating, navigating | navigate to R |
| creating, call open | create resolves and the route is on session Y | bound Y | report `--session Y`; R is left unused, nothing is navigated |
| creating, call open | create fails or returns an invalid id | pending | none |
| creating, navigating to R | navigation done, or route session R | bound R | report `--session R` |
| creating, navigating to R | navigation fails | pending | none; R is never reported |
| any but disposed | dispose | disposed | kill the running child, drop the queued reports, clear every timer, unsubscribe |
| any | report child exits 0 within its bound | same | next queued report starts; after a delivered `--pending`, open the create call |
| any | report child exits non-zero or was killed | same (failed after 3 in a row) | the report is queued again after a backoff, `--pending` first; no create; one stderr line when it first reaches 3 |
| any | retry wait is over | same | the queued report starts |
| any | report child passes 5 s | same | SIGTERM, then SIGKILL after 1 s |
| any | child still alive 2 s after SIGKILL | same | one line on stderr; reports keep waiting |

A route session, a `session.created` or a created id that is not a plain
session identifier (letters, digits, `-`, `_`, `.`, at most 128 characters) is
ignored. A `session.created` for the session the plugin created and did not
show is ignored too, so an unused session never rebinds the room.

Reports are ordered by construction. At most one `crowded session-report` child
is in flight. What still has to reach Crowded is kept as at most two items, the
pending marker and the latest session id, and is sent in that order: a
`--pending` first, then the `--session`. A home flip is information that a
newer session does not replace, so a route that goes home and on to a new
session while an older report is still running sends the old report, then
`--pending`, then `--session new`; only an older session id is replaced by a
newer one, and a `--pending` repeats nothing already running. A long chain of
clears therefore leaves at most one `--pending` and one `--session` queued.

The plugin passes `--ack` on every report and counts a report as delivered
only when the child exits 0 within the five second bound, which means Crowded
accepted it. A child that runs past the bound is sent SIGTERM and, after one
second, SIGKILL, and its report is not counted as delivered, nor is one that
exits non-zero. A report that was not delivered is queued again, ahead of
anything that must follow it, and tried again after a wait: immediately twice,
then after 5 s, 15 s and 60 s, and every 60 s from then on. There is never
more than one attempt in flight and never more than one attempt a minute at the
slowest, so a Crowded that is down costs one short-lived child a minute.

The pending marker is never given up on, and nothing that follows it is sent
before it is delivered, so Crowded cannot rebind the room without having held
the mail of the interval in between. While a `--pending` report has not been
delivered the plugin does not create, navigate or report a new session. After
three failures in a row it says so once on stderr (never stdout) - "Crowded has
not accepted the session report; retrying, and nothing is created until it
does" - and keeps retrying. A session the human reaches meanwhile is reported as
soon as the pending marker lands, and a create that was waiting runs then. A
session report that keeps failing is retried the same way, and a newer session
id replaces an older one that was never delivered.

A late child can never reach Crowded after a newer report because the next
report starts only after the previous child has exited. If a child survives
SIGKILL (or the host has no working kill) the plugin cannot know it is gone, and
it deliberately stays stuck rather than start a second child that the first
could overtake: reports wait, one line is written to stderr (never stdout), and
reports resume if the child ever exits. This is a safe-over-live choice, there
is no timeout that abandons it.

A `session.created` for the plugin's own new session may arrive before
`session.create` resolves; it is not on screen at that point, so it is
ignored, and the plugin navigates to the session once the create resolves and
reports it exactly once. Unloading the plugin kills the running child, drops
the queued reports, ignores a create that resolves later and removes the route
timer and the event listener. A create that lands after the room has moved on -
the human reached another session - is discarded: the plugin neither navigates
nor reports it, and the empty session it created is left in the session list for
the human to delete. A navigation the TUI refuses is never reported, so the room
stays pending until the human reaches a session the plugin can report.

Auto-create is on by default. Set `CROWDED_TUI_AUTOCREATE=0` in the room's
environment to turn it off (there is no `crowded.toml` key for this; adding
a config key would need new RoomSpec plumbing, so the environment variable
is the switch). While a room reports itself between sessions ("session
pending"), new mail is held with that reason and delivered to the new
session once its id is reported; mail saved for an earlier session stays
held and is never retargeted.

To watch a plugin work by hand, copy the generated directory into a scratch
project, point a fake `crowded` at a log file, and run `opencode` there:

```console
mkdir /tmp/probe && cd /tmp/probe
mkdir -p .opencode/plugins && cp -r <crowded-tui/> .opencode/plugins/
cat > crowded <<'EOF'
#!/bin/sh
printf '%s\n' "$@" >> /tmp/probe/calls.log
EOF
chmod +x crowded
CROWDED_BIN=/tmp/probe/crowded opencode
```

Type a one-word prompt, wait for the reply, then type `/clear`. The log
should show `--pending` right after the home flip, then `--session <new-id>`
once the next session appears; with auto-create on, the plugin creates that
session itself about 300 ms after the flip.
