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
- `AuditEvent::from_check` builds the structured audit record (`actor`, `namespace`,
  `verb`, `decision`, `obligations`, `gate_impl`, `session_id`) emitted once per gate
  consultation. Its JSON projection is a stable public contract — field names don't
  change without a new ADR.
- All wire types (`ActorRef`, `GateRequest`, `GateDecision`, `Obligation`) validate
  their invariants both at construction (`try_new` / `try_*` constructors) and at
  deserialization (custom `Deserialize` via a private `TryFrom<Raw*>` shape), so a
  policy engine handing back malformed JSON fails the same way a caller building the
  struct directly would.

## Where this sits

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
