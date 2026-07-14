# Gate audit events

`AuditEvent` is the stable record emitted once for each gate consultation, both to structured
tracing and, when configured, to the runtime event store.

## Stable JSON fields

| Field | Meaning |
| --- | --- |
| `timestamp` | UTC consultation time, RFC 3339 in JSON |
| `actor`, `namespace`, `verb` | request identity and operation |
| `decision` | lowercase `"allow"` or `"deny"` |
| `deny_reason` | present only for a denial |
| `obligations` | policy obligations on allow; always `[]` on deny |
| `gate_impl` | backend name from `Gate::impl_name` |
| `session_id` | request-context correlation token when present |

Field names are a public wire contract. Adding a field is compatible; removing or renaming one
requires an architectural compatibility decision. `obligations` is always serialized so non-Rust
consumers never need to distinguish absence from an empty array.

## `AuditEvent::from_check`

The constructor copies actor, namespace, verb, backend name, and optional session ID from the
request, stamps the current UTC time, and projects the decision. Allow carries its obligations and
no deny reason; deny carries its reason and an empty obligation array.
