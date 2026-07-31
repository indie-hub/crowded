# Crowded

The Crowded Room runs multiple terminal agents under one Ratatui roof.

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
cargo run -- plugin list
cargo run -- plugin preview greetings
cargo run -- plugin enable greetings
cargo run -- plugin disable greetings
cargo run -- plugin remove greetings
```

Crowded records the exact Git revision under `.crowded/plugins/`, then links
each skill into `.agents/skills/` for Codex, `.claude/skills/` for Claude, and
`.opencode/skills/` for OpenCode. Existing skill paths are never replaced.
Restart the rooms after installation so every CLI refreshes its skill list.

`plugin preview` shows the exact hook commands and files an adapter would add.
`plugin enable` shares the skills, merges native hooks into
`.claude/settings.local.json` and `.codex/hooks.json`, and links supported
OpenCode plugins and commands into `.opencode/`. `plugin disable` removes all
of those owned changes from every CLI; `remove` disables them automatically.
Crowded does not translate one vendor's plugin code into another vendor's
format.

## Shared Toolbox

Declare a local stdio MCP once to make it available in every configured agent
room:

```toml
[[mcp]]
name = "basic-memory"
command = "basic-memory"
args = ["mcp"]
```

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
