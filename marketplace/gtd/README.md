# khive gtd plugin

GTD-style task lifecycle for AI agents on top of [khive-mcp](https://github.com/ohdearquant/khive).

A task is a note with `kind = "task"`. GTD state (`inbox`/`next`/`waiting`/`someday`/`active`/`done`/`cancelled`), priority (p0–p3), assignee, due date, and dependencies live in `properties`. Hybrid search and graph traversal work on tasks the same as on any other note — `recall` surfaces them, `link` connects them to entities.

## Verbs

All verbs are dispatched through the single MCP `request` tool ([ADR-020](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-020-request-dsl.md)).

| Verb | What it does |
|------|--------------|
| `assign(title, priority?, status?, assignee?, due?, depends_on?, tags?, description?)` | Create a task. Defaults to `status=inbox`, priority salience 0.5. |
| `next(limit?, assignee?)` | Actionable tasks (status in `{next, active}`), priority-sorted. |
| `complete(id, result?)` | Mark done. Records `completed_at` and validates the transition. |
| `tasks(status?, assignee?, priority?, limit?, offset?)` | Filtered listing. |
| `transition(id, status, note?)` | Explicit GTD state change with lifecycle validation. |

Statuses accept canonical names *or* aliases: `in_progress → active`, `todo → inbox`, `blocked → waiting`, `later → someday`, `finished → done`.

## Skills

- **capture** — drop ideas / commitments into `inbox` cleanly.
- **today** — review actionable work and pick what to do now.
- **review** — weekly sweep: triage inbox, defer / cancel stale items.

## Install

```
/plugin marketplace add ohdearquant/khive
/plugin install gtd
```

The plugin's MCP server starts with `KHIVE_PACKS=gtd`, so only GTD verbs are advertised. Install the
`kg` plugin alongside if you want the knowledge graph verbs in the same agent session.

## License

Apache-2.0
