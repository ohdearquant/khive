# khive-gate-rego

A [Rego](https://www.openpolicyagent.org/docs/latest/policy-language/) (Open Policy
Agent) backend for [`khive-gate`](https://crates.io/crates/khive-gate)'s `Gate` trait,
powered by [`regorus`](https://crates.io/crates/regorus).

This is the **reference policy backend** for khive's authorization gate
([ADR-018](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-018-authorization-gate.md)).
It is opt-in: a deployment that wants Rego-based authorization adds this crate as a
dependency and installs a `RegoGate` as the runtime gate. Consumers that do not need
Rego (e.g. the personal-local OSS binary, which runs the permissive `AllowAllGate`
default) never compile `regorus`.

## Policy contract

A policy sees the `GateRequest` as JSON on `input`:

```text
input.actor.kind          # "user" | "agent" | "lambda" | "anonymous" | ...
input.actor.id            # caller id
input.namespace           # khive namespace, as a string
input.verb                # the verb being dispatched
input.args                # raw JSON args for the verb
input.context.session_id  # optional
input.context.timestamp   # optional, RFC3339
input.context.source      # optional ("mcp", "cli", ...)
```

Policies MUST define a `decision` rule under package `khive.gate` (or a custom
entrypoint set via [`RegoGate::with_entrypoint`]). The rule must evaluate to an object
matching `khive_gate::GateDecision`'s JSON shape:

```rego
package khive.gate

import rego.v1

# Fail-closed default: deny unless a rule below explicitly allows. WITHOUT this,
# an unmatched request leaves `decision` undefined, which surfaces as a gate
# evaluation error (see "Semantics" below).
default decision := {"decision": "deny", "reason": "no rule matched"}

# Authenticated users: full access, every call audited.
decision := {
    "decision": "allow",
    "obligations": [{"kind": "audit", "tag": sprintf("verb.%s", [input.verb])}],
} if input.actor.kind == "user"

# Anonymous callers: read-only. Writes fall through to the default deny.
decision := {"decision": "allow", "obligations": []} if {
    input.actor.kind == "anonymous"
    input.verb       in {"search", "get", "list"}
}
```

Keep the `decision` rules **mutually exclusive**. Two complete-rule bodies that both
match one request but produce *different* objects are a Rego conflict, which becomes a
gate evaluation error — and gate errors fail open (see below). Branch on disjoint
conditions (here, `actor.kind`) or use an ordered `else` chain.

`obligations` map to `khive_gate::Obligation`:
`{"kind": "audit", "tag": "..."}`,
`{"kind": "rate_limit", "window_secs": N, "max": M}`, or
`{"kind": "custom", "value": <any JSON>}`. They are declarative — the runtime records
them in the audit event; enforcement (rate limiting, etc.) is out of scope for v1.

## Usage

```rust
use std::sync::Arc;
use khive_gate::{ActorRef, Gate, GateRef, GateRequest};
use khive_gate_rego::RegoGate;
use khive_types::Namespace;
use serde_json::json;

let policy = r#"
    package khive.gate
    import rego.v1
    default decision := {"decision": "deny", "reason": "default"}
    decision := {"decision": "allow", "obligations": []} if {
        input.verb == "search"
    }
"#;

let gate: GateRef = Arc::new(RegoGate::from_policy_str(policy).unwrap());
let req = GateRequest::new(ActorRef::anonymous(), Namespace::local(), "search", json!({}));
assert!(gate.check(&req).unwrap().is_allow());
```

Load every `*.rego` file under a directory (non-recursive, deterministic order) with
`RegoGate::from_dir(path)`. Install the gate on the runtime via
`RuntimeConfig { gate: Arc::new(rego_gate), .. }` / `VerbRegistryBuilder::with_gate`.

## Semantics

Per [ADR-018](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-018-authorization-gate.md):

- **`Deny` is authoritative.** A `Deny` decision aborts dispatch with
  `RuntimeError::PermissionDenied`.
- **Policy load errors surface at construction.** A syntax/parse error (or an empty
  policy directory) is returned as `Err(GateError::Policy)` from
  `RegoGate::from_policy_str` / `from_dir` — i.e. when you *install* the gate, not at
  dispatch. Handle it at boot.
- **Policy evaluation uncertainty fails closed.** When `check` cannot produce a usable
  decision — the `decision` rule is undefined (no match and no `default`), the value
  fails to serialize, or the result is not a `GateDecision` shape — the gate converts
  it to an explicit `Ok(GateDecision::Deny)` with a diagnostic reason. Only failures
  before evaluation (e.g. request serialization, `GateError::Internal`) surface as
  `Err(GateError)`, which the runtime treats as an infrastructure failure per ADR-018.
  Declaring a `default decision := {deny ...}` is still good practice so unmatched
  requests deny with a policy-authored reason instead of the generic fail-closed one.

## Where this sits

- `khive-gate` (Apache-2.0) — the public `Gate` trait, `AllowAllGate` default, and the
  `AuditEvent` contract.
- **`khive-gate-rego` (Apache-2.0)** — this crate; the OSS reference Rego backend.

## License

Apache-2.0.
