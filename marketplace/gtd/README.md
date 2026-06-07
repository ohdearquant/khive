# khive gtd plugin

GTD-style task lifecycle for AI agents on top of [khive-mcp](https://github.com/ohdearquant/khive).

A task is a note with `kind = "task"`. GTD state
(`inbox`/`next`/`waiting`/`someday`/`active`/`done`/`cancelled`), priority (p0–p3), assignee, due
date, and dependencies live in `properties`. Hybrid search and graph traversal work on tasks the
same as on any other note — `search(kind="note", ...)` surfaces them, and the `kg` pack's `link`
connects them to entities.

`done` and `cancelled` are **terminal states**. Once a task reaches either, no further `transition`
or `complete` calls are accepted. To restart abandoned work, create a new task.

## Verbs

All verbs are dispatched through the single MCP `request` tool (ADR-016).

Add `presentation="agent"` (default, compact) or `"verbose"` (full UUIDs + ISO timestamps) to any
`request` call to control response shape (ADR-045). `"human"` is accepted but over MCP is identical
to verbose JSON — readable prose is a CLI-layer concern, not an MCP-layer one.

| Verb                                                                                             | What it does                                                                                                                                                                                              |
| ------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `gtd.assign(title, priority?, status?, assignee?, due?, depends_on?, context_entity_id?, tags?)` | Create a task. Defaults: `status=inbox`, `priority=p2`. `depends_on` is an array of UUIDs that creates real `depends_on` graph edges. `context_entity_id` links the task to a KG entity at creation time. |
| `gtd.next(limit?, assignee?)`                                                                    | Actionable tasks (status in `{next, active}`), sorted by priority (p0 first) then most-recent.                                                                                                            |
| `gtd.complete(id, result?, status?)`                                                             | Mark done (or `status="cancelled"`). Records `completed_at`. Terminal — no further transitions.                                                                                                           |
| `gtd.tasks(status?, assignee?, priority?, limit?, offset?)`                                      | Filtered listing.                                                                                                                                                                                         |
| `gtd.transition(id, status, note?)`                                                              | Explicit GTD state change with lifecycle validation.                                                                                                                                                      |

Statuses accept canonical names _or_ aliases: `in_progress → active`, `todo → inbox`,
`blocked → waiting`, `later → someday`, `finished → done`.

## What's New in 0.2.3

- **`context_entity_id` on assign**: `gtd.assign` now accepts an optional `context_entity_id`
  parameter to link a task to a KG entity at creation time.
- **`complete()` error path hardened**: `gtd.complete` returns a clear error when called on a task
  already in a terminal state, instead of silently failing.

## Skills

- **capture** — drop ideas / commitments into `inbox` cleanly.
- **process** — clarify inbox items into next, waiting, someday, done, or cancelled.
- **today** — review actionable work and pick what to do now.
- **plan** — choose realistic weekly commitments and defer stale work.
- **review** — weekly sweep: triage inbox, defer / cancel stale items.

## Prerequisites

This plugin provides skills only — it does **not** bundle an MCP server. You must install the
`kkernel` binary and register it as an MCP server in your harness **before** using any of the
skills below.

```bash
# Install the binary
cargo install kkernel

# Register in your harness (Claude Code example)
claude mcp add --transport stdio khive -- kkernel mcp --pack gtd
```

Or add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "khive": {
      "command": "kkernel",
      "args": ["mcp", "--pack", "gtd"]
    }
  }
}
```

Install the `kg` pack alongside if you want knowledge graph verbs in the same session:
`"args": ["mcp", "--pack", "kg", "--pack", "gtd"]`

## Install

```bash
/plugin marketplace add ohdearquant/khive
/plugin install gtd
```

## License

Apache-2.0
