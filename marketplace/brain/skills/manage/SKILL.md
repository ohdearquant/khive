---
description: Control brain profile lifecycle — activate, deactivate, and archive profiles.
---

# Manage

Use `manage` to change a profile's lifecycle state. Lifecycle controls whether the brain pack
applies live posterior updates for a given profile and whether the profile can serve active
requests.

## Lifecycle states

| State      | Meaning                                                                          |
| ---------- | -------------------------------------------------------------------------------- |
| `active`   | Live update loop runs. Profile receives posterior updates from every dispatch.   |
| `inactive` | Updates stopped. Posteriors are frozen but retained. Profile can be reactivated. |
| `archived` | Read-only. Audit-retained. Terminal — no transition out is permitted.            |

## Workflow

### brain.activate — start live updates

Required arg: `profile_id`.

```
request(ops="brain.activate(profile_id=\"balanced-recall-v1\")")
```

Response: `{ "profile_id": "...", "lifecycle": "active" }`.

Use this after a profile was deactivated for maintenance or experimentation and is ready to serve
traffic again.

### brain.deactivate — pause live updates

```
request(ops="brain.deactivate(profile_id=\"balanced-recall-v1\")")
```

Response: `{ "profile_id": "...", "lifecycle": "inactive" }`.

Posteriors are frozen in their current state. Use this before performing a `brain.reset` if you want
to prevent further drift while you inspect the profile. Use `brain.activate` to resume.

### brain.archive — retire a profile

Active profiles **must be deactivated first** — the runtime rejects an `Active → Archived`
transition directly. Call `brain.deactivate` before `brain.archive`:

```
request(ops="brain.deactivate(profile_id=\"balanced-recall-v1\") | brain.archive(profile_id=$prev.profile_id)")
```

Response of the archive call: `{ "profile_id": "...", "lifecycle": "archived" }`.

Archived profiles are retained for audit purposes. They no longer receive updates. Archive when a
profile is superseded by a replacement; do not delete profiles that have accumulated event history.

## Patterns

### Deactivate → inspect → activate

```
request(ops="brain.deactivate(profile_id=\"balanced-recall-v1\")")
```

Inspect the frozen state:

```
request(ops="brain.profile(profile_id=\"balanced-recall-v1\")")
```

Reactivate:

```
request(ops="brain.activate(profile_id=\"balanced-recall-v1\")")
```

### Check lifecycle after transition

Active profiles must be deactivated before archiving. Use two sequential requests or the chain form:

```
request(ops="brain.deactivate(profile_id=\"balanced-recall-v1\") | brain.archive(profile_id=$prev.profile_id) | brain.profile(profile_id=$prev.profile_id)")
```

Verify the `lifecycle` field in the second result equals `archived`.

## Stop condition

Profile lifecycle has been updated. Confirm with `brain.profile` if needed. To adjust bindings, use
the `bind` skill. To adjust posteriors, use the `tune` skill.

## Anti-patterns

- **Archiving the only active profile.** If `balanced-recall-v1` is the sole active profile for the
  `recall` consumer kind, archiving it makes `brain.resolve(consumer_kind="recall")` return
  `NotFound`.
- **Repeated archive/activate cycles.** Archive is a terminal-intent state. Use `inactive` for
  temporary pauses.
