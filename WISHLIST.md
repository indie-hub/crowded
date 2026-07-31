# Crowded Wishlist

This is the living roadmap for Crowded. Completed work belongs in
`CHANGELOG.md`; this file records what we still want and the decisions that
should survive chat history.

## Next: bring Code4Me into the room

Crowded hosts Code4Me; it does not embed or fork it.

### Code4Me Next Gen decision

- Build the lean kernel in its own repository,
  `https://github.com/indie-hub/code4me-ntg`; this is substantial new work, not
  a permanent branch or embedded Crowded module.
- The first kernel has no plugin dependency: one producer, one selected worker,
  one correlated result, and one append-only event log.
- Introduce integrations gradually only after that kernel is proven.

Deferred integration catalogue:

- **Basic Memory** — durable workspace-local memory shared by all rooms.
- **Ponytail** — lean implementation and output behaviour.
- **Headroom** — optional room-scoped local compression/proxy runtime.
- **CodeGraph** — preferred indexed source navigation.
- **CocoIndex Code (CCC)** — alternate structural source discovery.
- **Context Mode** — derived analysis and large non-source output handling.

Kernel integration boundary:

- Develop the integration in the new Code4Me Next Gen repository; leave the
  existing Code4Me repository intact as reference material.
- Keep task selection, task/result validation, and the append-only event log
  owned by Code4Me Next Gen.
- Let Crowded provide rooms, the Doorbell, capability tokens, and controls.
- When `CROWDED_SOCKET` is present, dispatch work through Crowded and correlate
  replies with task IDs instead of launching new Claude or Codex processes.
- In Crowded mode, choose among the rooms that actually exist instead of
  defaulting every role to a local subagent. Prefer a suitable available room
  and use vendor diversity when it strengthens the workflow; local subagents
  remain valid participants and fallbacks.
- Discover the roster dynamically. Claude, Codex, OpenCode, and future agents
  are optional room types—no particular vendor or fixed three-agent topology
  is required.
- Keep the kernel transport-neutral enough that a local subagent remains a
  valid worker without reproducing the legacy vendor bridge machinery.

The first milestone is complete when one producer can choose one local worker
or existing room, deliver one task, validate the correlated result, and record
both events in a single append-only log.

### Crowded prerequisite

Before the Code4Me adapter, add the smallest versioned Doorbell contract:

- An authenticated machine-readable roster with opaque room IDs, readiness,
  vendor, and declared capabilities.
- Authenticated task and result events correlated by `task_id`. Assignments and
  peer replies still enter rooms through the PTY; the Doorbell also retains the
  structured result so Code4Me never has to trust or scrape rendered terminal
  text.
- Bounded inline Context Packs or authenticated immutable references, with
  typed errors for oversized or unavailable content.

This is Crowded infrastructure, not a native vendor transport.

## After that

1. **Capability discovery** — expose each room's vendor, current model, exact
   model IDs, effort levels, and supported controls so agents do not guess.
2. **Base environment recipe** — populate `crowded init` with verified,
   version-pinned runners and initialization actions for Basic Memory,
   CodeGraph, CocoIndex Code, and Context Mode.
3. **Shared local memory** — configure one workspace-local Basic Memory project
   for every room. Basic Memory is the durable memory source of truth; rooms
   access it through MCP rather than editing its storage directly.
4. **Room-scoped Headroom** — optionally launch each original vendor CLI through
   its own local Headroom wrapper/proxy for isolation and per-room metrics.
   Headroom's compression cache and retrieval state are transient; they do not
   replace Basic Memory or the PTY transport.
5. **Windows portability audit** — isolate the Unix assumptions before the UI
   grows around them.
6. **Front Office** — a TUI for rooms, local MCPs, hooks, plugins, and
   permissions.
7. **Plugin lifecycle** — marketplace discovery, updates, version pinning, and
   rollback.
8. **Real sandboxing** — isolated worktrees, write leases, resource limits, and
   per-room capabilities.
9. **Optional native integrations** — keep PTY as Crowded's primary,
   vendor-neutral communication transport. Vendor APIs, hooks, ACP, Codex
   app-server, or Claude Remote Control may add capabilities when useful, but
   must not replace or bypass the room.
10. **Higher orchestration** — task graphs, explicit handoffs, completion
   signals, and room histories.
11. **TUI polish** — dynamic rooms and layouts, scrollback, searchable mailroom,
   and a richer Conductor sidebar.

## Windows checklist

- ConPTY support through `portable-pty`.
- Named pipes or loopback transport instead of Unix sockets.
- Windows-safe plugin links and executable discovery.
- PowerShell and `cmd.exe` whisper quoting.
- Correct path, permission, process cleanup, and signal behavior.
- Windows CI compilation and a small runtime smoke test.

## Foundations already shipped

- Arbitrary tiled Claude, Codex, OpenCode, and terminal rooms.
- Structured whispers, role hats, queues, readiness gates, and roster context.
- Shared local MCP configuration.
- Local hooks and the Room Pulse sidebar.
- Shared plugins, skills, hooks, and OpenCode components, including
  preview/enable/disable/remove.
- Idempotent `crowded init` with declarative plugins, direct one-time setup
  actions, native toolbox sync, and ignored machine-local state.
- Authenticated Conductor controls for clear, model, and effort where supported.
