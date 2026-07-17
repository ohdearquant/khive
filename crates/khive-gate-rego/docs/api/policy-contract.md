# Rego policy input and decision contract

This document describes the Rego policy interface for `khive-gate-rego`.

## Input Shape

Policies receive `GateRequest` as JSON on `input`:

```text
input.actor.kind        # deployment-defined, non-empty actor kind
input.actor.id          # caller id
input.namespace         # khive namespace as a string
input.verb              # verb being dispatched
input.args              # raw JSON args for the verb
input.context.session_id   # optional
input.context.timestamp    # optional RFC3339
input.context.source       # optional ("mcp", "cli", ...)
```

## Decision Shape

Policies MUST define a `decision` rule under package `khive.gate` (or a custom entrypoint set
via `RegoGate::with_entrypoint` / `RegoGate::try_with_entrypoint`). The rule must produce an
object matching `GateDecision`'s JSON shape:

```rego
package khive.gate

import rego.v1

default decision := {"decision": "deny", "reason": "no rule matched"}

decision := {"decision": "allow", "obligations": []} if {
    input.actor.kind == "user"
    input.namespace  == "team-a"
}
```

## Quick Start

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
let req = GateRequest::new(
    ActorRef::anonymous(),
    Namespace::local(),
    "search",
    json!({"query": "LoRA"}),
);
assert!(gate.check(&req).unwrap().is_allow());
```

## Entrypoint Rules

- The default entrypoint is `data.khive.gate.decision`.
- Use `RegoGate::try_with_entrypoint` to override with validation (returns `Err` for empty,
  whitespace-only, or non-`data.`-prefixed paths).
- Use `RegoGate::with_entrypoint` only when the entrypoint is already validated (infallible
  builder for programmatic use; operator configuration should prefer the fallible variant).

## Evaluation failures and fail-closed behavior

Per ADR-018, a `GateError` returned from `Gate::check` is treated as a fail-open infrastructure
failure by the dispatcher. `RegoGate` therefore converts policy evaluation uncertainty into an
explicit `Ok(GateDecision::Deny)`:

- a poisoned engine mutex;
- an evaluation error or missing rule;
- an undefined result because no rule branch matched;
- a result that cannot be serialized; or
- JSON that is not a valid `GateDecision`.

Request serialization is an internal pre-evaluation failure and remains `GateError::Internal`.
Invalid custom entrypoints should still be rejected at construction through
`try_with_entrypoint`, and directory loading propagates every `ReadDir` entry error so an
incomplete policy set never installs silently.

## Sensitive policy output

A malformed policy can return caller-controlled `input.args` as its decision. Wrong-shaped output
is never included in logs or denial text: only a top-level JSON shape such as `object` or `string`
is reported. This also avoids leaking serde's unknown-variant error, which can contain the invalid
`decision` tag verbatim. The fixed log category is `policy_decision_shape_mismatch`.

## Serialized evaluation

`regorus::Engine::eval_rule` requires mutable access, so each gate protects one engine with a
mutex. Concurrent checks against the same `RegoGate` are serialized; an engine pool or compiled
policy representation would be required if policy evaluation becomes a measured contention point.
