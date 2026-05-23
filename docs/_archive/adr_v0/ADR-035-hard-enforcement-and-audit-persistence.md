# ADR-035: Hard Authorization Enforcement and Audit Persistence

**Status**: accepted\
**Date**: 2026-05-19\
**Authors**: Ocean, lambda:khive

## Context

[ADR-029](ADR-029-authorization-gate.md) introduced the pluggable `Gate` trait with the default
`AllowAllGate`. It explicitly deferred hard enforcement to v0.3:

> "In v0.2 the gate is **advisory** — the dispatcher logs deny reasons but does not yet block.
> v0.3 makes the gate authoritative (deny → error)."

ADR-029 §"Implementation Status": "Hard enforcement (deny → dispatch error) — deferred to v0.3".

[ADR-033](ADR-033-audit-envelope.md) introduced the `AuditEvent` type and wired emission via
`tracing::info!` at the single dispatch site. It deferred `EventStore` persistence:

> "The `EventStore` already exists. Wiring storage-backed emission would require either: (a)
> adding a `KhiveRuntime` handle to `VerbRegistry`, or (b) adding an `Arc<dyn EventStore>` field
> to `VerbRegistryBuilder`. Both options are correct; the right coupling is a separate design
> decision that belongs with the v0.3 work."

ADR-033 §"Implementation Status": "`EventStore::append_event` wiring — deferred to v0.3".

This ADR closes both deferred items simultaneously, as they share the same dispatch-site change.

## Decision

### 1. `Deny` is authoritative — `PermissionDenied` before pack dispatch

When `gate.check()` returns `Ok(GateDecision::Deny { reason })`, `VerbRegistry::dispatch` returns
`RuntimeError::PermissionDenied { verb, reason }` immediately. The pack is never invoked.

The `AllowAllGate` default returns `Allow` for every request — personal/local users who have
never written a policy are unaffected. The behavior change is gated entirely behind the `Gate`
impl in use.

```rust
// New variant in RuntimeError (crates/khive-runtime/src/error.rs):
#[error("permission denied for verb {verb:?}: {reason}")]
PermissionDenied { verb: String, reason: String },
```

### 2. `Arc<dyn EventStore>` on `VerbRegistryBuilder` for audit persistence (option b)

ADR-033 §"Storage" offered two options:

- **(a)** Add a `KhiveRuntime` handle to `VerbRegistry` — couples the registry to storage, query,
  embedding, and every other runtime capability.
- **(b)** Add an `Arc<dyn EventStore>` to `VerbRegistryBuilder` — narrower: the registry
  only gains the one capability it needs for audit persistence.

**This ADR picks option (b).** Rationale in §Rationale below.

The field is `Option<Arc<dyn EventStore>>` — callers that do not configure a store continue to
work; the audit tracing path is the only emission for them, unchanged from v0.2.

```rust
// VerbRegistryBuilder gains:
pub fn with_event_store(&mut self, store: Arc<dyn EventStore>) -> &mut Self { ... }

// VerbRegistry gains (private):
event_store: Option<Arc<dyn EventStore>>,
```

### 3. Emission order at the dispatch site

After `gate.check` returns `Ok`:

1. Build `AuditEvent::from_check`.
2. Emit via `tracing::info!(audit_event = ..., "gate.check")` — the v0.2 path, preserved.
3. If `event_store.is_some()`, call `append_event(storage_event)` asynchronously; log warnings
   on failure, never propagate storage errors to the caller.
4. If decision is `Deny`, return `PermissionDenied` error — pack dispatch does not happen.

The storage `Event` is built from the audit data: `verb`, `namespace`, `actor`, outcome
(`EventOutcome::Denied` for deny, `EventOutcome::Success` for allow),
`SubstrateKind::Event`. The full `AuditEvent` envelope (including `deny_reason`, `gate_impl`,
`obligations`, and `session_id`) is serialized as JSON into `Event.data` so EventStore
consumers can recover the complete audit record.

When `gate.check` returns `Err(GateError)`: warn via tracing, do **not** write to `EventStore`
(no decision was produced), do **not** block dispatch. This preserves the fail-open contract for
gate infrastructure failures (a misconfigured Rego policy should not take down the whole server).

### 4. `EventOutcome::Denied` variant

The `EventOutcome` enum in `khive-types` already has `Success`, `Denied`, and `Error` variants.
Audit denies are deliberate policy decisions, distinct from operational errors — this PR uses the
existing `EventOutcome::Denied` variant to make deny events first-class and queryable.

## Rationale

### Why option (b) over option (a)

`VerbRegistry` is intentionally transport-agnostic and decoupled from storage. It holds only the
gate and the pack list. Adding `KhiveRuntime` (option a) would drag in storage backends,
embedding services, query compilers, and config — everything the runtime owns — into the
registry's dependency surface. That coupling would be difficult to reverse.

`Arc<dyn EventStore>` (option b) adds exactly one capability: append-only writes to the audit
log. The registry does not gain read access, query access, or any other storage surface. The
dependency is proportional to the responsibility.

The `MCP server` path (`KhiveMcpServer::with_packs`) already has access to `KhiveRuntime` and
therefore `runtime.events()` — it can thread the store into the builder with one line, keeping
the server setup readable.

### Why authoritative enforcement now

The advisory window served its purpose: `AuditEvent` is in production (ADR-033), the `RegoGate`
is validated against it (ADR-032), and the `AllowAllGate` default has been exercised in real
deployments. There is no reason to let Deny decisions pass silently once the audit record shows
that policies are matching as intended.

### Why `PermissionDenied { verb, reason }` shape

Consistent with `RuntimeError::NotFound(String)` and `RuntimeError::InvalidInput(String)`.
Adding `verb` makes the error message more actionable (clients see which verb was denied, not
just the reason) and aids observability in log aggregators.

### Why gate `Err` does not write to EventStore and does not block

An error from `gate.check` means the gate infrastructure itself failed — a Rego policy
evaluation error, a missing policy bundle, an internal timeout. Blocking dispatch on
infrastructure failures would make every verb depend on gate infrastructure availability, which
violates the fail-open contract for local deployments. The gate erroring out is not an
authorization decision; it is an operational event. Tracing alone is sufficient.

### Why the tracing path is preserved alongside EventStore

Log aggregators (Loki, CloudWatch) that are already consuming `gate.check` structured events
must not break when storage is added. The two paths are independent sinks. Operators can use
either or both; the audit record in the EventStore is an additional convenience, not a
replacement.

## Alternatives Considered

| Alternative                                                       | Pros                               | Cons                                                                    | Why rejected                                                               |
| ----------------------------------------------------------------- | ---------------------------------- | ----------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| Deny-as-`Option` (return `None` on deny, `Some(result)` on allow) | No new error variant               | Breaks the error model; callers can't distinguish deny from "no result" | Wrong semantic; errors are errors                                          |
| Parallel-write to EventStore in a spawned task                    | Non-blocking hot path              | Fire-and-forget races; harder to assert in tests                        | Integration tests need the write to complete synchronously before querying |
| Separate audit-bus (channel, broadcast)                           | Decouples registry from EventStore | Adds infrastructure (channel, background task) for one sink             | Over-engineering for a single append-only write                            |
| Option (a): `KhiveRuntime` on `VerbRegistry`                      | Co-locates all capabilities        | Couples registry to full runtime surface                                | Violates minimum-necessary coupling; hard to reverse                       |
| Block dispatch on gate `Err`                                      | Strict; no ambiguity               | Breaks local users when gate infra fails                                | Fail-open is intentional for infra failures                                |

## Consequences

### Positive

- Deny decisions from any `Gate` impl now block verb execution — the auth layer has teeth.
- Audit events persist to the substrate, enabling SQL queries over gate history.
- The `EventStore` wiring is opt-in per `VerbRegistryBuilder` — zero behavior change for
  consumers that don't call `with_event_store`.
- `EventOutcome::Denied` makes deny events first-class and queryable distinct from failures.
- Tracing emission is preserved — no log aggregator regressions.

### Negative

- `RuntimeError` grows one variant; existing `match` exhaustiveness checks must add an arm.
  (Impact: low — the variant is new; existing match arms with `_` catch-alls are unaffected.)
- `VerbRegistry` grows an `Option<Arc<dyn EventStore>>` field. Clone is still cheap (Arc).
- `EventOutcome::Denied` already exists in `khive-types`; no breaking change to the enum.
  Consumers that exhaustively match on `EventOutcome` are unaffected by this PR.

### Neutral

- Personal/local users who never write a policy use `AllowAllGate` → zero behavior change.
  The key safety guarantee: `AllowAllGate` MUST continue to return `Allow` for every request.
  Existing tests on the default path continue to pass unchanged.
- The `MCP server` wires the `EventStore` from the runtime during `KhiveMcpServer::with_packs`.
  This is a one-line addition; the overall server setup shape is unchanged.
- Gate `Err` path is still fail-open (warn + proceed). Hard-fail on gate errors belongs to a
  future ADR when infrastructure reliability requirements tighten.

## Implementation Status

| Step                                              | Where                                    | Status                               |
| ------------------------------------------------- | ---------------------------------------- | ------------------------------------ |
| `RuntimeError::PermissionDenied { verb, reason }` | `crates/khive-runtime/src/error.rs`      | done (this ADR)                      |
| `EventOutcome::Denied` variant                    | `crates/khive-types/src/lib.rs`          | pre-existing (not added by this ADR) |
| `VerbRegistryBuilder::with_event_store`           | `crates/khive-runtime/src/pack.rs`       | done (this ADR)                      |
| `VerbRegistry::event_store` field                 | `crates/khive-runtime/src/pack.rs`       | done (this ADR)                      |
| Hard deny in `VerbRegistry::dispatch`             | `crates/khive-runtime/src/pack.rs`       | done (this ADR)                      |
| EventStore write in `VerbRegistry::dispatch`      | `crates/khive-runtime/src/pack.rs`       | done (this ADR)                      |
| `KhiveMcpServer::with_packs` wires `event_store`  | `crates/khive-mcp/src/server.rs`         | done (this ADR)                      |
| ADR-029 implementation status update              | `docs/adr/ADR-029-authorization-gate.md` | done (this ADR)                      |
| ADR-033 implementation status update              | `docs/adr/ADR-033-audit-envelope.md`     | done (this ADR)                      |
| `EventFilter` + query surface for audit events    | `crates/khive-runtime/src/operations.rs` | deferred (post v0.3)                 |
| Alert / SLO on deny counts                        | TBD                                      | deferred                             |

## References

- [ADR-029](ADR-029-authorization-gate.md): Authorization gate — "deferred to v0.3" row this
  ADR closes; §"Alternatives — why advisory in v0.2" context
- [ADR-032](ADR-032-rego-gate.md): Rego gate — the primary production Gate impl affected by
  enforcement
- [ADR-033](ADR-033-audit-envelope.md): Audit envelope — `AuditEvent` type this ADR wires into
  storage; §"Storage: deferred to v0.3" row this ADR closes
- [ADR-004](ADR-004-substrate-observables.md): Event as substrate — the storage layer now used
- [ADR-025](ADR-025-pack-standard.md): Pack standard — packs are the target of dispatch gating
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single dispatch site — where both changes land
