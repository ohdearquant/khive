# GTD Task Management

The GTD pack manages work as `task` notes. A task has a lifecycle, a priority,
and an optional assignee; it can also be connected to the rest of the knowledge
graph. Use it to capture work, decide what is ready, record work in progress,
and retain the outcome.

Calls go through `request` using the function-call DSL. The
[GTD pack rustdoc](../../crates/khive-pack-gtd/src/vocab.rs) is the reference
for verb signatures, parameters, and response shapes. For the enclosing call
format and batching rules, see the [request tool rustdoc](../../crates/khive-mcp/src/tools/request.rs).

## Lifecycle

The normal path is:

```text
inbox -> next -> active -> done
                          \
                           -> cancelled
```

`inbox` is untriaged work. Move work to `next` when it is ready to be picked
up, then to `active` when work starts. `waiting` holds work blocked by something
outside the task, and `someday` keeps work that is deliberately deferred.

The pack validates each requested transition with its lifecycle rules. A task
can move among the non-terminal states only where that rule permits it. `done`
and `cancelled` are terminal: create a new task if work needs to be reopened.

## Capture and assign work

Create a task with `gtd.assign`. It defaults to `status="inbox"` and
`priority="p2"`; `p0` is the highest priority. The optional `assignee` is an
opaque identifier used to route and filter work. Use one stable identifier for
each queue.

```text
request(ops="gtd.assign(title=\"Triage documentation feedback\", assignee=\"docs-maintainer\")")
```

The response includes the task identifier needed by later operations. A task
can also have a context entity, due date, tags, and dependencies; their
parameter details belong in the [GTD pack rustdoc](../../crates/khive-pack-gtd/src/vocab.rs).

## Choose the next task

Use `gtd.next` to read the actionable queue. It considers only `next` and
`active` tasks, sorts them by priority (`p0` first), and can filter by exact
assignee.

```text
request(ops="gtd.next(assignee=\"docs-maintainer\", limit=10)")
```

Use `gtd.tasks` when reviewing work by status, assignee, or priority. Both
`gtd.tasks` and `gtd.next` accept an assignee filter. Without a status filter,
`gtd.tasks` shows non-terminal work; pass a terminal status when reviewing
completed or cancelled tasks.

## Start and finish work

Use `gtd.transition` for an explicit lifecycle change. This chained request
creates a ready task and starts it, using the identifier returned by the first
operation:

```text
request(ops="gtd.assign(title=\"Review the task guide\", assignee=\"docs-maintainer\", status=\"next\") | gtd.transition(id=$prev.id, status=\"active\", note=\"started review\")")
```

`gtd.transition` validates the lifecycle with `can_transition` before writing.
A repeated transition to the current status is a no-op. Use `gtd.complete` to
finish an actionable task (`next` or `active`); it records `completed_at`, and
can mark the task `done` (the default) or `cancelled`.

## Dependencies

Pass task identifiers in `depends_on` when creating a task to express blockers.
The GTD pack adds a `depends_on` endpoint rule for task-to-task edges; other
note or entity types are not valid dependency targets. `gtd.next` omits a task
until every listed dependency is `done`.

## Gotchas

- An assignee is a routing field, not an automatic personal queue. Always pass
  your assignee to `gtd.next` or `gtd.tasks`; a task assigned to a different
  identifier is not work to take without coordination.
- `done` and `cancelled` cannot transition again. Capture follow-up work as a
  new task instead of trying to reopen a terminal one.
- `gtd.complete` is for actionable tasks. To finish an inbox, waiting, or
  someday task directly, use a valid `gtd.transition` to `done` or `cancelled`.

## See also

- [Knowledge Graph Modeling](knowledge-graph.md) — model the entities that a
  task concerns.
- [Prompt Cookbook](prompt-cookbook.md) — additional request DSL patterns.
