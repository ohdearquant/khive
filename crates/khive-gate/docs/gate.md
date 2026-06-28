# khive-gate

**ADR:** [ADR-018 Authorization Gate](../../docs/adr/ADR-018-authorization-gate.md)

Pluggable authorization gate for verb dispatch in the khive runtime.

## Purpose

The runtime consults a `Gate` implementation before dispatching each verb.
The default `AllowAllGate` is permissive (suitable for personal/local deployments).
For production policy enforcement, plug a Rego-backed or capability-backed implementation
into `RuntimeConfig.gate`.

## Wire shapes

### `GateRequest` (input to every gate check)

| Field       | Type                | Notes                                  |
| ----------- | ------------------- | -------------------------------------- |
| `actor`     | `ActorRef`          | Caller identity (`kind` + `id`)        |
| `namespace` | `Namespace`         | Validated namespace from `khive-types` |
| `verb`      | `String`            | Verb being dispatched                  |
| `args`      | `serde_json::Value` | Verb arguments as arbitrary JSON       |
| `context`   | `GateContext`       | Optional session, timestamp, source    |

### `GateDecision` (output)

Tagged by `"decision"` field: `"allow"` or `"deny"`.

- `Allow` carries an optional `obligations` array.
- `Deny` carries a `reason` string.

### `AuditEvent` (ADR-018 audit record)

Emitted once per gate consultation. Fields include `actor`, `namespace`, `verb`,
`decision`, `deny_reason`, `obligations`, `gate_impl`, `session_id`, and `timestamp`.
Field names are a **stable public contract** — renaming requires a new ADR.

## Quick start

```rust
use std::sync::Arc;
use khive_gate::{AllowAllGate, Gate, GateRef, GateRequest, ActorRef};
use khive_types::Namespace;
use serde_json::json;

let gate: GateRef = Arc::new(AllowAllGate);
let req = GateRequest::new(
    ActorRef::anonymous(),
    Namespace::local(),
    "search",
    json!({"query": "LoRA"}),
);
assert!(gate.check(&req).unwrap().is_allow());
```

## Implementing a custom gate

```rust
use khive_gate::{Gate, GateDecision, GateError, GateRequest};

#[derive(Debug)]
struct MyGate;

impl Gate for MyGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        if req.verb == "delete" {
            Ok(GateDecision::deny("delete is not permitted"))
        } else {
            Ok(GateDecision::allow())
        }
    }

    fn impl_name(&self) -> &'static str {
        "MyGate"
    }
}
```

## Obligations

An `Allow` decision may carry obligations the dispatch layer should honour:

- `Audit { tag }` — persisted as part of the audit event when an `EventStore` is wired.
- `RateLimit { window_secs, max }` — not enforced in v0; recorded only.
- `Custom { value }` — policy-specific arbitrary JSON payload.

## Known gate implementations

| Crate             | License    | Description                               |
| ----------------- | ---------- | ----------------------------------------- |
| `khive-gate`      | Apache-2.0 | `AllowAllGate` — permissive local default |
| `khive-gate-rego` | Apache-2.0 | Rego-backed via `regorus` (ADR-018)       |

Deployments with multi-actor isolation requirements may supply a custom `Gate` implementation
behind the `Gate` trait without modifying this crate.
