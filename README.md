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
| `ACCEPTED by OpenCode` | OpenCode admitted it into its queue. This is not proof the agent read it. |
| `UNCERTAIN` | The reply was lost, so it may have been accepted. It is never resent. Crowded looks for it in OpenCode's inbox with a read-only request and settles it if it is found; until then later mail to that room waits behind it. |
| `HELD` | Saved but not sent (the request could not be made, or the mail belongs to an earlier launch of the room). It is kept and is never moved to another session. |
| `FAILED` | Rejected, for example by a wrong login (HTTP 401) or because OpenCode holds different content under the message ID. |

The doorbell answers a send with how far the message got within about a second
and a half: `accepted`, `queued` (saved, still waiting), `held`, `uncertain`, or a
rejection with the reason when it could not be saved. `queued` means saved
locally, not delivered.

A restart of Crowded gives each native room a new server and session. Mail
saved for an earlier launch stays in the log as `HELD` or `UNCERTAIN`; it is not
delivered to the new session and does not block it. A native room still cannot be
restarted, cleared, resumed or reconfigured in place, and `crowded resume` starts
a new session and says so. There is not yet a way to release or discard held mail
from inside Crowded.

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
