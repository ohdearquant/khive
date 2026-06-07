---
description: Plan a realistic GTD week - review all lists, choose commitments, defer stale work, and assign concrete follow-ups.
---

# Plan

Planning is the weekly commitment pass. It turns a processed task system into a small set of work
that should actually move this week.

Use this after `process` has cleared the inbox or when `gtd.next()` is technically correct but too
broad to guide the week.

## Workflow

### 1. Snapshot every list

Start with a full queue snapshot:

```
request(ops="[
  gtd.tasks(status=\"inbox\", limit=50),
  gtd.tasks(status=\"active\", limit=50),
  gtd.tasks(status=\"next\", limit=100),
  gtd.tasks(status=\"waiting\", limit=50),
  gtd.tasks(status=\"someday\", limit=100)
]")
```

If the inbox has more than a few items, run `process` first. Planning on an unprocessed inbox mixes
capture with commitment and produces a noisy week.

### 2. Close or park stale active work

`active` should mean in flight now. For each active task, decide whether it is still moving.

```
request(ops="gtd.tasks(status=\"active\", limit=50)")
```

Then move stale items:

```
request(ops="gtd.transition(id=\"<id>\", status=\"waiting\", note=\"weekly plan: blocked on review\")")
```

or finish them:

```
request(ops="gtd.complete(id=\"<id>\", result=\"finished before weekly planning\")")
```

### 3. Choose the week's commitments

Review current next work:

```
request(ops="gtd.tasks(status=\"next\", limit=100)")
```

Keep only the tasks you are willing to see every time you call `gtd.next()` this week. If a task is
real but not for this week, move it:

```
request(ops="gtd.transition(id=\"<id>\", status=\"someday\", note=\"weekly plan: not this week\")")
```

If a task is blocked, move it to waiting with the blocker:

```
request(ops="gtd.transition(id=\"<id>\", status=\"waiting\", note=\"weekly plan: blocked on benchmark data\")")
```

### 4. Promote the right someday items

Review deferred work:

```
request(ops="gtd.tasks(status=\"someday\", limit=100)")
```

Promote only items that have become real commitments:

```
request(ops="gtd.transition(id=\"<id>\", status=\"next\", note=\"weekly plan: committed for this week\")")
```

Cancel anything you would not choose again:

```
request(ops="gtd.transition(id=\"<id>\", status=\"cancelled\", note=\"weekly plan: no longer valuable\")")
```

### 5. Assign missing follow-ups

If the review reveals work that is not already captured, create concrete tasks immediately:

```
request(ops="gtd.assign(title=\"Write release notes for memory plugin\", priority=\"p1\", status=\"next\", tags=[\"weekly-plan\"])")
```

When delegating, set an assignee:

```
request(ops="gtd.assign(title=\"Verify KG agent queue syntax\", assignee=\"critic\", priority=\"p1\", status=\"next\", tags=[\"weekly-plan\", \"verification\"])")
```

### 6. Confirm the plan

End by checking the actionable queue:

```
request(ops="gtd.next(limit=20)")
```

The result should be small enough to scan and concrete enough to act. If it is not, continue
deferring, waiting, cancelling, or splitting tasks.

## Patterns

### Weekly review batch

```
request(ops="[
  gtd.tasks(status=\"active\", limit=25),
  gtd.tasks(status=\"next\", limit=50),
  gtd.tasks(status=\"waiting\", limit=25),
  gtd.tasks(status=\"someday\", limit=50)
]")
```

Use the results to make decisions, then send a second batch of transitions. Do not mix inspection
and decisions until you have read the lists.

### Rebalance by assignee

```
request(ops="[
  gtd.tasks(status=\"next\", assignee=\"digester\", limit=50),
  gtd.tasks(status=\"next\", assignee=\"polisher\", limit=50),
  gtd.tasks(status=\"next\", assignee=\"gap-analyst\", limit=50)
]")
```

If one assignee has too much work, assign explicit follow-ups to other agents or move lower-priority
items to `someday`.

### Create a planning trail

When the plan changes materially, include the reason in transition notes:

```
request(ops="gtd.transition(id=\"<id>\", status=\"waiting\", note=\"weekly plan: depends on schema decision\")")
```

Future review depends on those notes to distinguish blocked work from abandoned work.

## Anti-patterns

- **Planning before processing.** Clear or triage `inbox` first, otherwise captured noise becomes
  fake commitment.
- **Keeping everything in `next`.** A trusted next list is selective. Move non-commitments out.
- **Leaving stale `active` tasks untouched.** Active work that is not moving should become
  `waiting`, `next`, `someday`, `done`, or `cancelled`.
- **Assigning vague follow-ups.** New tasks created during planning should be concrete enough for
  the assignee to start.
- **No final `gtd.next()` check.** Planning is not complete until the actionable list reflects the
  plan.
