# Event subject history

`list(kind="event", target_id="<full UUID>")` selects events whose stored
`target_id` equals that UUID. Combine it with `event_kind="refusal"` to read
refused attempts against an existing knowledge atom:

```text
list(kind="event", event_kind="refusal", target_id="7426afd6-0234-4701-9045-83dfd39166e6")
```

The event target accepts a complete UUID, not a name or short prefix. It does not
look up an entity, note, or atom before filtering. A known atom UUID therefore
works even though the atom is not a graph entity. Other filters are intersected
with this exact predicate; omitting `target_id` keeps the existing behavior.

The request's authorized event namespace still bounds results. A target UUID
does not grant access to events in another namespace. In particular, a refusal
event records the operation in the caller namespace even when an admitted
properties-only update identified an atom in another namespace.

`target_id` selects the event's subject column. `observed` and `selected` instead
query graph-observation projections. Refusal events do not manufacture such
projections for knowledge atoms, so `observed=[atom_uuid]` is not a replacement
for this filter. The returned event retains its existing wire fields, including
`kind="refusal"` and `target_id`; the stored atom remains unchanged.

For `kind="edge"`, the existing `target_id` behavior is preserved: a full UUID,
a unique 8-or-more-character hex prefix, or an entity name. Edge prefix and name
resolution search the caller's primary namespace. Generated help and schema
descriptions distinguish these two identifier contracts.

## Event-store compatibility

The storage filter adds optional `EventFilter.target_id`; missing serialized
fields default to `None`. Query and count apply the same parameterized equality
predicate to the existing event column, alongside the namespace predicate. This
does not allocate a migration or index, and does not promise constant-time
history queries for large event stores.

Events-daemon protocol version 4 requires both client and daemon to understand
exact target filtering and the refusal kind. A version mismatch fails explicitly
instead of letting an older daemon ignore the new predicate and return unrelated
events. Upgrade/restart both peers together; embedded stores require no protocol
negotiation. Split-store reads apply the filter to both event stores before
merging their results.

The public Rust additions (`EventFilter.target_id`, `EventKind::Refusal`, and the
conditional identifier metadata mode) require downstream exhaustive matches and
struct literals to account for the new members. Release versioning follows the
closed-filter compatibility contract in ADR-022.
