# khive-gate

Pluggable authorization gate trait for khive verb dispatch, with a permissive
default implementation.

The runtime consults a [`Gate`] before dispatching each verb. This crate defines the
trait, the wire types the gate sees and returns, and `AllowAllGate` — the permissive
default installed in `RuntimeConfig` when no other gate is configured.

## Usage

```rust
use khive_gate::{ActorRef, AllowAllGate, Gate, GateRequest};
use khive_types::Namespace;
use serde_json::json;

let gate = AllowAllGate;
let req = GateRequest::new(
    ActorRef::anonymous(),
    Namespace::local(),
    "search",
    json!({ "kind": "entity", "query": "LoRA" }),
);
let decision = gate.check(&req).unwrap();
assert!(decision.is_allow());
```

`GateRequest::try_new` / `ActorRef::try_new` / `GateDecision::try_deny` return
`Result` and reject empty `verb`, `actor.kind`, `actor.id`, or deny `reason` fields;
the panicking `new` / `deny` variants call the same validation and `expect()` the
result. `Obligation::rate_limit` / `try_rate_limit` validate `window_secs` and `max`
are both non-zero.

## Contract

- `Gate::check(&GateRequest) -> Result<GateDecision, GateError>` is the only method
  a backend must implement. `Gate::impl_name()` defaults to the type name and is
  surfaced in audit events so multiple gate implementations (including wrappers)
  are distinguishable without inspecting the type.
- `GateDecision::Allow { obligations }` carries zero or more `Obligation` values
  (`Audit`, `RateLimit`, `Custom`) the runtime records on dispatch. `GateDecision::Deny
  { reason }` aborts dispatch — deny is authoritative and requires a non-empty reason.
- `AuditEvent::from_check` builds the structured audit record for an explicit Allow or
  Deny decision. `AuditEvent::gate_unavailable` builds the corresponding record when
  `Gate::check` returns `GateError`; the runtime refuses dispatch without invoking the
  operation. Both carry `actor`, `namespace`, `verb`, `decision`, `obligations`,
  `gate_impl`, and `session_id`. Their JSON projection is a stable public contract —
  field names don't change without a new ADR.
- All wire types (`ActorRef`, `GateRequest`, `GateDecision`, `Obligation`) validate
  their invariants both at construction (`try_new` / `try_*` constructors) and at
  deserialization (custom `Deserialize` via a private `TryFrom<Raw*>` shape), so a
  policy engine handing back malformed JSON fails the same way a caller building the
  struct directly would.

## Built-in caller restrictions

The optional configuration table combines caller enrollment with a restriction
on user-requested domain mutations:

```toml
[gate]
granted_actors = ["service:writer", "service:duty"]
grant_unattributed = false
deny_writes_for = ["*:duty"]
```

Enrollment is required first. `deny_writes_for` never enrolls an actor: matched,
enrolled callers may execute only explicitly reviewed `Read` operations from the
[operation table](docs/api/operation-access.md). Every other operation is denied,
including unknown or unclassified mounted/plugin names, mutation aliases, `comm.read`,
`comm.mark_read`, and broad-token `authorize`. Both runtime authorization methods
check `authorize`; an `authorize.visible` read check cannot grant the primary
write-capable token. Ordinary approved dispatch still works through its concrete
verb check.

Patterns match the complete effective actor ID, case-sensitively. `*` is the only
wildcard and matches zero or more characters, including colons. Every other
character is literal, including Unicode, `?`, brackets, slash and backslash;
there is no escaping, trimming, case folding, or implicit actor hierarchy.
Each pattern must be nonblank and at most 256 UTF-8 bytes; at most 256 entries are
accepted. An anonymous caller is enrolled only by `grant_unattributed`, and its
fallback ID `local` is then subject to the same pattern restriction.

Omitting `[gate]` preserves the programmatic base gate (normally `AllowAllGate`).
An empty table still denies all callers. Omitting `deny_writes_for`, or setting
it to `[]`, preserves the existing enrollment-only behavior and fingerprint.
Nonempty restrictions fingerprint the sorted, deduplicated patterns and the
classifier version, so a warm daemon cannot reuse a different effective policy.
Invalid files fail validation; an invalid programmatic policy fails every gate
check closed. `CallerEnrollmentGate::new` remains enrollment-only; the additive
`with_write_denials` constructor installs the restriction.

This is a dispatch policy, not a storage-level read-only mode. Read handlers can
still persist normal audit, telemetry, cache, and maintenance effects. In
particular, `memory.recall` returns results and can persist `RecallExecuted`,
while its separately gated `brain.record_serve` call is denied for a restricted
caller; the recall serve ledger is therefore not populated by that call. No
internal privilege bypass is added. Already-held tokens are not revoked, and
direct storage calls using them are not rechecked. Actor IDs are resolved labels;
this setting does not authenticate a label or prevent a same-UID operator from
changing configuration or identity. `help=true` retains its existing pure
introspection path before the operation gate.

## Runtime placement

`khive-gate` sits below `khive-runtime`, which holds the `RuntimeConfig.gate: GateRef`
field consulted before every verb dispatch and defaults it to `AllowAllGate`. It has no
dependency on any other khive crate beyond `khive-types`.

- **`khive-gate` (Apache-2.0)** — this crate; the trait, wire types, and permissive default.
- [`khive-gate-rego`](https://crates.io/crates/khive-gate-rego) (Apache-2.0) — the OSS
  reference [Rego](https://www.openpolicyagent.org/) backend (`RegoGate`), installed in
  place of `AllowAllGate` when a deployment needs real policy enforcement.

Governed by [ADR-018](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-018-authorization-gate.md).

## License

Apache-2.0.
