# khive-gate-rego Policy Contract

This document describes the Rego policy interface for `khive-gate-rego`.

## Input Shape

Policies receive `GateRequest` as JSON on `input`:

```text
input.actor.kind        # "user" | "agent" | "lambda" | "anonymous" | ...
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
    input.namespace  == "ocean"
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

## Fail-Open Behavior (ADR-018)

Per ADR-018, a `GateError` returned from `Gate::check` is treated as a fail-open infrastructure
failure by the dispatcher. To prevent unintended fail-open:

- Invalid custom entrypoints must be rejected at construction time via `try_with_entrypoint`.
- Policy directory loading propagates all `ReadDir` entry errors so incomplete policy sets are
  detected before the gate is installed.
