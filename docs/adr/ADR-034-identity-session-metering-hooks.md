# ADR-034: Identity, Session, and Metering Extension Hooks

**Status**: superseded\
**Date**: 2026-05-19\
**Authors**: Ocean, lambda:khive

## Context

`khive-gate` today provides:

- A pluggable `Gate` trait with `AllowAllGate` default and `RegoGate` (Rego/regorus) backend
  ([ADR-029](ADR-029-authorization-gate.md), [ADR-032](ADR-032-rego-gate.md)).
- Structured `AuditEvent` per dispatch, emitted via `tracing::info!`; ADR-033 defines an
  `EventStore` trait for persistence, with the dispatch-time wiring planned (not yet shipped).
- `ActorRef { kind, id }` — a two-field identity tag on every `GateRequest`. Not persisted;
  asserted by the transport caller and trusted by `AllowAllGate`.
- `GateContext::session_id` — a free-form string forwarded from the transport. Not validated,
  not stored, not lifecycle-managed.
- `Namespace` — a plain string owned by nobody ([ADR-007](ADR-007-namespace-as-open-string.md)).
  Any transport can claim any namespace.
- Pack machinery ([ADR-025](ADR-025-pack-standard.md)) — composable vocabulary extension.

These primitives are sufficient for single-process, single-user deployments. Some downstream
deployments — multi-user services, audited environments, deployments with usage budgets —
need more:

1. **Persisted actor identity.** An actor record exists in a substrate table, not just
   asserted per-request. When an actor authenticates, its kind, id, and namespace are
   resolved against that record.
2. **Session lifecycle.** Sessions are created, validated, renewed, and expired. A session
   token is not valid indefinitely.
3. **Namespace ownership policy.** `Namespace` is an open string today; some deployments
   need to enforce that writes to namespace `X` require the caller to own `X` (or hold
   a delegation).
4. **Usage metering.** Verb invocations are counted per actor per quota or usage-budget period.
5. **Metering consumption seam.** Counted invocations must be consumable by an external
   sink (quota enforcer, usage analytics, downstream accounting) without coupling the
   dispatch path to any specific backend.

None of these five behaviors belong in the OSS substrate itself: actor records have no canonical
schema, session tokens have no canonical format, ownership semantics vary (flat owner, team
delegation, hierarchical), and metering destinations differ across deployments. But the OSS
substrate is also the **single dispatch site** (ADR-027), so any extension that needs to
intercept the request lifecycle must do it through traits the substrate exposes — or fork
the dispatcher.

This ADR adds the trait surface and obligation variant that let downstream deployments
implement persistent identity, session lifecycle, ownership policy, and metering without
forking dispatch.

---

## Decision

### D1: Actor Persistence — `ActorStore` trait in `khive-gate`

The OSS deliverable is a trait, not an implementation:

```rust
// crates/khive-gate/src/lib.rs (additive)
pub trait ActorStore: Send + Sync + std::fmt::Debug {
    fn resolve(&self, kind: &str, id: &str)
        -> Result<Option<ActorRecord>, GateError>;
}

pub struct ActorRecord {
    pub actor_ref:  ActorRef,
    pub namespace:  Namespace,
    pub created_at: DateTime<Utc>,
}
```

`ActorRecord` is the persisted projection of `ActorRef` plus its owned namespace.
No pack, no verb — storing actor records is out of scope for the pack model. OSS ships
`NoopActorStore` — returns `None` for every lookup, meaning the transport-asserted
`ActorRef` is accepted as-is, preserving today's behavior. Downstream deployments that
need persistence implement `ActorStore` (e.g., backed by SQL) and wire it into
`RuntimeConfig`.

`ActorStore` lives in `khive-gate` for the same reason `Gate` does: it is part of the gate
layer's input resolution, not the storage layer's CRUD. Placing it in `khive-types` or
`khive-storage` would force every storage crate to import a concept it does not use.

**Resolution call site:** `ActorStore::resolve` is invoked **inside the gate impl's
`check` method (gate-side)**, not in the transport. The transport asserts an `ActorRef`
on `GateRequest` (today's contract, unchanged); the gate resolves the actor before
evaluating its policy and incorporates the resolved record into whatever input shape
its policy evaluator consumes (for example, a `RegoGate` impl constructs an enriched
Rego input that includes the resolved namespace and any actor metadata). The exact
composition is a gate impl internal: the OSS contract is just
`Gate::check(&GateRequest) -> Result<GateDecision, GateError>`. ADR-034 does not
require new fields on `GateContext`. This concentrates identity resolution at the
single gate interception point established by ADR-029, so adding HTTP, CLI, or any
future transport does not require re-implementing the resolve dance per-transport.
Transports stay identity-agnostic; the gate is the identity resolution boundary.

### D2: Session Lifecycle — `SessionStore` trait in `khive-gate`

The OSS contract:

```rust
// crates/khive-gate/src/lib.rs (additive)
pub trait SessionStore: Send + Sync + std::fmt::Debug {
    fn create(&self, actor: &ActorRef, ttl_secs: u64)
        -> Result<SessionToken, GateError>;
    fn validate(&self, token: &str)
        -> Result<Option<SessionRecord>, GateError>;
    fn expire(&self, token: &str)
        -> Result<(), GateError>;
}

pub struct SessionToken(pub String);   // opaque string

pub struct SessionRecord {
    pub token:      SessionToken,
    pub actor_ref:  ActorRef,
    pub namespace:  Namespace,         // from the actor record at creation time
    pub expires_at: DateTime<Utc>,
}
```

Sessions are **not** a new pack. They are a cross-cutting concern that the gate layer manages.
The transport (MCP or HTTP) creates a session token on successful authentication and forwards
it on `GateContext::session_id`; the gate resolves that string against
`SessionStore::validate` and incorporates the resolved `SessionRecord` (actor + namespace +
expiry) into its policy input by the same internal mechanism it uses for `ActorStore`
resolution (see D1). The OSS `GateContext` wire shape is unchanged.

OSS ships `EphemeralSessionStore` — an in-memory `HashMap<String, SessionRecord>` suitable for
testing and single-process personal deployments. It is not durable; restart evicts all sessions.
Downstream deployments that need persistence implement `SessionStore` (e.g., backed by SQL).

The `session_id` field on `GateContext` and `AuditEvent` (ADR-033) remains a plain
`Option<String>` — the gate populates it from the resolved `SessionRecord.token`,
maintaining JSON wire-shape backward compatibility.

### D3: Namespace Ownership — Rego document contract

The OSS contract is the Rego policy interface, not a Rust trait:

```rego
# Canonical ownership check
package khive.gate

import rego.v1

# namespace_owner is a data document populated by the deployment's gate impl.
# Policy checks it; the gate impl populates it (e.g., from ActorStore).
default decision := {"decision": "deny", "reason": "no rule matched"}

decision := {"decision": "allow", "obligations": [
    {"kind": "meter", "tag": sprintf("verb.%s", [input.verb])}
]} if {
    input.actor.kind != "anonymous"
    data.khive.namespace_owner[input.namespace] == input.actor.id
}
```

The OSS substrate defines the `data.khive.namespace_owner` document shape as a public contract:
**this ADR locks only the single-owner shape — a flat `{ namespace_string: actor_id_string }`
map suitable for personal and small-team deployments**. Delegation, team ownership, and
hierarchical schemes are out of scope for ADR-034 and are tracked for a future amendment
that may broaden the document shape (e.g., add a sibling `delegations` document, or
restructure `namespace_owner` into a richer record). A deployment's gate impl populates
the document before evaluating each request; no other crate needs to know how.

This keeps namespace ACL out of OSS runtime code. The enforcement boundary is the gate
policy. The document contract is what this ADR locks; the _evolution_ of that contract
remains in Rego, not in Rust.

### D4: Usage Metering — `Obligation::Meter` variant in `khive-gate`

A new variant is added to the `Obligation` enum:

```rust
#[non_exhaustive]
pub enum Obligation {
    Audit     { tag: String },
    RateLimit { window_secs: u64, max: u32 },
    Meter     { tag: String, dimensions: serde_json::Value },
    Custom    { value: serde_json::Value },
}
```

**Rust source-compatibility precondition:** today's `Obligation` enum is a public,
exhaustive enum. Adding `Meter` is **not** source-compatible — downstream crates that
`match Obligation { … }` exhaustively will fail to compile. The implementation PR that
lands `Meter` MUST either (a) mark `Obligation` `#[non_exhaustive]` in the same commit,
or (b) land `#[non_exhaustive]` in a separate prep PR that ships first. Until then,
the `Custom { value }` variant is the documented escape hatch for downstream policies
that need metering-shaped data on the existing enum. The serde wire shape
(`{"kind":"meter", "tag":..., "dimensions":...}`) is JSON-additive regardless — existing
policies that emit only `Audit` obligations remain valid.

**Consumption scope:** ADR-034 ships only the obligation variant and its serde wire shape.
`Meter` is **advisory** in OSS: the dispatcher includes any emitted `Meter` obligations in
the `AuditEvent` it logs / persists via the ADR-033 path, alongside `Audit` and any other
obligations. ADR-034 does **not** add a dispatcher-side consumer hook (no `MeterSink` field
on `RuntimeConfig`, no obligation-consumer trait). Building such a hook — and the
synchronous-call semantics that go with it — is a separate concern, tracked as future work
(see "Future Work" below).

This separation is deliberate. Different consumer shapes (synchronous trait call, in-process
channel, polled `EventStore` reader, external broker bridge) have different performance and
failure-mode characteristics; locking one in ADR-034 would either over-commit OSS to a
specific deployment shape or repeat the rejected "second cross-cutting call site" pattern
(Alternatives table). Until the future consumer-hook ADR lands, downstream deployments that
need synchronous metering can either:

- Consume `Meter` from the gate decision **inside their own gate impl** before returning it
  (e.g., a `LambdaGate` wraps `RegoGate`, observes obligations, calls its own sink, returns
  the decision unchanged), or
- Wait for ADR-033's `EventStore::append_event` dispatch wiring to ship, then poll
  `AuditEvent` rows for `Meter` obligations.

Policy authors indicate metering intent via obligations:

```rego
{"kind": "meter", "tag": "verb.create", "dimensions": {"plan": "self-hosted"}}
```

`dimensions` is an arbitrary JSON object — same escape-hatch rationale as `Custom.value`. The
`tag` is the metered line-item identifier. This keeps metering taxonomy in Rego policy files,
not in OSS source.

**No event-streaming infrastructure is added by this ADR.** The path is:

```text
gate.check() → obligations: [Meter{tag, dims}, Audit{tag}, ...]
   ↓
dispatcher emits AuditEvent { obligations, ... }   (today: tracing::info!
                                                    planned per ADR-033: append to EventStore)
   ↓
verb dispatch runs
```

There is no Kafka, no Redis Streams, no NATS in the OSS surface introduced by ADR-034. Once
ADR-033's dispatch-time wiring lands, `EventStore` also serves as a durable event log
indexed by `created_at`, which a future consumer-hook ADR can use as the pull-based fallback
path. The shape of that hook (synchronous trait call, channel, polled reader) is explicitly
deferred.

### D5: Extension Boundary Rules

The four deliverables above form the extension boundary:

| OSS artifact                                | Where                         | Implemented by                                                |
| ------------------------------------------- | ----------------------------- | ------------------------------------------------------------- |
| `ActorStore` trait + `ActorRecord`          | `crates/khive-gate/`          | Downstream (e.g. SQL-backed)                                  |
| `NoopActorStore`                            | `crates/khive-gate/`          | Default — preserves today's behavior                          |
| `SessionStore` trait + `SessionRecord`      | `crates/khive-gate/`          | Downstream (e.g. SQL-backed)                                  |
| `EphemeralSessionStore`                     | `crates/khive-gate/`          | Default — in-memory, non-durable                              |
| `data.khive.namespace_owner` document shape | ADR-034 (this) + policy files | Downstream gate impl populates                                |
| `Obligation::Meter { tag, dimensions }`     | `crates/khive-gate/`          | Captured in `AuditEvent`; consumer hook deferred (future ADR) |

What downstream extension code MUST NOT cross:

- Extension crates MUST NOT import `khive-db` or `khive-runtime` to reach storage directly.
  All storage access goes through the traits defined in `khive-gate` and `khive-storage`.
- Extension crates MUST NOT write to the `notes`, `entities`, or `events` tables directly.
  Records belonging to extension infrastructure (e.g. actor records, session records) live
  in extension-owned tables — by convention, prefix them (`_khive_actors`, `_khive_sessions`)
  to avoid collision with substrate data.
- The `Gate::check` signature is the only call site extension code is permitted to intercept.
  Business logic injected elsewhere breaks the single-dispatch-site invariant of ADR-027.

---

## Rationale

### D1 rationale: trait in `khive-gate`, not a pack

Actor persistence is not a verb. It is not surfaced to agents calling `request(ops=...)`. A pack
would add verbs and kinds, but "create actor" and "resolve actor" are administrative operations
the transport handles before the request enters the dispatch path. Wrapping them in a pack
would expose admin verbs on the same surface as data verbs — violating the single-dispatch
principle by adding a privileged verb class only some actors may call.

A dedicated `khive-pack-identity` was rejected on the same grounds: verb dispatch is for data
operations, not identity management. Identity management is a transport/gate concern.

### D2 rationale: ephemeral store in OSS, not a panic-stub

`EphemeralSessionStore` serves two purposes: it provides a runnable default for OSS deployments
(single-user, single-process, no persistence required), and it gives downstream impls a
reference to test against. A panic-stub-only alternative was rejected because it breaks the
test harness: gate tests need a session lifecycle to exercise `GateContext::session_id`.

### D3 rationale: Rego document contract, not a Rust trait for ownership

Namespace ownership is inherently a policy decision. A Rust trait for ownership would encode
the enforcement model in code, requiring a breaking trait change every time the model
evolves (single owner → delegation → hierarchical → team). A Rego document contract keeps the
enforcement logic in policy files where it is auditable and changeable without recompiling the
binary.

ADR-034 ships only the **single-owner data document shape** for v0.3. A `NamespaceOwnerStore`
Rust trait was rejected because the shape will plausibly need to grow (delegation, team,
hierarchy), and a trait-based evolution path would require a breaking impl swap. Because the
contract lives in Rego, future evolution requires only an ADR-034 amendment to the document
shape — no Rust trait change, no impl breakage, no semver event for downstream consumers.
The flat shape is a starting commitment, not the final shape.

### D4 rationale: `Obligation::Meter` over a separate metering hook

The `Obligation` mechanism is already the established channel for "things the policy wants the
dispatcher to do." `Audit` proved the pattern. Adding `Meter` as a sibling variant means
metering is policy-driven — a per-write policy emits `Meter` on every write; a quota-only
policy emits `Meter` only on budget overage; a self-hosted policy emits no `Meter` at all.
Downstream sinks process whatever the policy sends. Hardcoding metering in the dispatch path
would make it impossible to conditionally meter without modifying OSS code.

A dedicated metering hook outside the obligation channel was rejected because it would add a
second cross-cutting call site parallel to the gate, violating the single-site principle
(ADR-027). The obligation channel is the correct generalization.

### D5 rationale: prefix convention for extension tables

The substrate data model (entities, notes, events) is the runtime's data. Extension
infrastructure records (actors, sessions, metering) are operational metadata, not research
data. A prefix convention (e.g., `_khive_`) separates them visually and prevents tools that
scan substrate tables from accidentally treating extension records as entities. A separate
SQLite file was rejected because it complicates transaction boundaries between extension
resolution and verb dispatch.

---

## Alternatives Considered

| Alternative                                       | Pros                                      | Cons                                                                           | Why rejected                                               |
| ------------------------------------------------- | ----------------------------------------- | ------------------------------------------------------------------------------ | ---------------------------------------------------------- |
| Bake persistence/sessions/metering into khive-oss | One codebase                              | Pre-commits OSS to one data model, schema, and consumer surface                | Pre-commits to a model that varies across deployments      |
| Fork `VerbRegistry::dispatch` downstream          | Clean separation                          | Must fork the dispatch surface; diverges on every OSS change                   | Forks the dispatch surface — maintenance cost is forever   |
| Trait-and-noop extension hooks (this ADR)         | Minimal OSS surface; clear extension seam | Requires disciplined "traits in OSS, impls downstream" governance              | Accepted: governed by this ADR                             |
| Full ACL trait in OSS                             | Generic; usable everywhere                | Pre-commits to one ownership model; extensibility becomes a compatibility cost | Ownership model belongs in policy (Rego), not traits       |
| `khive-pack-identity` for actor/session verbs     | Pack pattern is established               | Exposes admin-class verbs on the same surface as data verbs                    | Verb dispatch is for data; identity is a transport concern |
| `NamespaceOwnerStore` Rust trait                  | Type-safe; fast                           | Pre-commits to flat ownership; delegation/hierarchy requires interface changes | Rego policy is more expressive and auditable               |
| `MeterSink` field on `RuntimeConfig` (OSS)        | Cleaner than obligation channel           | Second cross-cutting call site; violates ADR-027 single-dispatch invariant     | Obligation channel is the established generalization       |

---

## Consequences

### Positive

- The Rego policy contract for namespace ownership means ownership semantics are auditable,
  changeable without a code release, and applicable to any deployment that wants namespace
  scoping (not only one specific consumer).
- `Obligation::Meter` makes metering policy-driven. Self-hosted deployments that never want
  metering author a policy that emits no `Meter` obligations; no code change required.
- `AuditEvent` (ADR-033) captures `Obligation::Meter` in the same structured record it already
  captures `Obligation::Audit`. No second log channel.
- OSS default behavior is unchanged: `AllowAllGate` + `NoopActorStore` + `EphemeralSessionStore`
  produce the same end-user experience as today.
- Adding HTTP, CLI, or any future transport does not require duplicating identity resolution
  per-transport — the gate is the single resolution point.

### Negative

- `ActorStore` and `SessionStore` traits in `khive-gate` expand the crate's public surface.
  Breaking changes to `ActorRecord` or `SessionRecord` field names require a deprecation cycle.
- `Obligation::Meter` is advisory in OSS. Deployments that depend only on khive-oss for
  metering will not get durable counts — the obligation is logged via `AuditEvent`, not
  separately stored.
- The `data.khive.namespace_owner` document contract is a soft contract: it is specified in
  this ADR and enforced by convention in policy files, not by the Rust type system.

### Neutral

- `GateContext::session_id` wire shape is unchanged. Transports that today pass an arbitrary
  session string continue to work. Persistence-aware transports replace the arbitrary string
  with a token resolved via `SessionStore::validate`.
- `Obligation` remains an internally-tagged enum. The serde representation
  `{"kind": "meter", "tag": "...", "dimensions": {...}}` is JSON-additive — existing
  policies that emit only `Audit` obligations require no change. The Rust enum surface
  changes per D4's precondition (`#[non_exhaustive]` must land alongside or before
  `Meter`); downstream Rust consumers that match `Obligation` exhaustively need a
  fallback arm at that point.
- Extension-owned tables (actor records, session records, metering counters) are NOT managed
  by the OSS migration system ([ADR-022](ADR-022-schema-migrations.md)). Downstream
  migrations are separate.

---

## Implementation Status

| Deliverable                                             | Location                           | Status                             |
| ------------------------------------------------------- | ---------------------------------- | ---------------------------------- |
| `ActorStore` trait + `ActorRecord` type                 | `crates/khive-gate/`               | planned                            |
| `NoopActorStore` default impl                           | `crates/khive-gate/`               | planned                            |
| `SessionStore` trait + `SessionRecord` + `SessionToken` | `crates/khive-gate/`               | planned                            |
| `EphemeralSessionStore` in-memory impl                  | `crates/khive-gate/`               | planned                            |
| `Obligation::Meter { tag, dimensions }` variant         | `crates/khive-gate/`               | planned                            |
| `data.khive.namespace_owner` document contract          | ADR-034 (this) + policy files      | specified                          |
| Hard gate enforcement (deny → dispatch error)           | `crates/khive-runtime/src/pack.rs` | planned (separate PR / ADR)        |
| `EventStore` wiring for `AuditEvent` dispatch           | `crates/khive-runtime/src/pack.rs` | planned (per ADR-033, separate PR) |

Downstream impls (persisted `ActorStore`/`SessionStore`, gate impls that populate
`namespace_owner` and observe `Meter` obligations) live in their own repositories. The
dispatcher-side obligation consumer hook itself is out of scope for ADR-034 — see
"Future Work" above.

---

## Decisions Resolved

The following design choices were called out during ADR review and are locked here so
implementation PRs do not re-litigate them:

1. **`ActorStore::resolve` is gate-side, not transport-side.** See D1 above.
2. **Metering uses the obligation channel, not a separate `MeterSink` field on
   `RuntimeConfig`.** See D4 above and the alternatives table.
3. **No event-streaming infrastructure is added by this ADR.** See D4 — `Meter` is captured
   in `AuditEvent.obligations`. Once ADR-033's dispatch-time wiring lands, `EventStore`
   becomes a polled fallback path for downstream consumers.
4. **No dispatcher-side consumer hook is added by this ADR.** A consumer hook (synchronous
   trait call, channel, polled reader) is a separate concern and is tracked as future work
   (see "Future Work" below). Downstream deployments that need synchronous metering today
   consume `Meter` inside their own gate impl (`LambdaGate` wraps `RegoGate`, observes
   obligations, calls its sink, returns the unchanged decision).

## Future Work

The following are explicitly **out of scope** for ADR-034 and tracked for a separate ADR:

- **Dispatcher-side obligation consumer hook.** A trait + `RuntimeConfig` field that lets
  downstream code consume obligations (including `Meter`) synchronously from the dispatcher,
  without wrapping a gate impl. Shape options include `ObligationConsumer::consume(&self,
  obligation, gate_request, gate_decision)`, a typed `MeterSink::record(...)` specialization,
  or a channel-based fan-out. The right shape depends on performance characteristics
  (synchronous backpressure vs. write-behind) and failure-mode requirements (fail-closed on
  consumer error vs. best-effort) — both of which need empirical evidence before being
  baked into OSS.
- **`AuditEvent` pull-based consumers.** Once ADR-033 wires `EventStore` into the dispatch
  path, a pattern emerges for any consumer (analytics, replay, fraud detection) to poll the
  event log instead of subscribing in-process. The polling cadence, cursor semantics, and
  delivery guarantees would belong in a separate ADR.

## Deferred to Implementation PRs

The following are intentionally not specified in this ADR; each will be decided in the PR
that lands the relevant impl:

1. **`namespace_owner` document invalidation strategy.** Per-request refresh vs. cached
   (TTL) vs. session-scoped vs. event-driven invalidation is a perf-vs-staleness tradeoff
   that depends on the deployment's actor cardinality and write rate. The trait shape does
   not force a choice.
2. **`EphemeralSessionStore` thread-safety model.** `RwLock<HashMap>` vs. `DashMap` vs. a
   single-threaded assumption. Implementation detail of one impl; revisit if benchmarking
   shows contention.
3. **Session token format.** Opaque random ID (DB round-trip on validate) vs. signed JWT
   with embedded claims (stateless validate) is a latency-vs-statefulness tradeoff. The
   `SessionToken(String)` trait shape accommodates both; the choice belongs to each
   `SessionStore` impl.

---

## References

- [ADR-007](ADR-007-namespace-as-open-string.md): Namespace as open string — the baseline this
  ADR extends with ownership policy semantics
- [ADR-025](ADR-025-pack-standard.md): Pack standard — why actor persistence is not a pack
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single dispatch site — the invariant a
  separate metering hook outside the obligation channel would violate
- [ADR-029](ADR-029-authorization-gate.md): Authorization gate — the trait, `ActorRef`,
  `GateContext::session_id`, and `Obligation` enum extended here
- [ADR-032](ADR-032-rego-gate.md): Rego policy backend — the enforcement engine for
  `data.khive.namespace_owner` policy
- [ADR-033](ADR-033-audit-envelope.md): Audit envelope — `Obligation::Meter` is captured
  alongside `Obligation::Audit` in `AuditEvent.obligations`; `EventStore` provides the
  event log seam
- Hard gate enforcement (deny → dispatch error) and dispatch-time `EventStore` wiring are
  prerequisites for the `Meter` obligation channel and the "EventStore as fallback event log"
  claim in D4. They are tracked as separate planned work, not assumed by this ADR. Until they
  land, `Obligation::Meter` is logged via `tracing::info!` only and downstream sinks must
  consume it from the gate decision directly (not from a persisted log).
- `crates/khive-gate/src/lib.rs`: Existing `Gate`, `ActorRef`, `GateContext`, `Obligation`
  types this ADR extends
- `crates/khive-types/src/namespace.rs`: `Namespace` — the open string this ADR adds

---

## Amendment — 2026-05-20

**Status**: superseded\
**Date**: 2026-05-20\
**Rationale**: Identity, session lifecycle, and metering are cloud middleware concerns, not OSS
gate concerns. The extension hooks specified here (`ActorStore`, `SessionStore`,
`Obligation::Meter`) belong in the khive-cloud authorization layer (khive-cloud ADR-039), where
tenant identity and billing context are available at request time. The OSS `khive-gate` crate
stays minimal: authorization decisions only, no identity persistence or metering obligations.\
**Affected sections**: Status line (above); Implementation Status table (§Implementation
Status) — all deliverables listed as planned remain unimplemented and will not be implemented
in the OSS layer; Future Work section — cloud metering channel transfers to khive-cloud scope.
ownership semantics to
