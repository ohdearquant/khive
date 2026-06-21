---
description: Tune adaptive recall with brain profiles — inspect profiles and posteriors, manage lifecycle (activate/deactivate/archive/reset), emit feedback (brain.feedback / brain.auto_feedback) to shift recall weights, and wire actors to profiles via bind/unbind/resolve/bindings. Use whenever recall behavior feels off, a new tuning profile is needed, or you need to confirm which profile serves a given caller.
---

# Tune adaptive recall with brain profiles

The brain pack is the adaptive layer that adjusts how `memory.recall` ranks results. It holds
Bayesian profiles whose posteriors shift with every feedback signal. The verbs are
`brain.create_profile`, `brain.profiles`, `brain.profile`, `brain.activate`, `brain.deactivate`,
`brain.archive`, `brain.reset`, `brain.feedback`, `brain.auto_feedback`, `brain.resolve`,
`brain.bind`, `brain.unbind`, and `brain.bindings`. Per-verb param detail is one `help=true` away:
`request(ops="brain.feedback(help=true)")`.

## The pattern

### 1. Orient: which profile is active and who does it serve?

Start every session with a single batch call to see the current state:

```
request(ops="[brain.profiles(lifecycle=\"active\"), brain.resolve(consumer_kind=\"recall\")]")
```

The built-in profile is `balanced-recall-v1`. Inspect current posterior values (relevance,
salience, temporal weights) with:

```
request(ops="brain.profile(profile_id=\"balanced-recall-v1\")")
```

### 2. Feed signal back after recall

After `memory.recall` returns results, emit a signal on each result you can evaluate. The
convenience verb handles the common case:

```
request(ops="brain.auto_feedback(query=\"<the recall query>\", results=[{\"id\":\"<uuid-1>\"}])")
```

Both `query` (the recall query that produced the results) and `results` are required; the first
result's id is credited. For explicit per-item control, use `brain.feedback`. Eight signals are available: `useful`,
`not_useful`, `wrong`, `explicit_positive`, `explicit_negative`, `implicit_positive`,
`implicit_negative`, `correction`. Batch across multiple results in one call:

```
request(ops="[brain.feedback(target_id=\"<uuid-1>\", signal=\"useful\"), brain.feedback(target_id=\"<uuid-2>\", signal=\"not_useful\")]")
```

### 3. Manage lifecycle when behavior drifts

Posteriors that have drifted can be reset without losing event history:

```
request(ops="brain.reset()")
```

To pause live updates while inspecting a profile, deactivate it first:

```
request(ops="brain.deactivate(profile_id=\"balanced-recall-v1\")")
```

Then reactivate when ready:

```
request(ops="brain.activate(profile_id=\"balanced-recall-v1\")")
```

To retire a profile permanently, deactivate it before archiving — the runtime rejects a direct
`active` to `archived` transition:

```
request(ops="brain.deactivate(profile_id=\"balanced-recall-v1\") | brain.archive(profile_id=$prev.profile_id)")
```

### 4. Wire a new profile to actors or namespaces

When you need a separate tuning profile for a specific actor or context, create it (starts
`Inactive`), activate it, then bind it:

```
request(ops="brain.create_profile(name=\"research-v1\", consumer_kind=\"recall\")")
request(ops="brain.activate(profile_id=\"research-v1\")")
request(ops="brain.bind(profile_id=\"research-v1\", actor=\"researcher\", consumer_kind=\"recall\")")
```

Confirm the binding resolves correctly before relying on it:

```
request(ops="brain.resolve(consumer_kind=\"recall\", actor=\"researcher\")")
```

Resolution priority: exact (actor+namespace+consumer_kind) then namespace wildcard, actor
wildcard, and finally global wildcard. `NotFound` means no binding covers that combination.

## Anti-patterns

- **Short UUID in `brain.feedback`.** `target_id` requires the full UUID from the recall result.
  Short prefix IDs are rejected without a helpful error.
- **Archiving the only active profile.** If `balanced-recall-v1` is the sole active binding for
  `recall`, archiving it makes `brain.resolve(consumer_kind="recall")` return `NotFound` for all
  callers until a replacement is bound and activated.
- **Calling `brain.unbind()` with no args.** At least one filter (`profile_id`, `actor`,
  `namespace`, or `consumer_kind`) is required. A bare call is rejected.
- **Resetting mid-session.** `brain.reset` discards posteriors accumulated in the current session.
  Reset between sessions or after deliberate behavioral experiments, not while actively recalling.
- **Binding before activating.** A binding to an `Inactive` profile is valid but the profile will
  not receive live updates until `brain.activate` is called.
