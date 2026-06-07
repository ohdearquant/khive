---
description: Inspect current brain state — list active profiles, check which profile serves a consumer kind, and read posterior values.
---

# Inspect

Use `inspect` to observe the brain pack's current state without modifying it. Start here when recall
behavior feels off, when onboarding to a new namespace, or when debugging adaptive tuning.

## Workflow

### 1. List all profiles

See every profile and its lifecycle:

```
request(ops="brain.profiles()")
```

Filter to active profiles only:

```
request(ops="brain.profiles(lifecycle=\"active\")")
```

The response includes `id`, `lifecycle`, `consumer_kind`, `total_events`, and `exploration_epoch`
for each profile.

### 2. Inspect a specific profile

Get the full record including the latest state snapshot:

```
request(ops="brain.profile(id=\"balanced-recall-v1\")")
```

The `state_snapshot` field contains the current posterior means and variances for each weight
parameter.

### 3. Check which profile serves a consumer kind

Find out which profile the brain pack would select for a given caller:

```
request(ops="brain.resolve(consumer_kind=\"recall\")")
```

Pass an actor or namespace to simulate explicit binding lookup:

```
request(ops="brain.resolve(consumer_kind=\"recall\", actor=\"agent-x\")")
```

A `NotFound` error means no profile is bound for that combination — the default wildcard binding may
be missing or the pack may need a reset.

### 4. Batch: list profiles and resolve in one call

```
request(ops="[brain.profiles(), brain.resolve(consumer_kind=\"recall\")]")
```

## Stop condition

Use the inspect skill to gather facts. When inspection reveals a problem (e.g. wrong lifecycle,
unexpected posterior state, missing binding), switch to the `tune`, `manage`, or `bind` skill to
act.
