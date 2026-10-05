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
