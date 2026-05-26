---
description: Query the brain profile registry — list profiles, fetch details, and resolve which profile serves a caller context.
---

# Profiles

The brain pack ships with one built-in profile, `balanced-recall-v1`, which drives the `recall` consumer kind. Use this skill to enumerate profiles, read their state, and resolve the binding that applies for a given caller.

## Verbs covered

| Verb                                               | What it does                                         |
| -------------------------------------------------- | ---------------------------------------------------- |
| `brain.profiles(lifecycle?)`                       | List all profiles, optionally filtered by lifecycle. |
| `brain.profile(id)`                                | Full profile record with state snapshot.             |
| `brain.resolve(consumer_kind, actor?, namespace?)` | Which profile would serve this caller?               |

## Workflow

### brain.profiles — list the registry

List every profile:

```
request(ops="brain.profiles()")
```

Filter by lifecycle. Valid values: `active`, `inactive`, `archived`.

```
request(ops="brain.profiles(lifecycle=\"inactive\")")
```

Fields returned per profile: `id`, `description`, `consumer_kind`, `state_class`, `lifecycle`, `total_events`, `exploration_epoch`, `created_at`.

### brain.profile — read one profile

Required arg: `id` (the profile string identifier, not a UUID).

```
request(ops="brain.profile(id=\"balanced-recall-v1\")")
```

The response adds `state_snapshot` to the fields above. The snapshot contains current posterior values for `relevance_weight`, `salience_weight`, and `temporal_weight`.

A `NotFound` error is returned if no profile with that `id` exists.

### brain.resolve — which profile serves this context?

Required arg: `consumer_kind`.

```
request(ops="brain.resolve(consumer_kind=\"recall\")")
```

Pass optional `actor` and/or `namespace` to simulate explicit binding lookup:

```
request(ops="brain.resolve(consumer_kind=\"recall\", namespace=\"project-a\")")
```

```
request(ops="brain.resolve(consumer_kind=\"recall\", actor=\"researcher\", namespace=\"project-a\")")
```

Resolution priority: exact (actor+namespace+consumer_kind) > namespace wildcard > actor wildcard > global wildcard. If no binding matches, `NotFound` is returned — use the `bind` skill to add a binding.

## Stop condition

Use this skill to read current profile state. To change lifecycle, use the `manage` skill. To change bindings, use the `bind` skill. To adjust posteriors, use the `tune` skill.
