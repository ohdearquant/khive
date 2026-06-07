---
description: Adjust recall posteriors via explicit feedback and reset the balanced-recall profile when behavior is off.
---

# Tune

The brain pack adapts recall weights automatically from every verb dispatch. Use `tune` when you
want to provide explicit signal on whether a recalled memory was useful, or when the posteriors have
drifted and you want to return to the prior.

## Workflow

### 1. Emit explicit feedback on a recalled memory

After a `recall` call returns a memory, note its `note_id` from the result. Emit feedback with one
of eight signals:

| Signal | Meaning |
| --- | --- |
| `useful` | Memory was relevant and helpful |
| `not_useful` | Memory was returned but unhelpful |
| `wrong` | Memory content was incorrect (strongest negative) |
| `explicit_positive` | Strong positive signal (equivalent to `useful`) |
| `explicit_negative` | Strong negative signal (equivalent to `not_useful`) |
| `implicit_positive` | Weak positive signal inferred from agent behavior |
| `implicit_negative` | Weak negative signal inferred from agent behavior |
| `correction` | Memory was returned with errors that were corrected |

`target_id` must be the **full UUID** of the memory note — short prefix IDs are not accepted.

Mark a memory as useful:

```
request(ops="brain.feedback(target_id=\"<full-note-uuid>\", signal=\"useful\")")
```

Mark a memory as not useful:

```
request(ops="brain.feedback(target_id=\"<full-note-uuid>\", signal=\"not_useful\")")
```

Mark a memory as wrong (strongest negative signal):

```
request(ops="brain.feedback(target_id=\"<full-note-uuid>\", signal=\"wrong\")")
```

Optional: attribute the feedback to a specific profile (useful when multiple profiles are in play):

```
request(ops="brain.feedback(target_id=\"<full-note-uuid>\", signal=\"useful\", served_by_profile_id=\"balanced-recall-v1\")")
```

Optional: send per-section signals (object mapping section names to signal strings). Section signals
only accept `"useful"`, `"not_useful"`, or `"wrong"` — the top-level signal enum values do not apply here:

```
request(ops="brain.feedback(target_id=\"<full-note-uuid>\", signal=\"useful\", section_signals={relevance: \"useful\", salience: \"not_useful\"})")
```

The response includes `emitted: true`, the event ID, and the signal that was recorded.

### 2. Check current posteriors after feedback

Inspect the profile to confirm the state snapshot updated:

```
request(ops="brain.profile(profile_id=\"balanced-recall-v1\")")
```

Read the `state_snapshot.balanced_recall` field. Weight means should have shifted toward the
feedback signal.

### 3. Reset posteriors to priors

When recall quality is severely degraded or you want to start fresh after a behavioral experiment:

```
request(ops="brain.reset()")
```

The response includes `reset: true` and the new `exploration_epoch`. Event history is preserved —
only the in-memory posteriors are rolled back to the Beta prior.

Reset does not affect profile bindings or lifecycle state.

## Patterns

### Feedback loop after a recall session

```
request(ops="[brain.feedback(target_id=\"<full-uuid-1>\", signal=\"useful\"), brain.feedback(target_id=\"<full-uuid-2>\", signal=\"not_useful\")]")
```

### Verify posterior state before and after reset

```
request(ops="brain.profile(profile_id=\"balanced-recall-v1\")")
```

Record the `exploration_epoch`. Then reset:

```
request(ops="brain.reset()")
```

The epoch increments by 1. Run `brain.profile` again to confirm the posterior means returned to the
prior (~0.7 for relevance, ~0.2 for salience, ~0.1 for temporal with the default
`balanced-recall-v1` priors).

## Anti-patterns

- **Using a short UUID prefix for `target_id`.** `brain.feedback` requires the full UUID — short
  prefix IDs are rejected. Use the complete `note_id` from the recall response.
- **Resetting during an active session.** Reset discards posteriors accumulated during the current
  session. Use it between sessions or after deliberate behavioral experiments.
- **Over-emitting wrong signal.** A single `wrong` event is sufficient; duplicate signals on the
  same target accumulate without improvement.
