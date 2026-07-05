---
description: Manage work with the GTD pack — capture tasks, process and triage the inbox, plan the week's commitments, review and unblock waiting work, and advance the queue through its lifecycle. Use whenever you create a task, check what to do next, process inbox items, do a weekly planning pass, or complete and cancel work.
---

# Manage work over GTD

khive GTD is five verbs: `gtd.assign`, `gtd.next`, `gtd.tasks`, `gtd.transition`, and
`gtd.complete`. The thing worth learning is the workflow pattern, not the individual verbs.
Per-verb param detail is one call away: `request(ops="gtd.assign(help=true)")`.

## The pattern

### 1. Capture: always set assignee at creation

```
request(ops="gtd.assign(title=\"write release notes\", priority=\"p1\", assignee=\"agent:docs\")")
```

Multiple agents write into the same namespace. Without `assignee`, a task is invisible to
`gtd.next` scoped queries and appears as noise to everyone else. Set it on every `gtd.assign`.
Default status is `inbox`, default priority is `p2`. Batch captures with the parallel DSL:

```
request(ops="[
  gtd.assign(title=\"reply to maintainer\", priority=\"p1\", assignee=\"agent:docs\"),
  gtd.assign(title=\"file quarterly report\", priority=\"p0\", due=\"2026-04-15\", assignee=\"agent:docs\")
]")
```

### 2. Process: clear the inbox before looking at next

```
request(ops="gtd.tasks(status=\"inbox\", assignee=\"agent:docs\", limit=25)")
```

For each inbox item, decide: `next` (committed, actionable), `active` (starting now),
`waiting` (blocked, name the blocker), `someday` (no commitment), `done` (already complete),
or `cancelled`. Batch the decisions once you have read the whole list:

```
request(ops="[
  gtd.transition(id=\"<id1>\", status=\"next\", note=\"clear next action\"),
  gtd.transition(id=\"<id2>\", status=\"waiting\", note=\"blocked on API review from Alex\"),
  gtd.transition(id=\"<id3>\", status=\"cancelled\", note=\"requirement dropped\"),
  gtd.transition(id=\"<id4>\", status=\"done\", note=\"already shipped last week\")
]")
```

`gtd.complete` only accepts actionable tasks (`next` / `active`). To cancel or finish an item
still in `inbox` / `waiting` / `someday`, use `gtd.transition` (`status="cancelled"` or
`status="done"`) — or move it to `next` / `active` first, then complete.

### 3. Surface actionable work

```
request(ops="gtd.next(assignee=\"agent:docs\", limit=20)")
```

`gtd.next` returns only `next` and `active` tasks, sorted by priority then recency. If it is
empty, the inbox has unprocessed items or everything is parked. Use `gtd.tasks` for broader
filtering:

```
request(ops="[
  gtd.tasks(status=\"active\", assignee=\"agent:docs\"),
  gtd.tasks(status=\"waiting\", assignee=\"agent:docs\")
]")
```

### 4. Advance: transition as reality changes

Promote to active when you start real work:

```
request(ops="gtd.transition(id=\"<id>\", status=\"active\")")
```

Park a task when it is blocked:

```
request(ops="gtd.transition(id=\"<id>\", status=\"waiting\", note=\"blocked on benchmark data\")")
```

Unblock by transitioning back:

```
request(ops="gtd.transition(id=\"<id>\", status=\"next\", note=\"unblocked: Alex approved spec\")")
```

### 5. Complete with evidence, not narrative

```
request(ops="gtd.complete(id=\"<id>\", result=\"PR #198 merged, smoke tests green\")")
```

`gtd.complete` stamps `completed_at` and validates the terminal transition. Pass a PR number,
test output, or one concrete sentence as `result`. `done` and `cancelled` are terminal: no
further transitions. Reopened work means a new `gtd.assign`, not a reopen.

### 6. Weekly planning pass

Snapshot all lists in one batch (active/next/waiting/someday), then make decisions in a
second pass. For active: park or finish anything that has not moved in days. For next: keep
only what you will touch this week, defer the rest to someday. For someday: promote real
commitments, cancel what you would not pull forward. End by checking that `gtd.next` is small
enough to scan and concrete enough to act on.

## Anti-patterns

- **Omitting `assignee` at creation.** Unowned tasks are invisible to scoped queries and
  pollute every other agent's `gtd.next`.
- **Inflating priority.** p0 means today. Most work is p1 or p2. Overuse of p0 collapses the
  signal.
- **Completing without evidence.** `result="done"` tells no one anything. Include a PR, output
  snippet, or one concrete sentence.
- **Leaving stale active tasks.** `active` means in flight now. Work that has not moved in
  days belongs in `waiting`, `next`, `someday`, or `done`.
- **Planning on an unprocessed inbox.** Process first. Mixing captured noise with commitments
  produces a plan no one trusts.
- **Reading `gtd.tasks(status="inbox")` as the working queue.** The inbox is a capture bucket.
  Run the process step before treating any list as a plan.
