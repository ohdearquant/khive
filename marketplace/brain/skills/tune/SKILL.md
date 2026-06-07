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
of three signals: `useful`, `not_useful`, or `wrong`.

Mark a memory as useful:

```
request(ops="brain.feedback(target_id=\"<note-uuid>\", signal=\"useful\")")
```

Mark a memory as not useful:

```
request(ops="brain.feedback(target_id=\"<note-uuid>\", signal=\"not_useful\")")
```

Mark a memory as wrong (strongest negative signal):

```
request(ops="brain.feedback(target_id=\"<note-uuid>\", signal=\"wrong\")")
```

Optional: attribute the feedback to a specific profile (useful when multiple profiles are in play):

```
request(ops="brain.feedback(target_id=\"<note-uuid>\", signal=\"useful\", served_by_profile_id=\"balanced-recall-v1\")")
```

The response includes `emitted: true`, the event ID, and the signal that was recorded.

### 2. Check current posteriors after feedback

Inspect the profile to confirm the state snapshot updated:

```
request(ops="brain.profile(id=\"balanced-recall-v1\")")
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
request(ops="[brain.feedback(target_id=\"<id-1>\", signal=\"useful\"), brain.feedback(target_id=\"<id-2>\", signal=\"not_useful\")]")
```

### Verify posterior state before and after reset

```
request(ops="brain.profile(id=\"balanced-recall-v1\")")
```

Record the `exploration_epoch`. Then reset:

```
request(ops="brain.reset()")
```

The epoch increments by 1. Run `brain.profile` again to confirm the posterior means returned to the
prior (~0.7 for relevance, ~0.2 for salience, ~0.1 for temporal with the default
`balanced-recall-v1` priors).

## Anti-patterns

- **Emitting feedback on the wrong target.** Use the `note_id` from the recall response, not an
  entity UUID.
- **Resetting during an active session.** Reset discards posteriors accumulated during the current
  session. Use it between sessions or after deliberate behavioral experiments.
- **Over-emitting wrong signal.** A single `wrong` event is sufficient; duplicate signals on the
  same target accumulate without improvement.
