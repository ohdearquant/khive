---
description: Periodic GTD review — empty the inbox, defer / cancel stale items, reopen what came back. Restore the queue's trustworthiness.
---

# Review

The GTD queue is trustworthy only if you review it. This skill is the recurring sweep: clear `inbox`, prune `someday`, finish or cancel old `active` work, and decide what's `next` for the period ahead.

Cadence: weekly is the GTD canon. For agent-driven workloads, do it at session start or at the boundary of a focus block.

## Workflow

### 1. Process the inbox

```
request(ops="tasks(status=\"inbox\", limit=50)")
```

For each item, make one decision and move it. The five legal moves from `inbox`:

| Move        | When                                                                 |
| ----------- | -------------------------------------------------------------------- |
| `next`      | You'll commit to doing it in the current period (today / this week). |
| `active`    | You're starting *right now*.                                         |
| `waiting`   | You can't do it yet — note who/what you're waiting on.               |
| `someday`   | Worth keeping but no commitment date.                                |
| `done`      | Already done (captured retroactively).                               |
| `cancelled` | No longer relevant. Be ruthless.                                     |

Batch the moves once decisions are made — the DSL is built for this:

```
request(ops="[
  transition(id=\"<id1>\", status=\"next\"),
  transition(id=\"<id2>\", status=\"someday\"),
  transition(id=\"<id3>\", status=\"cancelled\", note=\"requirement dropped\"),
  complete(id=\"<id4>\", result=\"already shipped last week\")
]")
```

### 2. Refresh `active` work

```
request(ops="tasks(status=\"active\", limit=20)")
```

If something's been `active` for more than a few days without progress, it's probably not actually in flight. Either:

- Park it: `transition(id=..., status="waiting", note="<blocker>")`
- Finish it: `complete(id=..., result="<one line>")`
- Cancel it: `transition(id=..., status="cancelled", note="<why>")`

Active should reflect *now*, not aspiration.

### 3. Unblock `waiting`

```
request(ops="tasks(status=\"waiting\", limit=20)")
```

For each, read `properties.transition_note` or `properties.description` to remember the blocker. If the blocker is resolved, transition back to `next` or `active`. If the blocker is permanent, `cancelled`.

```
request(ops="transition(id=\"<id>\", status=\"next\", note=\"unblocked: Alex approved spec\")")
```

### 4. Prune `someday`

```
request(ops="tasks(status=\"someday\", limit=50)")
```

The `someday` list will rot if you never look at it. For each, ask: "would I be sad if this never happens?"
- Yes → promote to `next` and commit, or set a `due` to make it real.
- No → `cancelled`.

### 5. Review recent `done` for follow-ups

```
request(ops="tasks(status=\"done\", limit=20)")
```

Reading recent completions often reveals follow-up work (a `result` line that ends with "...but should also do X"). Capture follow-ups with `assign` while context is fresh.

## Patterns

### Five-minute review (skipping `someday`)

```
request(ops="[
  tasks(status=\"inbox\"),
  tasks(status=\"active\"),
  tasks(status=\"waiting\")
]")
```

Process those three, ignore `someday` for the short version.

### Recall + review

If the `kg` pack is loaded, recall the past week's recorded insights before reviewing — it surfaces commitments that may not have made it into a task:

```
request(ops="search(kind=\"note\", query=\"commitment OR promise OR todo\", limit=10)")
```

Any unfulfilled commitments get added via `assign` in the same session.

### Carrying a task forward

If a task has been `next` for multiple reviews without progress, that's a signal — either:
- It's actually not next (downgrade to `someday`).
- It's blocked (move to `waiting` + describe the blocker).
- It's the wrong granularity (cancel, then capture smaller, more concrete sub-tasks).

Don't let zombies linger in `next`. They erode trust in the actionable list.

## Anti-patterns

- **Reviewing without making decisions.** If you read the inbox without transitioning, the next review will be longer. Make a call on every item.
- **Hoarding `someday`.** A list of 200 maybe-projects is no different from no list. Cull aggressively — anything you wouldn't actively pull onto `next` in the next three months is `cancelled`.
- **Never reopening `done`.** GTD's lifecycle allows `done → next` / `done → active` for a reason: if a task came back, it isn't a new task, it's the same one re-opening.
- **Batch transitioning without notes.** When deferring, killing, or unblocking, take three seconds to add a `note` — future you (and future agents reading recall) need the context.
