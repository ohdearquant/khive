# Rego policy input and decision contract

This document describes the Rego policy interface for `khive-gate-rego`.

## Input Shape

Policies receive `GateRequest` as JSON on `input`:

```text
input.actor.kind        # deployment-defined, non-empty actor kind
input.actor.id          # caller id
input.namespace         # khive namespace as a string
input.verb              # verb being dispatched
input.args              # resolved transport args; pre-validation/canonicalization
input.context.session_id   # optional
input.context.timestamp    # optional RFC3339
input.context.source       # optional ("mcp", "cli", ...)
```

`input.args` preserves the stable raw-envelope policy input: MCP `$prev` references have been
resolved, but handler defaults, kind hooks, and coordinator validation/canonicalization have not
run. Policies MUST NOT treat it as the authoritative effective semantic request. The supported
authorization scope is actor, namespace, and verb; semantic argument invariants are enforced at
the handler seam. See ADR-018 Amendment 4.

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

Per ADR-018 and ADR-129, a `GateError` returned from `Gate::check` is audited and refused as an
infrastructure outage by the dispatcher. `RegoGate` converts policy evaluation uncertainty into
an explicit `Ok(GateDecision::Deny)` so it remains distinguishable as a policy denial:

- a poisoned engine mutex;
- an evaluation error or missing rule;
- an undefined result because no rule branch matched;
- a result that cannot be serialized; or
- JSON that is not a valid `GateDecision`.

The evaluation-error and unserializable-result branches use a **static, classified deny
reason** — `"policy evaluation failed"` and `"policy produced an unserializable decision"`
respectively — and never interpolate the underlying `regorus` error text or echo any part of
the input that triggered it. This crate has no access to the runtime's log masker, so the raw
detail is dropped entirely rather than risk an unmasked leak on the wire or in `tracing`;
operators reproduce the failing policy/input locally to debug it. The two reasons are
deliberately distinct so the failure modes stay distinguishable to a caller.

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
