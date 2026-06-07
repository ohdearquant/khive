# GTD Task Management

This guide covers task management in khive — the GTD lifecycle, priority levels,
task dependencies, and common workflow patterns.

## What tasks are

Tasks in khive are notes with `kind=task`, managed by the GTD pack. They have a
status lifecycle, priority level, optional assignee, and can be linked to
entities in the knowledge graph.

Tasks are created with `gtd.assign`, not `create(kind="note")`. The GTD pack
handles lifecycle validation and status transitions.

## Task lifecycle

```
inbox ──> next ──> active ──> done
  │         │        │
  │         │        └──> cancelled
  │         │
  └──> someday    waiting ◄──┐
         │                    │
         └────────────────────┘
```

### Status meanings

| Status      | Meaning                       | When to use                                           |
| ----------- | ----------------------------- | ----------------------------------------------------- |
| `inbox`     | Captured but not committed    | Default. Something that needs triage.                 |
| `next`      | Committed, ready to work on   | After triage — this is actionable and prioritized.    |
| `active`    | Currently in progress         | When you start working on it.                         |
| `done`      | Completed                     | Finished successfully.                                |
| `cancelled` | Abandoned                     | No longer relevant.                                   |
| `waiting`   | Blocked on something external | Waiting for a response, a dependency, or a condition. |
| `someday`   | Deferred indefinitely         | Not urgent, not committed, but worth remembering.     |

### Valid transitions

Not all transitions are valid. The GTD pack validates them:

- `inbox` can go to: `next`, `someday`, `cancelled`
- `next` can go to: `active`, `waiting`, `someday`, `cancelled`
- `active` can go to: `done`, `waiting`, `cancelled`
- `waiting` can go to: `next`, `active`, `cancelled`
- `someday` can go to: `next`, `cancelled`

Idempotent transitions (same status to same status) are accepted silently.

## Creating tasks

### Basic task

```
request(ops="gtd.assign(title=\"Implement FlashAttention-3 in lattice\")")
```

Defaults: `status=inbox`, `priority=p2`.

### Task with priority and status

```
request(ops="gtd.assign(title=\"Benchmark attention variants\", priority=\"p0\", status=\"next\")")
```

### Task linked to an entity

```
request(ops="gtd.assign(title=\"Review FlashAttention paper\", context_entity_id=\"<entity_id>\")")
```

The `context_entity_id` links the task to a KG entity, making it discoverable
via graph traversal.

### Task with assignee

```
request(ops="gtd.assign(title=\"Write attention tests\", assignee=\"lambda:platform\")")
```

### Task with tags

```
request(ops="gtd.assign(title=\"Profile memory usage\", tags=[\"perf\", \"attention\"])")
```

## Priority levels

| Priority | Meaning                                  |
| -------- | ---------------------------------------- |
| `p0`     | Critical — do now, everything else waits |
| `p1`     | High — do today                          |
| `p2`     | Normal — do this cycle (default)         |
| `p3`     | Low — do when convenient                 |

## Working with tasks

### Get next actions

```
request(ops="gtd.next(limit=5)")
```

Returns tasks with `status` in `[next, active]`, sorted by priority (p0 first).

### List tasks by status

```
request(ops="gtd.tasks(status=\"active\")")
request(ops="gtd.tasks(status=\"waiting\", assignee=\"lambda:khive\")")
```

### Transition a task

```
request(ops="gtd.transition(id=\"<task_id>\", status=\"active\", note=\"started implementation\")")
```

The `note` parameter records why the transition happened.

### Complete a task

```
request(ops="gtd.transition(id=\"<task_id>\", status=\"done\")")
```

You can also use `gtd.complete`:

```
request(ops="gtd.complete(id=\"<task_id>\", result=\"all benchmarks pass\")")
```

Note: `gtd.complete` requires the task to be in `active` status. Use
`gtd.transition(status="done")` if the task is in another status.

## Task dependencies

Tasks can depend on other tasks using `depends_on` edges. The GTD pack extends
the base edge contract to allow task-to-task `depends_on` relationships:

```
request(ops="gtd.assign(title=\"Write tests\", status=\"next\")")
# Get the task id from the response

request(ops="gtd.assign(title=\"Run CI\", depends_on=[\"<test_task_id>\"])")
```

Or link them explicitly:

```
request(ops="link(source_id=\"<ci_task_id>\", target_id=\"<test_task_id>\", relation=\"depends_on\")")
```

## Workflow patterns

### Daily review

```
request(ops="gtd.next(limit=10)")
```

Review actionable tasks, reprioritize, transition stale items to `waiting` or
`someday`.

### Triage inbox

```
request(ops="gtd.tasks(status=\"inbox\")")
```

For each inbox item, decide: promote to `next`, defer to `someday`, or
`cancel`.

```
request(ops="gtd.transition(id=\"<task_id>\", status=\"next\", note=\"promoted after review\")")
```

### Context switching

When picking up a new area of work, filter by assignee or tags:

```
request(ops="gtd.tasks(assignee=\"lambda:khive\", status=\"next\")")
```

### Batch status update

```
request(ops="[gtd.transition(id=\"<id1>\", status=\"done\"), gtd.transition(id=\"<id2>\", status=\"done\")]")
```

## See also

- [Prompt Cookbook](prompt-cookbook.md) — task verb patterns
- [Knowledge Graph Modeling](knowledge-graph.md) — linking tasks to entities
- [Memory and Recall](memory.md) — storing context alongside tasks
