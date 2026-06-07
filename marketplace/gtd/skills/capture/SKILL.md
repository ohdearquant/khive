---
description: Capture a task or commitment into the GTD inbox cleanly — title, priority, optional context — without trying to plan it.
---

# Capture

You have a thought, commitment, request, or to-do that needs to leave your head. Drop it in the
inbox now; plan later.

## Principle

Capture is a one-shot action. The goal is to **stop thinking about it** and trust the system to
surface it later. Don't try to schedule, prioritize precisely, or break down — `inbox` is the right
status for anything you haven't processed.

## Workflow

### 1. Decide the title

A task title is what you'd say out loud: short, verb-first, complete enough to remember in a week.
"draft README" beats "documentation"; "ship release" beats "release stuff".

### 2. Assign with the smallest commitment

```
request(ops="gtd.assign(title=\"<title>\", priority=\"p2\")")
```

Default priority is `p2`. Use `p0`/`p1` only if you genuinely want it pushed up in `next` listings.
`p3` for "nice to have, no pressure".

### 3. Add a deadline if it's real

If there's a deadline, set it — but only if it's a real deadline, not aspirational:

```
request(ops="gtd.assign(title=\"prep slides\", priority=\"p0\", due=\"2026-06-01T10:00:00Z\")")
```

### 4. Multiple captures? Batch them.

The DSL takes a parallel batch:

```
request(ops="[
  gtd.assign(title=\"call dentist\", priority=\"p2\"),
  gtd.assign(title=\"renew passport\", priority=\"p1\", due=\"2026-08-01\"),
  gtd.assign(title=\"finish bench script\", priority=\"p1\", tags=[\"work\",\"lattice\"])
]")
```

### 5. Don't pre-plan in capture

Resist:

- Setting `status=\"active\"` because "I'll do it now" — start the task with `gtd.transition`, not
  on creation.
- Adding `depends_on` for soft preferences — only encode hard prerequisites (the dep must actually
  be done first).
- Writing the full description up front — capture the _task_, save the _plan_ for later.

## When to use other verbs instead

| Situation                                               | Verb                                                                                                                                                                   |
| ------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| "Can I act on this right now?" — actually start working | `gtd.transition(id=..., status="active")`                                                                                                                              |
| "I know I can't do this yet, blocked by X"              | `gtd.assign(..., status="waiting")` — note the blocker in the title or a `gtd.transition` note later                                                                  |
| "Maybe someday, not now"                                | `gtd.assign(..., status="someday")`                                                                                                                                    |
| Need to record several together and link dependencies   | capture the blocker first, then `gtd.assign(..., depends_on=[blocker_full_id])` for the dependent task — the property and the `depends_on` graph edge both get written |

## Examples

**Personal**:

```
request(ops="gtd.assign(title=\"book physical exam\", priority=\"p1\", due=\"2026-06-30\")")
```

**With dependency** (two-step — the second `assign` needs the first task's `full_id`):

```
# Step 1: capture the blocker
request(ops="gtd.assign(title=\"write spec\", priority=\"p1\")")
# → returns { id: "<short>", full_id: "<spec-uuid>", ... }

# Step 2: capture the dependent task referencing the blocker
request(ops="gtd.assign(title=\"implement feature\", priority=\"p2\", depends_on=[\"<spec-uuid>\"])")
```

`assign`'s `depends_on` writes both the property (`properties.depends_on`) and a `depends_on` graph
edge between the two tasks — so
`neighbors(node_id=\"<impl-uuid>\", direction=\"out\", relations=[\"depends_on\"])` will surface the
blocker.

**Quick brain-dump** (4 things at once):

```
request(ops="[
  gtd.assign(title=\"reply to Mitchell\", priority=\"p1\"),
  gtd.assign(title=\"clean kitchen\", priority=\"p3\"),
  gtd.assign(title=\"file taxes\", priority=\"p0\", due=\"2026-04-15\"),
  gtd.assign(title=\"learn Polars\", priority=\"p3\", status=\"someday\")
]")
```
