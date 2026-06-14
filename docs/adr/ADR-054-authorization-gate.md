# ADR-054: Authorization Gate -- ActorStore, SessionStore, and Cloud-Tier Caller Propagation

**Status**: Proposed
**Date**: 2026-06-13

## Context

ADR-018 shipped the `khive-gate` crate and the mandatory single-dispatch-site invariant.
`VerbRegistry::dispatch` (`crates/khive-runtime/src/pack.rs:657-746`) calls
`self.gate.check(&gate_req)` before every verb invocation. The gate is enforced today: `Deny`
blocks dispatch with `RuntimeError::PermissionDenied`; `AllowAllGate` is the OSS default. The
single enforcement point exists and is not optional.

What ADR-018 leaves underspecified is the **caller identity** side of the picture. The
current dispatch path builds a `GateRequest` with `ActorRef::anonymous()` on every call
(`pack.rs:671`). The shipped `Gate` trait receives a `GateRequest` carrying `actor.kind =
"anonymous"` unconditionally, which means:

1. **No token-to-caller resolution stage.** Multi-tenant deployments need to resolve an
   authenticated principal (API key, JWT, session cookie, mTLS cert) to a `(actor_id,
   namespace)` pair before the gate sees the request. Today there is no defined contract for
   that stage.
2. **No session lifecycle.** Connection-oriented transports (HTTP, WebSocket) authenticate once
   and issue a session token. Nothing in the current design specifies how sessions are created,
   resolved, or revoked.
3. **No clean cloud injection point.** A cloud `TenantGate` (behind the `Gate` trait) can
   enforce per-verb ACLs and metering, but without a resolved `ActorRef` it cannot distinguish
   tenants beyond what the `namespace` field carries.

This ADR specifies the **ActorStore and SessionStore** traits that plug into the transport
layer upstream of `VerbRegistry::dispatch`, so that by the time the gate sees a `GateRequest`
the `actor` field carries a resolved, authenticated identity rather than the anonymous sentinel.

## Decision

### Shipped types (reference, not re-specified here)

The following types are already defined and shipped. This ADR does not change them.

```rust
// crates/khive-gate/src/gate.rs:14-48
pub trait Gate: Send + Sync + std::fmt::Debug {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError>;
    fn impl_name(&self) -> &'static str { ... }
}

// crates/khive-gate/src/request.rs
pub struct GateRequest {
    pub actor:     ActorRef,
    pub namespace: Namespace,
    pub verb:      String,
    pub args:      serde_json::Value,
    pub context:   GateContext,
}

// crates/khive-gate/src/actor.rs
pub struct ActorRef {
    pub kind: String,  // "user" | "agent" | "lambda" | "anonymous" | custom
    pub id:   String,
}
```

`VerbRegistry::dispatch` constructs the `GateRequest` with `ActorRef::anonymous()` today. This
ADR adds the missing upstream stage that resolves a real `ActorRef` before that point.

### 1. ActorStore -- token-to-caller resolution

```rust
// Proposed location: crates/khive-gate/src/actor_store.rs (Apache-2.0)

/// Resolves an opaque token (API key, JWT, mTLS subject, etc.) to an ActorRef.
///
/// The embedded default returns a fixed local-user caller so that OSS single-user
/// deployments need no configuration. Cloud deployments replace this with a
/// tenant-aware implementation that validates the token against the auth store.
pub trait ActorStore: Send + Sync {
    fn resolve(&self, token: &str) -> Result<ActorRef, AuthError>;
}

/// Embedded default: always returns ActorRef::anonymous() unchanged.
///
/// Preserves the current behavior for OSS personal-local deployments.
pub struct NoopActorStore;

impl ActorStore for NoopActorStore {
    fn resolve(&self, _token: &str) -> Result<ActorRef, AuthError> {
        Ok(ActorRef::anonymous())
    }
}
```

`AuthError` is a new error type in `khive-gate`, parallel to `GateError`. It carries a reason
string and a boolean `is_permanent` so callers can distinguish auth failures (wrong key, expired
token) from transient infrastructure errors.

### 2. SessionStore -- connection-scoped identity

```rust
// Proposed location: crates/khive-gate/src/session_store.rs (Apache-2.0)

pub type SessionToken = String;

/// Manages session lifecycle for connection-oriented transports.
///
/// The embedded default is in-process and ephemeral (sessions lost on restart).
/// Cloud deployments use a durable store (Redis, Postgres) shared across replicas.
pub trait SessionStore: Send + Sync {
    fn create(&self, actor: &ActorRef) -> Result<SessionToken, SessionError>;
    fn resolve(&self, token: &SessionToken) -> Result<ActorRef, SessionError>;
    fn revoke(&self, token: &SessionToken) -> Result<(), SessionError>;
}

pub struct EphemeralSessionStore {
    sessions: std::sync::Mutex<std::collections::HashMap<SessionToken, ActorRef>>,
}
```

For MCP stdio (the current production transport), session lifecycle is implicit: one process,
one caller. `EphemeralSessionStore` is sufficient. The trait exists so the HTTP gateway (not yet
shipped) can plug in a durable session store without changing the gate surface.

### 3. Transport dispatch pipeline

The transport layer (currently `crates/khive-mcp/src/server.rs`) runs four stages before
handing off to `VerbRegistry::dispatch`:

```
transport (MCP stdio / HTTP)
  -> auth      ActorStore::resolve(token) -> ActorRef     (who is calling)
  -> session   SessionStore::resolve(token) -> ActorRef   (for connection-oriented transports)
  -> metering  record the operation for billing            (cloud tier only; no-op embedded)
  -> gate      Gate::check(&gate_req)                     (via VerbRegistry::dispatch, unchanged)
  -> dispatch  pack handler                               (record-level namespace check still applies)
```

The `GateRequest` handed to `VerbRegistry::dispatch` carries the resolved `ActorRef` instead of
`ActorRef::anonymous()`. The gate logic in ADR-018 is unchanged; the only difference is the
`actor` field now carries a real identity when the transport has resolved one.

### 4. Cloud-tier TenantGate

A cloud `TenantGate` (non-OSS, behind the Apache-2.0 `Gate` trait) uses the resolved `ActorRef`
to enforce per-verb ACLs and feed the metering stage. Because it implements the existing `Gate`
trait with its existing `check(&self, req: &GateRequest) -> Result<GateDecision, GateError>`
signature, swapping `AllowAllGate` for `TenantGate` changes no pack and no handler.

### Invariants (unchanged from ADR-018)

The ADR-018 invariants remain in force:

1. Single dispatch site. `Gate::check` is called on every `VerbRegistry::dispatch`. A new verb
   is gated automatically.
2. No authority elevation. All nested verb calls run under the same `ActorRef`.
3. Zero embedded cost. `AllowAllGate::check` compiles to a no-op.
4. Handlers never authorize. Pack handlers must not perform authorization; the dispatch site is
   the sole enforcement point.

### Crate placement

`ActorStore`, `SessionStore`, and their embedded defaults live in `khive-gate` (Apache-2.0) so
that a commercial cloud tier can implement them without a restrictively-licensed dependency. The
`khive-gate-rego` and any future `khive-gate-cloud` crates depend on `khive-gate`, not the
other way around.

## Migration path

1. Add `AuthError`, `SessionError`, `ActorStore` (with `NoopActorStore`), and `SessionStore`
   (with `EphemeralSessionStore`) to `crates/khive-gate/`.
2. Add an `actor_store: Arc<dyn ActorStore>` field to `VerbRegistryBuilder` (defaulting to
   `NoopActorStore`). The builder resolves the actor before constructing the `GateRequest`, so
   the gate consistently sees a resolved `ActorRef`.
3. MCP server startup wires `NoopActorStore` by default; embedded behavior is unchanged.
4. (Cloud tier, separate repo/crate) implement a `TenantActorStore` that validates API keys
   against the tenant store and returns a namespace-scoped `ActorRef`.

## Consequences

- The gate reliably sees a resolved caller identity on every dispatch, not always
  `ActorRef::anonymous()`.
- Cloud-tier authentication plugs in at one place (the `ActorStore`) without modifying packs
  or handlers.
- The session lifecycle contract is defined before the HTTP gateway ships, avoiding
  post-hoc retrofitting.
- Embedded OSS deployments are unchanged: `NoopActorStore` returns `ActorRef::anonymous()` as
  before.
- Approximately 150 LOC of new types and defaults in `khive-gate`, plus builder wiring in the
  runtime.

## Related ADRs

- ADR-018: Authorization Gate -- defines `Gate`, `GateRequest`, `GateDecision`, `AuditEvent`,
  and the mandatory single-dispatch-site invariant that this ADR extends.
- ADR-003: System Architecture -- gate enforcement is the agent-binary boundary.
- ADR-016: Request DSL -- dispatch path that calls `VerbRegistry::dispatch`.
- ADR-017: Pack Standard -- `VerbRegistry` is the dispatch site where the gate fires.
