# ADR-054: Authorization Gate Trait

**Status**: Proposed
**Date**: 2026-06-13
**Origin**: khivedb ADR-105 (salvage)

## Context

khive enforces namespace isolation at the runtime layer (every ID-based
operation checks record.namespace == caller_namespace after fetch). This
works for embedded single-user deployments but has no abstraction for
cloud-tier authorization. Retrofitting authorization across 30 crates and
7 packs is the most expensive class of platform retrofit.

khivedb established a Gate trait with a single-dispatch-site invariant:
Gate::check is called by VerbRegistry on every dispatch, including verbs
invoked from KQL verb bodies. Pack handlers never perform authorization.

## Decision

Port the Gate abstraction into khive-types (or a new khive-gate-core crate)
and enforce the single-dispatch-site invariant in khive-runtime.

### Dispatch pipeline

```
transport (MCP / HTTP)
  -> auth       ActorStore::resolve(token) -> Caller
  -> metering   record operation for billing (cloud only)
  -> gate       Gate::check(&caller, verb, params) -> Ok | Denied
  -> dispatch   VerbRegistry executes verb handler
```

### Types (in khive-types or khive-gate-core)

```rust
pub trait Gate: Send + Sync {
    fn check(&self, caller: &Caller, verb: &str, params: &Value) -> Result<(), GateError>;
}

pub struct AllowAllGate;  // v0 embedded default
// TenantGate for cloud: namespace-to-tenant mapping, verb ACLs
```

### Invariants

1. Gate::check is called on EVERY VerbRegistry::dispatch, including nested
   verb calls from KQL verb bodies.
2. No authority elevation: nested checks run under the same Caller.
3. AllowAllGate makes nested checks free for embedded deployments.
4. Pack handlers MUST NOT perform authorization checks.

### Adaptation for khive

- khive already has khive-gate and khive-gate-rego crates. These implement
  policy engines. ADR-054 standardizes the dispatch-site invariant, not the
  policy engine.
- The existing Gate implementations become policy backends; the invariant
  ensures they are always consulted.

## Consequences

- Cloud API tier can replace AllowAllGate with TenantGate without modifying
  any pack or handler code.
- Authorization is auditable: one call site, one log point.
- ~200 LOC addition to khive-types + enforcement in VerbRegistry dispatch.
