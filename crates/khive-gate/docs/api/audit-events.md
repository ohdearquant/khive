# Gate audit events

`AuditEvent` is the stable record emitted once for each gate consultation, both to structured
tracing and, when configured, to the runtime event store.

## Stable JSON fields

| Field                        | Meaning                                                                 |
| ---------------------------- | ----------------------------------------------------------------------- |
| `timestamp`                  | UTC consultation time, RFC 3339 in JSON                                 |
| `actor`, `namespace`, `verb` | request identity and operation                                          |
| `decision`                   | lowercase `"allow"`, `"deny"`, or `"gate_unavailable"`                  |
| `deny_reason`                | present only for a denial                                               |
| `obligations`                | policy obligations on allow; always `[]` on deny or gate unavailability |
| `gate_impl`                  | backend name from `Gate::impl_name`                                     |
| `session_id`                 | request-context correlation token when present                          |
| `operation_index`            | zero-based parser position within a request group, when available       |
| `argument_origins`           | top-level literal / resolved-reference / mixed provenance               |
| `resolved_arguments`         | masked canonical digest and bounded keys for the pre-gate envelope      |
| `effective_arguments`        | masked canonical digest and bounded keys after handler canonicalization |

Field names are a public wire contract. Adding a field is compatible; removing or renaming one
requires an architectural compatibility decision. `obligations` is always serialized so non-Rust
consumers never need to distinguish absence from an empty array.

Argument values are never stored. Each identity hashes a secret-masked, recursively key-sorted JSON
projection with BLAKE3 and exposes only bounded, sorted, secret-masked top-level keys. A differing
resolved/effective digest proves that post-gate validation or a kind hook changed the handler
request without disclosing the values. `effective_arguments` is absent when no handler or
coordinator ran (deny, gate outage, or unknown verb).

## `AuditEvent::from_check`

The constructor copies actor, namespace, verb, backend name, and optional session ID from the
request, uses the request-context timestamp when supplied (stamping the current UTC time only when it is absent), and projects the decision. Allow carries its obligations and
no deny reason; deny carries its reason and an empty obligation array.

## `AuditEvent::gate_unavailable`

The constructor preserves the request identity, namespace, verb, timestamp, optional session ID,
and gate implementation while recording `decision="gate_unavailable"`, no deny reason, and an
empty obligation array. Runtime dispatch persists this envelope with `EventOutcome::Error` before
returning `RuntimeError::GateUnavailable` without invoking the operation.
