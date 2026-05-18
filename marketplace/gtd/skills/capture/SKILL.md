---
description: Capture a task or commitment into the GTD inbox cleanly — title, priority, optional context — without trying to plan it.
---

# Capture

You have a thought, commitment, request, or to-do that needs to leave your head. Drop it in the inbox now; plan later.

## Principle

Capture is a one-shot action. The goal is to **stop thinking about it** and trust the system to surface it later. Don't try to schedule, prioritize precisely, or break down — `inbox` is the right status for anything you haven't processed.

## Workflow

### 1. Decide the title

A task title is what you'd say out loud: short, verb-first, complete enough to remember in a week. "draft README" beats "documentation"; "ship release" beats "release stuff".

### 2. Assign with the smallest commitment

```
request(ops="assign(title=\"<title>\", priority=\"p2\")")
```

Default priority is `p2`. Use `p0`/`p1` only if you genuinely want it pushed up in `next` listings. `p3` for "nice to have, no pressure".

### 3. Add context if it's not in your head

If you might forget *why* this matters, add a description:

```
request(ops="assign(title=\"<title>\", priority=\"p1\", description=\"<one-sentence why>\")")
```

If there's a deadline, set it — but only if it's a real deadline, not aspirational:

```
request(ops="assign(title=\"prep slides\", priority=\"p0\", due=\"2026-06-01T10:00:00Z\")")
```

### 4. Multiple captures? Batch them.

The DSL takes a parallel batch:

```
request(ops="[
  assign(title=\"call dentist\", priority=\"p2\"),
  assign(title=\"renew passport\", priority=\"p1\", due=\"2026-08-01\"),
  assign(title=\"finish bench script\", priority=\"p1\", tags=[\"work\",\"lattice\"])
]")
```

### 5. Don't pre-plan in capture

Resist:
- Setting `status=\"active\"` because "I'll do it now" — start the task with `transition`, not on creation.
- Adding `depends_on` for soft preferences — only encode hard prerequisites (the dep must actually be done first).
- Writing the full description up front — capture the *task*, save the *plan* for later.

## When to use other verbs instead

| Situation                                                 | Verb                                                        |
| --------------------------------------------------------- | ----------------------------------------------------------- |
| "Can I act on this right now?" — actually start working   | `transition(id=..., status="active")`                       |
| "I know I can't do this yet, blocked by X"                | `assign(..., status="waiting")` + describe blocker in `description` |
| "Maybe someday, not now"                                  | `assign(..., status="someday")`                             |
| Need to record several together and link dependencies     | batch `assign` first, then a follow-up `request` with `link(... relation="depends_on")` |

## Examples

**Personal**:
```
request(ops="assign(title=\"book physical exam\", priority=\"p1\", due=\"2026-06-30\")")
```

**With dependency** (two-step):
```
# Step 1: capture both
request(ops="[
  assign(title=\"write spec\", priority=\"p1\"),
  assign(title=\"implement feature\", priority=\"p2\")
]")

# Step 2: encode the dep (replace IDs with the returned full_ids)
request(ops="link(source_id=\"<impl-uuid>\", target_id=\"<spec-uuid>\", relation=\"depends_on\", weight=1.0)")
```

**Quick brain-dump** (4 things at once):
```
request(ops="[
  assign(title=\"reply to Mitchell\", priority=\"p1\"),
  assign(title=\"clean kitchen\", priority=\"p3\"),
  assign(title=\"file taxes\", priority=\"p0\", due=\"2026-04-15\"),
  assign(title=\"learn Polars\", priority=\"p3\", status=\"someday\")
]")
```
