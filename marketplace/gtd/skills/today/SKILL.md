---
description: Pick what to work on right now — surface actionable tasks, narrow by context, advance the queue.
---

# Today

You have a long list of tasks. You don't need to see all of them — you need to know what to *do next*. This skill walks you from "queue" to "first concrete action".

## Workflow

### 1. See what's actionable

```
request(ops="next(limit=20)")
```

`next` returns only tasks whose status is `next` or `active`, sorted by priority (p0 first), then most-recent. Tasks in `inbox`, `waiting`, or `someday` are intentionally hidden — they aren't ready.

If the list is empty, you either have no committed work (process the inbox — see `review`) or every task is blocked / parked.

### 2. Narrow by context if needed

By assignee (if you collaborate with other agents):
```
request(ops="next(assignee=\"lambda:khive\", limit=10)")
```

By status — for example, "what's already in progress":
```
request(ops="tasks(status=\"active\", limit=10)")
```

By priority + status:
```
request(ops="tasks(status=\"next\", priority=\"p0\")")
```

### 3. Start one task

When you pick one to actually work on, promote it to `active` so the queue reflects reality:

```
request(ops="transition(id=\"<short-id-or-full-uuid>\", status=\"active\")")
```

Both 8-char short IDs (the `id` field) and full UUIDs (`full_id`) are accepted.

### 4. Park what isn't actually next

A task tagged `next` but you realize you can't move on it right now (waiting on someone, missing input, etc.) should leave the actionable list:

```
request(ops="transition(id=\"<id>\", status=\"waiting\", note=\"blocked on review from Alex\")")
```

### 5. Finish what you finished

```
request(ops="complete(id=\"<id>\", result=\"shipped in v0.2.1\")")
```

`complete` records `completed_at` automatically and validates the transition. You can re-open later with `transition(..., status="next")` if it turns out the work wasn't done.

## Patterns

### "Show me everything I have"

```
request(ops="[
  tasks(status=\"active\"),
  tasks(status=\"next\"),
  tasks(status=\"waiting\")
]")
```

### "What's blocking me right now"

```
request(ops="tasks(status=\"waiting\", limit=20)")
```

For each, read the `properties.transition_note` (set when you parked it) or `properties.description` to remember the blocker.

### "Pick highest-priority p0/p1 task that's actually doable"

```
request(ops="[
  tasks(status=\"next\", priority=\"p0\"),
  tasks(status=\"next\", priority=\"p1\")
]")
```

If both come back empty, you have no high-priority committed work. That's a planning signal — go to the `review` skill.

### "Recall similar past work" (cross-pack)

If the `kg` pack is also loaded, `recall` ranges over tasks and notes alike — tasks are just notes with `kind="task"`. To find prior work on the same topic:

```
request(ops="search(kind=\"note\", query=\"<short topic phrase>\", limit=5)")
```

Past completed tasks (status=done) will surface here too, useful as a "what did I do last time I worked on this".

## Anti-patterns

- **Don't grind `tasks(status=\"inbox\")` looking for what to do.** The inbox is unprocessed — process it via `review`, then `next` will have meaningful candidates.
- **Don't transition to `done` for tasks you didn't actually finish.** Use `cancelled` if you're abandoning, `waiting` if blocked. `done` is a commitment that the work is complete.
- **Don't batch dozens of transitions in one `request`.** Status changes have ordering semantics in your head; do them one or two at a time so you can react to each result.
