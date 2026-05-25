# khive gtd plugin

GTD-style task lifecycle for AI agents on top of [khive-mcp](https://github.com/ohdearquant/khive).

A task is a note with `kind = "task"`. GTD state (`inbox`/`next`/`waiting`/`someday`/`active`/`done`/`cancelled`), priority (p0–p3), assignee, due date, and dependencies live in `properties`. Hybrid search and graph traversal work on tasks the same as on any other note — `search(kind="note", ...)` surfaces them, and the `kg` pack's `link` connects them to entities.

## Verbs

All verbs are dispatched through the single MCP `request` tool ([ADR-020](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-020-request-dsl.md)).

| Verb                                                                                   | What it does                                                      |
| -------------------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| `assign(title, priority?, status?, assignee?, due?, start?, end?, depends_on?, tags?, description?)` | Create a task. Defaults to `status=inbox`, priority salience 0.5. |
| `next(limit?, assignee?)`                                                              | Actionable tasks (status in `{next, active}`), priority-sorted.   |
| `complete(id, result?)`                                                                | Mark done. Records `completed_at` and validates the transition.   |
| `tasks(status?, assignee?, priority?, limit?, offset?)`                                | Filtered listing.                                                 |
| `transition(id, status, note?)`                                                        | Explicit GTD state change with lifecycle validation.              |

Statuses accept canonical names _or_ aliases: `in_progress → active`, `todo → inbox`, `blocked → waiting`, `later → someday`, `finished → done`.

## Skills

- **capture** — drop ideas / commitments into `inbox` cleanly.
- **process** — clarify inbox items into next, waiting, someday, done, or cancelled.
- **today** — review actionable work and pick what to do now.
- **plan** — choose realistic weekly commitments and defer stale work.
- **review** — weekly sweep: triage inbox, defer / cancel stale items.

## Prerequisites

This plugin provides skills only — it does **not** bundle an MCP server.
You must install the `khive-mcp` binary and register it as an MCP server in your
harness **before** using any of the skills below.

```bash
# Install the binary
cargo install khive-mcp

# Register in your harness (Claude Code example)
claude mcp add --transport stdio khive -- khive-mcp --pack gtd
```

Or add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "khive": {
      "command": "khive-mcp",
      "args": ["--pack", "gtd"]
    }
  }
}
```

Install the `kg` pack alongside if you want knowledge graph verbs in the same session:
`"args": ["--pack", "kg", "--pack", "gtd"]`

## Install

```bash
/plugin marketplace add ohdearquant/khive
/plugin install gtd
```

## License

Apache-2.0
