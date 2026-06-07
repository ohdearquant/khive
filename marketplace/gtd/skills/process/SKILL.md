---
description: Process the GTD inbox into trusted lists - clarify each item, choose the next state, and keep only actionable work in next.
---

# Process

The inbox is a capture bucket, not a working list. Processing turns each unclarified item into a
concrete commitment, a waiting item, a someday item, or a finished/cancelled record.

Use this skill when `gtd.tasks(status="inbox")` has items or when `gtd.next()` feels noisy because
captured work has not been clarified.

## Workflow

### 1. Pull the inbox

```
request(ops="gtd.tasks(status=\"inbox\", limit=50)")
```

Read every returned task title and description. Do not start by editing priorities. First decide
what the item means and whether it is still real.

### 2. Clarify the outcome

For each inbox item, write down the concrete outcome in your head before moving it. A useful item
has a verb, an object, and enough context to act later.

If the captured title is too vague to act on, do not promote it as-is. Either move it to `someday`
with a note or cancel it and create concrete replacement tasks.

### 3. Choose exactly one state

| State       | Use when                                                        |
| ----------- | --------------------------------------------------------------- |
| `next`      | It is a real commitment and can be acted on without more input. |
| `active`    | You are starting it immediately.                                |
| `waiting`   | A named person, event, or artifact is blocking it.              |
| `someday`   | It is worth keeping but not a current commitment.               |
| `done`      | It was captured after the fact and is already complete.         |
| `cancelled` | It is no longer worth tracking.                                 |

**Transition rules**: `inbox` is a one-way entry point — no state can transition back to it.
`waiting` and `someday` cannot reach each other directly (go through `next` or `active`). `done` and
`cancelled` are **terminal** — no further transitions are accepted. To restart abandoned work,
create a new task.

```
inbox → next, waiting, someday, active, done, cancelled
next → active, waiting, someday, done, cancelled
active → next, waiting, done, cancelled
waiting → next, active, done, cancelled
someday → next, active, done, cancelled
done → (terminal)
cancelled → (terminal)
```

Move the item with `transition`:

```
request(ops="gtd.transition(id=\"<id>\", status=\"next\", note=\"processed: clear next action\")")
```

When blocked, include the blocker in the note:

```
request(ops="gtd.transition(id=\"<id>\", status=\"waiting\", note=\"blocked on API review from Alex\")")
```

### 4. Batch clear obvious items

Once decisions are made, batch independent transitions:

```
request(ops="[
  gtd.transition(id=\"<id1>\", status=\"next\", note=\"ready for implementation\"),
  gtd.transition(id=\"<id2>\", status=\"waiting\", note=\"blocked on design approval\"),
  gtd.transition(id=\"<id3>\", status=\"someday\", note=\"not a current commitment\"),
  gtd.transition(id=\"<id4>\", status=\"cancelled\", note=\"duplicate of existing work\")
]")
```

Use `gtd.complete` instead of `gtd.transition(..., status="done")` when recording a completed
result:

```
request(ops="gtd.complete(id=\"<id>\", result=\"already handled in the previous sweep\")")
```

### 5. Check the actionable list

After the inbox pass, confirm that the trusted queue is clean:

```
request(ops="gtd.next(limit=20)")
```

If `next` is empty, there is no committed actionable work. If it is too large, move low-commitment
work back to `someday` or `waiting` with a note.

## Patterns

### Process by assignee

```
request(ops="gtd.tasks(status=\"inbox\", assignee=\"lambda:khive\", limit=25)")
```

This is useful when multiple agents capture into the same store and you only own one slice.

### Promote a captured item and start it

```
request(ops="[
  gtd.transition(id=\"<id>\", status=\"next\", note=\"processed: ready\"),
  gtd.transition(id=\"<id>\", status=\"active\", note=\"starting now\")
]")
```

Only do this when you are actually beginning the work. Otherwise leave it in `next`.

### Split a vague inbox item

If one captured item really contains several actions, cancel the vague item and create concrete
replacements:

```
request(ops="[
  gtd.transition(id=\"<old-id>\", status=\"cancelled\", note=\"split into concrete tasks\"),
  gtd.assign(title=\"Draft plugin README\", priority=\"p1\", status=\"next\"),
  gtd.assign(title=\"Add plugin.json env block\", priority=\"p1\", status=\"next\")
]")
```

## Anti-patterns

- **Leaving clarified items in `inbox`.** Once you know what an item means, move it.
- **Using `next` for maybe-work.** If you are not willing to see it in `gtd.next()`, it belongs in
  `someday` or `waiting`.
- **Moving blocked work to `next`.** Capture the blocker in a `waiting` transition note.
- **Processing by priority only.** Priority is secondary; the first decision is whether the item is
  actionable, blocked, deferred, done, or cancelled.
- **Starting everything you process.** `active` is for work in flight now, not work you hope to
  start soon.
