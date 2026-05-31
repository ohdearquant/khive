---
description: Wire brain profiles to actors, namespaces, and consumer kinds via the binding resolution table.
---

# Bind

The brain pack resolves which profile to use for a given caller context via a binding table. Each
binding maps a (actor, namespace, consumer_kind) triple to a profile. Use `brain.bind` to add or
replace a binding and `brain.unbind` to remove one.

## How resolution works

When `brain.resolve` is called with a (actor, namespace, consumer_kind) context, it walks the
binding table from most-specific to least-specific:

1. Exact match: actor + namespace + consumer_kind
2. Wildcard actor: `*` + namespace + consumer_kind
3. Wildcard namespace: actor + `*` + consumer_kind
4. Global wildcard: `*` + `*` + consumer_kind

The first match wins. If no match is found, `NotFound` is returned.

`*` is the reserved wildcard sentinel. Real values cannot contain `*`.

## Workflow

### brain.bind — add or replace a binding

Required arg: `profile_id`. At least one of `actor`, `namespace`, or `consumer_kind` should be set;
all default to `*` (wildcard) if omitted.

Bind all callers for the `recall` consumer kind to the default profile:

```
request(ops="brain.bind(profile_id=\"balanced-recall-v1\", consumer_kind=\"recall\")")
```

Bind a specific actor to a profile:

```
request(ops="brain.bind(profile_id=\"balanced-recall-v1\", actor=\"researcher\", consumer_kind=\"recall\")")
```

Bind a namespace + consumer kind:

```
request(ops="brain.bind(profile_id=\"balanced-recall-v1\", namespace=\"project-a\", consumer_kind=\"recall\")")
```

Set `priority` to control tie-breaking when multiple bindings could match (higher number = higher
priority, default 0):

```
request(ops="brain.bind(profile_id=\"balanced-recall-v1\", namespace=\"project-a\", consumer_kind=\"recall\", priority=10)")
```

If a binding for the same (actor, namespace, consumer_kind) triple already exists, `brain.bind`
replaces it atomically.

Response:
`{ "bound": true, "profile_id": "...", "actor": "...", "namespace": "...", "consumer_kind": "..." }`.

### brain.unbind — remove bindings

At least one filter is required. Only bindings matching ALL supplied criteria are removed.

Remove all bindings for a specific actor:

```
request(ops="brain.unbind(actor=\"researcher\")")
```

Remove a specific actor + namespace + consumer_kind binding:

```
request(ops="brain.unbind(actor=\"researcher\", namespace=\"project-a\", consumer_kind=\"recall\")")
```

Remove all bindings for a profile:

```
request(ops="brain.unbind(profile_id=\"balanced-recall-v1\")")
```

Response: `{ "unbound": N }` where N is the count of removed bindings.

Calling `brain.unbind()` with no args is rejected — at least one filter must be provided.

### Verify a binding with brain.resolve

After binding, confirm resolution works as expected:

```
request(ops="brain.resolve(consumer_kind=\"recall\", actor=\"researcher\")")
```

## Anti-patterns

- **Using `*` inside a real value.** `*` is the wildcard sentinel. A value like `proj-*-team` is
  rejected.
- **Unbinding with no args.** `brain.unbind()` with no filters is rejected with an error. At least
  one of `profile_id`, `actor`, `namespace`, or `consumer_kind` must be supplied.
- **Binding to a nonexistent profile.** `brain.bind` validates the profile exists and returns
  `NotFound` if not. List profiles first with `brain.profiles()`.

## Stop condition

Binding is in place. Confirm with `brain.resolve` using the exact actor/namespace/consumer_kind
context you expect to use. If resolution returns the expected profile, the skill is done.
