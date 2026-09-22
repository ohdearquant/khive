# Gate audit events

`AuditEvent` is the stable record emitted once for each gate consultation, both to structured
tracing and, when configured, to the runtime event store.

## Stable JSON fields

| Field                        | Meaning                                                                  |
| ---------------------------- | ------------------------------------------------------------------------ |
| `timestamp`                  | UTC consultation time, RFC 3339 in JSON                                  |
| `actor`, `namespace`, `verb` | request identity and operation                                           |
| `decision`                   | lowercase `"allow"`, `"deny"`, or `"gate_unavailable"`                   |
| `deny_reason`                | present only for a denial                                                |
| `obligations`                | policy obligations on allow; always `[]` on deny or gate unavailability  |
| `gate_impl`                  | backend name from `Gate::impl_name`                                      |
| `session_id`                 | request-context correlation token when present                           |
| `op_index`                   | zero-based parser position within the request, or `null` when unknown    |
| `ref_resolution`             | `"literal"` or `"resolved"`, or `null` together with an unknown position |

Field names are a public wire contract. Adding a field is compatible; removing or renaming one
requires an architectural compatibility decision. `obligations` is always serialized so non-Rust
consumers never need to distinguish absence from an empty array.

## `AuditEvent::from_check`

The constructor copies actor, namespace, verb, backend name, and optional session ID from the
request, uses the request-context timestamp when supplied (stamping the current UTC time only when it is absent), and projects the decision. Allow carries its obligations and
no deny reason; deny carries its reason and an empty obligation array.

## `AuditEvent::gate_unavailable`

The constructor preserves the request identity, namespace, verb, timestamp, optional session ID,
and gate implementation while recording `decision="gate_unavailable"`, no deny reason, and an
empty obligation array. Runtime dispatch persists this envelope with `EventOutcome::Error` before
returning `RuntimeError::GateUnavailable` without invoking the operation.

## Operation provenance

The composed-request runner supplies both operation fields through a scoped context.
Single operations record index `0`. Batch operations keep their parser position even
when they finish or persist in a different order. A chain operation records `resolved`
only when an argument consumed a `$prev` reference; its first literal operation and
later operations without references record `literal`.

The same fields are persisted as event columns and exposed by event `list` and `get`.
They do not add a filter or change request-group correlation. Historical rows and
non-request producers have no proven parser position: both values are `null`, never
an invented `0`/`literal`. Deserializing an older audit envelope with absent fields
preserves that unknown state. The runtime attaches known provenance after constructing
the gate envelope; direct `from_check` and `gate_unavailable` calls default to unknown.

Events capture provenance before being queued for persistence. Nested awaited work
retains the originating operation; spawned background tasks do not inherit the scope.
Explicitly deferred transactional producers can carry an owned attribution snapshot.
Reusable token-scoped event stores do not retain an operation position between calls.
No argument values or argument digests are added by these fields.
