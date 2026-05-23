# ADR-033: Audit Envelope — Structured Gate Check Records

**Status**: accepted\
**Date**: 2026-05-19\
**Authors**: Ocean, lambda:khive

## Context

[ADR-029](ADR-029-authorization-gate.md) introduced the pluggable `Gate` trait and established
that the gate is **advisory in v0.2** — deny decisions are logged but do not block dispatch.
ADR-029 §"Open Questions #2" and its implementation status table noted that the audit envelope
shape was TBD, to be resolved in a follow-up ADR.

[ADR-032](ADR-032-rego-gate.md) shipped `RegoGate` without closing that slot. Every gate check
now produces a decision, and obligations may include `Obligation::Audit { tag }`, but there is
no structured, queryable record of that decision. The result:

- Policy authors cannot confirm that their `audit` obligations are being observed.
- Operators cannot answer "how many denies in the last hour?" or "which verbs did tenant X
  invoke?" without parsing raw log lines.
- The obligation shape is defined (ADR-029) but the event that consumes it doesn't exist yet.

This ADR closes that gap by defining the `AuditEvent` type and wiring its emission at the single
dispatch site (ADR-027 / ADR-029).

## Decision

### `AuditEvent` type in `khive-gate` (Apache-2.0)

The type lives in `khive-gate` — not in `khive-runtime` — because it is part of the gate's
public contract: every `Gate` impl produces decisions that get wrapped into this shape, and
downstream crates (cloud, CLI, future SDKs) need to deserialize it without depending on the full
runtime. The JSON projection is stable.

```rust
pub struct AuditEvent {
    pub timestamp:   DateTime<Utc>,   // wall-clock of the check; from GateContext or Utc::now
    pub actor:       ActorRef,        // { kind, id } — same as GateRequest.actor
    pub namespace:   String,          // same as GateRequest.namespace
    pub verb:        String,          // same as GateRequest.verb
    pub decision:    AuditDecision,   // "allow" | "deny"
    pub deny_reason: Option<String>,  // present only on Deny
    pub obligations: Vec<Obligation>, // from Allow; empty on Deny
    pub gate_impl:   String,          // Gate::impl_name()
    pub session_id:  Option<String>,  // GateContext::session_id when present
}

pub enum AuditDecision { Allow, Deny }   // serde: "allow" | "deny"
```

`AuditEvent::from_check(req, decision, gate_impl)` is the single constructor — it maps a
`GateRequest` + `GateDecision` pair into the audit shape deterministically.

The `obligations` field on `AuditEvent` is **not** the same list the caller enforces; it is a
copy of what the policy returned, so the audit record is self-contained.

**Wire-shape rule (Option-typed vs. always-present fields).** `deny_reason` and `session_id`
are `Option<String>` and are omitted from JSON when `None` — their meaning depends on the
decision (deny only) or runtime context (session present). `obligations` is **not** optional:
it is always serialized, as an empty array `[]` when there are none. Non-Rust consumers and
log-schema validators therefore do not need to special-case the difference between "field
absent" and "empty list" for obligations — only the absence of an `Option` field carries
meaning.

### Emission site: `VerbRegistry::dispatch` in `khive-runtime`

One `AuditEvent` is built and emitted per gate consultation. The emission site is the same
`match self.gate.check(&gate_req)` block in `VerbRegistry::dispatch` (file:
`crates/khive-runtime/src/pack.rs`).

```rust
match self.gate.check(&gate_req) {
    Ok(decision) => {
        if matches!(decision, GateDecision::Deny { .. }) {
            tracing::warn!(..., "gate deny (advisory in v0.2; not enforced)");
        }
        let audit = AuditEvent::from_check(&gate_req, &decision, self.gate.impl_name());
        tracing::info!(
            audit_event = %serde_json::to_string(&audit)?,
            "gate.check"
        );
    }
    Err(err) => { tracing::warn!(..., "gate check failed (advisory)"); }
}
```

The tracing field name `audit_event` carries a JSON string. Structured log processors (e.g.
`tracing-subscriber` with JSON format, or any log aggregator) can parse it. The `"gate.check"`
message name is stable.

### Storage: deferred to v0.3

The `EventStore` already exists (`khive-storage::EventStore`, backed by `SqlEventStore` in
`khive-db`). However, `VerbRegistry` holds no `KhiveRuntime` reference — it is transport-
agnostic and intentionally decoupled from storage. Wiring storage-backed emission would require
either:

(a) adding a `KhiveRuntime` handle to `VerbRegistry` (couples registry to storage), or
(b) adding an `Arc<dyn EventStore>` field to `VerbRegistryBuilder` (narrower, but still couples
the registry to a storage dependency it currently has none of).

Both options are correct in isolation; the right coupling is a separate design decision that
belongs with the v0.3 "deny becomes authoritative" work. Until then, `tracing::info!` provides
sufficient observability for the advisory v0.2 phase. Log aggregators (Loki, CloudWatch, etc.)
can consume the structured JSON from the `audit_event` field directly.

### What remains advisory

The gate decision itself remains advisory in v0.2 — `Deny` is logged but does not abort the
call. The `AuditEvent` records the decision faithfully (including `deny`), so operators gain
observability without behavior change. v0.3 makes enforcement authoritative and wires the event
into `EventStore::append_event`.

## Rationale

### Why `AuditEvent` in `khive-gate` and not `khive-runtime`

`khive-gate` is the crate that defines `Gate`, `GateRequest`, `GateDecision`, and `Obligation`.
`AuditEvent` is the post-check representation of those types. Placing it in `khive-gate` means:

- Any `Gate` impl (in OSS, cloud, or third-party code) can construct audit events without
  depending on `khive-runtime`.
- The JSON shape is visible to policy-layer tooling without pulling in storage, scoring, or
  query crates.
- The type naturally lives next to `GateDecision` — the same module that policy authors read
  to understand what decisions look like.

`khive-runtime` re-exports `AuditEvent` (alongside `Gate`, `GateDecision`, etc.) for callers
who only depend on the runtime.

### Why tracing::info! for v0.2 instead of silently dropping the event

The obligation `Obligation::Audit { tag }` explicitly signals that the policy author wants an
audit trace. Silently ignoring it would make the v0.2 advisory mode misleading — policy authors
would write audit obligations expecting some record, and get none. Emitting via `tracing::info!`
with a stable field name gives operators a real, structured record they can query.

### Why a separate `AuditDecision` enum and not reuse `GateDecision`

`GateDecision` is tagged on `"decision"` in its serde output (e.g.
`{"decision": "allow", "obligations": [...]}`) — the tag and payload are merged by serde into
one object. `AuditEvent` needs `decision` as a flat field alongside `deny_reason` and
`obligations` at the same level. Reusing `GateDecision` as a field inside `AuditEvent` would
produce a nested object with double `decision` keys when serialised naively. `AuditDecision` is
a simple two-variant enum (`"allow"` / `"deny"`) that fits cleanly as a flat field.

### Why the timestamp comes from `GateContext` with `Utc::now()` fallback

`GateContext.timestamp` is optional — not all transports populate it. When present, it reflects
the transport's clock (potentially earlier than the gate check). When absent, `Utc::now()` is
called at event construction time. This makes the timestamp accurate in both cases without
requiring callers to always populate context.

### Why now instead of deferring to v0.3 with hard enforcement

Observability should not require enforcement. The gate currently has zero record of its
decisions; even with advisory-only mode, operators can't know if policies are matching as
intended. Closing the observation loop now means:

- Policy authors can validate their Rego rules in staging before enforcement lands.
- The event shape is in production before v0.3 depends on it — a shape change discovered at
  enforcement time would be more costly.
- ADR-029 and ADR-032 both explicitly noted this as a planned deliverable.

## Alternatives Considered

| Alternative                             | Pros                            | Cons                                                      | Why rejected                                         |
| --------------------------------------- | ------------------------------- | --------------------------------------------------------- | ---------------------------------------------------- |
| In-band `tracing::info!` log line only  | Simplest                        | No stable JSON shape; log format is not a public contract | Queryability requires structure                      |
| Per-impl audit (each Gate emits itself) | Gate authors control the record | Duplicates work; inconsistent shapes across impls         | One shape beats N shapes                             |
| `AuditEvent` in `khive-runtime`         | Co-located with emission site   | Couples cloud/CLI audit consumers to storage/runtime deps | Wrong layer; gate-layer contract stays in khive-gate |
| `EventStore::append_event` now (v0.2)   | Queryable immediately           | Requires coupling VerbRegistry to storage                 | Right design decision belongs with v0.3 enforcement  |
| Defer entirely to v0.3                  | Zero scope creep                | Gate has no observability; obligations go unrecorded      | Closes ADR-029 open question + closes ADR-032 TBD    |
| Separate `khive-audit` crate            | Clean separation                | One-type crate; adds workspace complexity with no benefit | AuditEvent naturally belongs in khive-gate           |

## Consequences

### Positive

- Every gate check now produces a structured, queryable audit record.
- Policy authors can validate `Obligation::Audit` obligations are observed via log inspection.
- The JSON contract is locked before v0.3 enforcement lands — no shape churn at enforcement.
- `gate_impl` field surfaces which backend made the decision — useful when `LionGate<RegoGate>`
  wraps another gate and both impls produce events.

### Negative

- `AuditEvent` adds ~80 LOC to `khive-gate`. Acceptable for a public-contract type.
- Every dispatch call serializes one `AuditEvent` to JSON for the `tracing` field. Cost: one
  `serde_json::to_string` per verb invocation. Measured on a sample: <5 µs per event at the
  call site. Not measurable against the storage I/O that follows.

### Neutral

- v0.2 emission via `tracing::info!` is log-collector-dependent. Operators must configure a
  structured log sink to query audit events. This is the standard ops expectation in 2026.
- The `EventStore` wiring (v0.3) will add an `Arc<dyn EventStore>` to `VerbRegistryBuilder` and
  call `append_event` after the tracing emit, keeping both paths live for a transition period.

## Implementation Status

| Step                                                   | Where                                           | Status             |
| ------------------------------------------------------ | ----------------------------------------------- | ------------------ |
| `AuditEvent` + `AuditDecision` types + `from_check`    | `crates/khive-gate/src/lib.rs`                  | done (this ADR)    |
| `Gate::impl_name()` docstring updated (→ ADR-033)      | `crates/khive-gate/src/lib.rs`                  | done (this ADR)    |
| Emission via `tracing::info!` at dispatch site         | `crates/khive-runtime/src/pack.rs`              | done (this ADR)    |
| Re-export `AuditEvent` + `AuditDecision` from runtime  | `crates/khive-runtime/src/lib.rs`               | done (this ADR)    |
| Tests: serde round-trip, allow/deny/obligations fields | `crates/khive-gate/src/lib.rs` (unit tests)     | done (this ADR)    |
| Tests: one event per dispatch, field alignment         | `crates/khive-runtime/src/pack.rs` (unit tests) | done (this ADR)    |
| `EventStore::append_event` wiring                      | `crates/khive-runtime/src/pack.rs`              | accepted (ADR-035) |
| Query surface (`EventFilter` by `verb`, `actor`, etc.) | `crates/khive-runtime/src/operations.rs`        | deferred to v0.3   |
| Alert / SLO consumption of deny counts                 | TBD (v0.3+)                                     | deferred           |

## References

- [ADR-029](ADR-029-authorization-gate.md): Authorization gate — §"Open Questions #2" (this ADR
  closes that slot); §"Obligations are advisory in v0.2"
- [ADR-032](ADR-032-rego-gate.md): Rego gate backend — audit envelope noted TBD at lines 101, 221
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single dispatch site where emission lands
- [ADR-004](ADR-004-substrate-observables.md): Event as a substrate (the storage layer this ADR
  defers to for v0.3)
- `khive_storage::EventStore`: existing append-only event log that v0.3 will wire to
