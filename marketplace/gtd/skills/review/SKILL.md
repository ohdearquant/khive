---
description: Periodic GTD review — empty the inbox, defer / cancel stale items, reopen what came back. Restore the queue's trustworthiness.
---

# Review

The GTD queue is trustworthy only if you review it. This skill is the recurring sweep: clear
`inbox`, prune `someday`, finish or cancel old `active` work, and decide what's `next` for the
period ahead.

Cadence: weekly is the GTD canon. For agent-driven workloads, do it at session start or at the
boundary of a focus block.

## Workflow

### 1. Process the inbox

```
request(ops="gtd.tasks(status=\"inbox\", limit=50)")
```

For each item, make one decision and move it. The five legal moves from `inbox`:

| Move        | When                                                                 |
| ----------- | -------------------------------------------------------------------- |
| `next`      | You'll commit to doing it in the current period (today / this week). |
| `active`    | You're starting _right now_.                                         |
| `waiting`   | You can't do it yet — note who/what you're waiting on.               |
| `someday`   | Worth keeping but no commitment date.                                |
| `done`      | Already done (captured retroactively).                               |
| `cancelled` | No longer relevant. Be ruthless.                                     |

Batch the moves once decisions are made — the DSL is built for this:

```
request(ops="[
  gtd.transition(id=\"<id1>\", status=\"next\"),
  gtd.transition(id=\"<id2>\", status=\"someday\"),
  gtd.transition(id=\"<id3>\", status=\"cancelled\", note=\"requirement dropped\"),
  gtd.complete(id=\"<id4>\", result=\"already shipped last week\")
]")
```

### 2. Refresh `active` work

```
request(ops="gtd.tasks(status=\"active\", limit=20)")
```

If something's been `active` for more than a few days without progress, it's probably not actually
in flight. Either:

- Park it: `gtd.transition(id=..., status="waiting", note="<blocker>")`
- Finish it: `gtd.complete(id=..., result="<one line>")`
- Cancel it: `gtd.transition(id=..., status="cancelled", note="<why>")`

Active should reflect _now_, not aspiration.

### 3. Unblock `waiting`

```
request(ops="gtd.tasks(status=\"waiting\", limit=20)")
```

For each, read `properties.transition_note` or `properties.description` to remember the blocker. If
the blocker is resolved, transition back to `next` or `active`. If the blocker is permanent,
`cancelled`.

```
request(ops="gtd.transition(id=\"<id>\", status=\"next\", note=\"unblocked: Alex approved spec\")")
```

### 4. Prune `someday`

```
request(ops="gtd.tasks(status=\"someday\", limit=50)")
```

The `someday` list will rot if you never look at it. For each, ask: "would I be sad if this never
happens?"

- Yes → promote to `next` and commit, or set a `due` to make it real.
- No → `cancelled`.

### 5. Review recent `done` for follow-ups

```
request(ops="gtd.tasks(status=\"done\", limit=20)")
```

Reading recent completions often reveals follow-up work (a `result` line that ends with "...but
should also do X"). Capture follow-ups with `gtd.assign` while context is fresh. `done` is terminal
— create a new task rather than reopening.

## Patterns

### Five-minute review (skipping `someday`)

```
request(ops="[
  gtd.tasks(status=\"inbox\"),
  gtd.tasks(status=\"active\"),
  gtd.tasks(status=\"waiting\")
]")
```

Process those three, ignore `someday` for the short version.

### Review with cross-pack search

If the `kg` pack is loaded, search the past week's recorded insights before reviewing — it surfaces
commitments that may not have made it into a task:

```
request(ops="search(kind=\"note\", query=\"commitment OR promise OR todo\", limit=10)")
```

Any unfulfilled commitments get added via `gtd.assign` in the same session.

### Carrying a task forward

If a task has been `next` for multiple reviews without progress, that's a signal — either:

- It's actually not gtd.next (downgrade to `someday`).
- It's blocked (move to `waiting` + describe the blocker).
- It's the wrong granularity (cancel, then capture smaller, more concrete sub-tasks).

Don't let zombies linger in `next`. They erode trust in the actionable list.

## Anti-patterns

- **Reviewing without making decisions.** If you read the inbox without transitioning, the next
  review will be longer. Make a call on every item.
- **Hoarding `someday`.** A list of 200 maybe-projects is no different from no list. Cull
  aggressively — anything you wouldn't actively pull onto `next` in the next three months is
  `cancelled`.
- **Trying to reopen `done` tasks.** `done` and `cancelled` are terminal. If work comes back,
  capture a new task — don't attempt a transition out of a terminal state.
- **Batch transitioning without notes.** When deferring, killing, or unblocking, take three seconds
  to add a `note` — future you (and future agents searching task content) need the context.
