# ADR-054: Authorization Gate -- Single-Dispatch-Site Invariant

**Status**: Proposed
**Date**: 2026-06-13

## Context

khive enforces tenancy at the runtime layer. Every ID-based operation (get, update, delete,
merge) fetches a record by UUID and then checks `record.namespace == caller_namespace`; storage
stores are ID-only, and the runtime is the trust boundary
([CLAUDE.md](../../CLAUDE.md) "Namespace isolation"). This is **record-level** isolation: it
answers "does this caller own the record it named?"

khive also already ships `khive-gate` and `khive-gate-rego` -- policy-engine crates capable of
expressing access rules. What khive does **not** have is a single, mandatory place where
authorization runs. The record-level namespace check is scattered across runtime methods, which
has three costs:

1. **No verb-level / pre-dispatch authorization.** "May this caller invoke this verb at all?"
   and "may this tenant reach this namespace?" have no enforcement point. The namespace check is
   post-fetch and per-method.
2. **Easy to omit.** Every new ID-based method must remember to call the namespace check. A
   forgotten check is a silent tenancy leak; nothing structurally guarantees coverage.
3. **No clean cloud-tier injection point.** A multi-tenant deployment needs to map an
   authenticated principal to a namespace, enforce per-verb ACLs, and meter usage. Retrofitting
   that across every pack handler is the most expensive class of platform change.

This ADR establishes a **single-dispatch-site authorization invariant**: one `Gate::check` call
on the `VerbRegistry` dispatch path, consulted on every verb invocation including nested calls
from KQL verb bodies ([ADR-052](ADR-052-khiveql-integration.md)). It standardizes _where_
authorization runs and _who the caller is_; the existing gate crates become the policy _backends_
behind it.

## Decision

### 1. Core types (Apache-2.0 core crate)

`Gate`, `Caller`, `ActorStore`, and `SessionStore` live in the Apache-2.0 storage/runtime core
(`khive-types` or a small `khive-gate-core` crate) so that a commercial cloud tier can implement
a tenant gate **without** taking a restrictively-licensed dependency. The trait surface is the
contract; implementations live wherever their license dictates.

```rust
/// The authenticated principal for one dispatch.
pub struct Caller {
    pub actor_id: String,
    pub namespace: Namespace,
}

/// Authorization decision point. One method, called once per dispatch.
pub trait Gate: Send + Sync {
    fn check(&self, caller: &Caller, verb: &str, params: &Value) -> Result<(), GateError>;
}

/// Embedded default: every call is allowed. Inlined to a no-op.
pub struct AllowAllGate;
impl Gate for AllowAllGate {
    fn check(&self, _c: &Caller, _v: &str, _p: &Value) -> Result<(), GateError> { Ok(()) }
}

/// Token -> Caller resolution (auth stage). Embedded default resolves to a fixed local caller.
pub trait ActorStore: Send + Sync {
    fn resolve(&self, token: &str) -> Result<Caller, AuthError>;
}
pub struct NoopActorStore;   // embedded: always the local namespace caller

/// Session lifecycle for connection-oriented transports. Embedded default is in-memory.
pub trait SessionStore: Send + Sync {
    fn create(&self, caller: &Caller) -> Result<SessionToken, SessionError>;
    fn resolve(&self, token: &SessionToken) -> Result<Caller, SessionError>;
    fn revoke(&self, token: &SessionToken) -> Result<(), SessionError>;
}
pub struct EphemeralSessionStore;  // embedded: in-process map
```

### 2. Dispatch pipeline

```
transport (MCP stdio / HTTP)
  -> auth       ActorStore::resolve(token) -> Caller        (who is calling)
  -> metering   record the operation for billing            (cloud tier only; no-op embedded)
  -> gate       Gate::check(&caller, verb, params)          (may they call this verb / reach this ns)
  -> dispatch   VerbRegistry runs the verb handler          (record-level namespace check still applies)
```

The gate runs **before** dispatch and operates on `(caller, verb, params)` -- it does not see the
fetched record. Record-level namespace isolation (the existing post-fetch
`record.namespace == caller.namespace` check) **remains** inside the runtime operations. The two
are complementary, not redundant:

- **Gate (pre-dispatch):** verb-level authority and tenant->namespace mapping. "May this caller
  invoke `delete`? May this tenant act in this namespace at all?"
- **Runtime namespace check (post-fetch):** record ownership. "The record this UUID resolves to
  is in the caller's namespace."

Neither subsumes the other: the gate cannot inspect a record it has not fetched, and the
post-fetch check cannot block a verb the caller should never call.

### 3. Invariants

1. **Single dispatch site.** `Gate::check` is called on every `VerbRegistry::dispatch`, including
   verbs invoked from inside KQL verb bodies. There is exactly one call site; a new verb is gated
   automatically with no extra code.
2. **No authority elevation.** Nested verb calls run under the **same** `Caller`. A verb body that
   calls another verb cannot escalate privilege; there is no sudo path.
3. **Zero embedded cost.** `AllowAllGate::check` is a no-op the compiler inlines away, so embedded
   single-user deployments pay nothing for the invariant.
4. **Handlers never authorize.** Pack handlers MUST NOT perform authorization. Authorization is
   the dispatch site's single responsibility, which is what makes it auditable -- one call site,
   one log point.

### 4. Cloud-tier mapping

A cloud `TenantGate` (in the commercial tier, behind the Apache-2.0 `Gate` trait) maps an
authenticated principal to a namespace via an **injective** `namespace -> tenant` relation
established at the cloud boundary, enforces per-verb ACLs, and feeds the metering stage. Because
it implements the core `Gate` trait, swapping `AllowAllGate` for `TenantGate` changes no pack and
no handler.

### Adaptation for khive

- `khive-gate` and `khive-gate-rego` become **policy backends**: a `Gate` impl delegates
  `check` to the Rego policy engine. This ADR does not replace them -- it gives them a single,
  guaranteed consultation point. Today a policy engine that nothing is required to call is
  advisory; under the invariant it is mandatory.
- The existing record-level namespace checks stay. Once the gate is proven in production, an
  audit can decide per-verb whether any post-fetch check is fully subsumed by a tenant gate, but
  the default is defense-in-depth: keep both.
- `Namespace` is the existing khive type; `Caller` wraps it with an `actor_id` so logs and ACLs
  can distinguish principals within a namespace.

## Migration path

1. Add the four traits + embedded defaults (`AllowAllGate`, `NoopActorStore`,
   `EphemeralSessionStore`) to the Apache-2.0 core.
2. Thread a `Caller` through `VerbRegistry::dispatch` and add the single `Gate::check` call site;
   default-wire `AllowAllGate` so embedded behavior is unchanged.
3. Add the auth + metering stages to the MCP transport (no-op metering embedded).
4. Provide a `Gate` adapter over `khive-gate-rego` so policy authoring routes through the
   invariant.
5. (Cloud tier, separate repo/crate) implement `TenantGate` + a real `ActorStore`/`SessionStore`
   over the same traits.

## Consequences

- Authorization has exactly one enforcement point; a new verb cannot be shipped unguarded.
- The cloud tier replaces `AllowAllGate` with `TenantGate` without touching any pack or handler.
- Authorization is auditable: one call site, one structured log point per dispatch.
- Embedded deployments pay zero runtime cost (no-op gate inlined).
- The Apache-2.0 trait placement lets a differently-licensed cloud tier implement tenancy
  without a license-incompatible dependency.
- ~200 LOC of traits + defaults, plus the single dispatch-site wiring in `VerbRegistry`. Existing
  `khive-gate`/`khive-gate-rego` are reused, not rewritten.
