# Crowded

See [WISHLIST.md](WISHLIST.md) for the living roadmap and durable design
decisions.

The Crowded Room runs multiple terminal agents under one Ratatui roof.

## Workspace bootstrap

Run this in a clean project directory:

```console
crowded init
```

The first run creates a starter `crowded.toml`, adds `/.crowded/` to
`.gitignore`, and stops so the configuration can be reviewed. Later runs
validate the whole file, install missing declared plugins, synchronize the
native toolbox, and run pending setup actions.

Declare shared plugins and direct, one-time setup commands alongside rooms and
MCPs:

```toml
[[plugin]]
name = "code4me-ntg"
source = "https://github.com/indie-hub/code4me-ntg.git"
adapters = true
# ref = "v0.4.0"

[[plugin]]
name = "context-mode"
source = "https://github.com/mksglu/context-mode.git"
ref = "v1.0.169"
adapters = true

[[setup]]
name = "ccc-index"
command = "ccc"
args = ["index"]
```

Crowded invokes setup programs directly, so `command` may be an existing
executable, `uv`, `uvx`, or `npx`; package installation remains an explicit
setup action in the reviewed configuration.
Each successful action creates `.crowded/init/NAME.done`. Failed actions are
not marked and run again on the next `crowded init`. Existing plugins are left
at their installed revision; use `crowded plugin update` explicitly.

The starter configuration shares pinned tools for CocoIndex Code, CodeGraph,
Basic Memory, and Context Mode. It also creates one workspace-named Basic
Memory project, initializes CodeGraph, and initializes and indexes CCC. Review
the file before the second `crowded init`: CCC's local embedding dependencies
can download several GB on Linux and its first initialization asks which model
to use. The recipe installs Basic Memory and CCC once with `uv tool install`;
their MCP and setup actions then call the persistent executables directly.
CCC's environment currently constrains `mcp<2` because CCC still uses the
Python MCP SDK's 1.x FastMCP import path.
`uv` and `npx` must be on `PATH`. Crowded checks each executable immediately
before its action, allowing an earlier setup action to install a tool used by
the next one.

## Local rooms

Create `crowded.toml` in the directory where you launch Crowded:

```toml
[[rooms]]
name = "Claude"
command = "claude"
transport = "raw"
allow_control = true

[[rooms]]
name = "Codex"
command = "codex"
transport = "raw"
allow_control = true

[[rooms]]
name = "OpenCode"
command = "opencode"
transport = "raw"
allow_control = true
```

Then run:

```console
cargo run
```

Rooms inherit the launch directory (`$PWD`). Optional `args` and `cwd` fields
override command arguments and a particular room's working directory:

```toml
args = ["--continue"]
cwd = "../another-project"
```

For a plain terminal room:

```toml
[[rooms]]
name = "Terminal"
command = "/bin/zsh"
args = ["-l"]
transport = "shell"
```

`shell` rooms print whispers safely and are skipped by Shared Toolbox. `raw`
rooms are treated as agent TUIs and require a supported native adapter.

Command-line guests still work and override the configured room list while
retaining the Shared Toolbox:

```console
cargo run -- raw:claude raw:codex
```

## Live Roster

Every room can discover the current topology through the authenticated
Doorbell:

```console
"$CROWDED_BIN" roster
```

The JSON response lists each numeric room, name, guest program, transport,
live state, and whether peer control is enabled. This lets orchestration choose
from the rooms that actually exist instead of assuming a fixed vendor or room
number.

## The Conductor

A room can control another opted-in agent through Crowded's authenticated
Doorbell:

```console
"$CROWDED_BIN" control 2 clear
"$CROWDED_BIN" control 2 model gpt-5
"$CROWDED_BIN" control 2 effort high
```

`allow_control` defaults to `false`. Controls are structured events, so
ordinary whispers and terminal output cannot trigger them. All three native
CLIs support `clear` and `model`; Claude and Codex support `effort`. OpenCode
effort is rejected until it exposes a stable launch option.

This first Conductor slice restarts the target CLI. `clear` also removes known
resume arguments so the replacement starts a fresh context; model and effort
retain the room's configured continuation arguments.

## Shared Plugins

The first local plugin slice shares instruction-only skills with every agent
room. A compatible Git repository needs a top-level `skills/` directory and
one of:

- `crowded-plugin.toml`
- `.codex-plugin/plugin.json`
- `.claude-plugin/plugin.json`

Native Codex and Claude manifests make existing plugins installable without
repackaging. Skills are shared immediately; executable vendor components stay
disabled until you preview and enable them.

A minimal Crowded-native repository has this layout:

```text
crowded-plugin.toml
skills/
└── room-greeter/
    └── SKILL.md
```

`crowded-plugin.toml` contains:

```toml
name = "greetings"
version = "1.0.0"
```

Install from a local Git repository, Git URL, or GitHub `owner/repo` shorthand:

```console
cargo run -- plugin add indie-hub/greetings --ref v1.0.0
cargo run -- plugin update greetings
cargo run -- plugin list
cargo run -- plugin preview greetings
cargo run -- plugin enable greetings
cargo run -- plugin disable greetings
cargo run -- plugin remove greetings
```

Crowded records the exact Git revision under `.crowded/plugins/`, then links
each skill into `.agents/skills/` for Codex, `.claude/skills/` for Claude, and
`.opencode/skills/` for OpenCode. Existing skill paths are never replaced.
Restart the rooms after installation or update so every CLI refreshes its skill
list.

`plugin update` fetches the installed plugin's recorded source and ref, validates
the replacement before swapping it in, and preserves its enabled state and
plugin data. Pass `--ref REF` to move a pinned plugin to another Git ref.

`plugin preview` shows the exact hook commands and files an adapter would add.
`plugin enable` shares the skills, merges native hooks into
`.claude/settings.local.json` and `.codex/hooks.json`, and links supported
OpenCode plugins and commands into `.opencode/`. `plugin disable` removes all
of those owned changes from every CLI; `remove` disables them automatically.
Set `adapters = true` on a `[[plugin]]` declaration to have later `crowded init`
runs enable those adapters after the first-run configuration review. Crowded
does not translate one vendor's plugin code into another vendor's format.

## Shared Toolbox

Declare a local stdio MCP once to make it available in every configured agent
room:

```toml
[[mcp]]
name = "basic-memory"
command = "basic-memory"
args = ["mcp"]
```

Limit an MCP to particular clients when a vendor has a better native adapter:

```toml
[[mcp]]
name = "context-mode"
command = "npx"
args = ["-y", "context-mode@1.0.169"]
clients = ["claude", "codex"]

[[opencode_plugin]]
package = "context-mode@1.0.169"
```

That is how the starter config installs Context Mode: Claude and Codex use its
shared MCP plus native hooks, while OpenCode uses the npm plugin only. Avoiding
both integrations in OpenCode prevents Context Mode's duplicate-tool conflict.

Shared MCPs currently support native `claude`, `codex`, and `opencode`
commands. Crowded keeps them project-local and does not modify the guests'
global configuration.

### Native project files

Preview the project-local MCP and pulse-hook files Crowded would create or
merge:

```console
cargo run -- toolbox preview
```

Then synchronize the native files in each configured room's working directory:

```console
cargo run -- toolbox sync
```

While synchronized, Crowded lets the guests load those project files instead
of injecting MCP command-line arguments. The Room Pulse sidebar reports only
`starting`, `thinking`, `working`, `ready`, `error`, or `offline`; prompts,
commands, and tool output never enter the pulse channel.

The generated hook files are:

- `.claude/settings.local.json`
- `.codex/hooks.json`
- `.opencode/plugins/crowded-pulse.js`

Codex requires a one-time review of project hooks through `/hooks`. OpenCode
loads its local plugin automatically. The toolbox can synchronize hooks even
when `crowded.toml` has no `[[mcp]]` declarations.

Remove Crowded's managed entries with:

```console
cargo run -- toolbox remove
```

Crowded keeps ownership state in the ignored, private
`.crowded/toolbox-state.json`. JSON files may be reformatted or extended by
their native CLI; Crowded checks and removes only its own MCP entries. Codex
TOML and hook files use exact snapshots, and OpenCode JSONC files are left
untouched.
