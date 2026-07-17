# ADR-018: Authorization Gate

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

khive's verb dispatch path is the chokepoint where every operation reaches storage.
Without an authorization layer, anyone with transport access (stdio for MCP, HTTP for
the future gateway, FFI for embedded callers) can invoke any verb on any namespace.
For personal-local deployments this is fine. For:

- **Multi-tenant deployments** — one khive process serving multiple users or agents
- **Deployments with auditability requirements** — any scenario where operators need a record
  of which principals performed which actions
- **Public services** — exposed via HTTP gateway with authenticated principals

…the absence of gating is a deal-breaker.

The system must satisfy:

1. **Pluggable policy.** No hardcoded auth logic. Each deployment plugs in its policy
   engine — Rego, capability-based, OAuth-scope-based, or custom.
2. **Permissive defaults.** A permissive default (`AllowAllGate`) is the boot
   default; nothing changes for personal-local users.
3. **Hard enforcement from day 1.** `Deny` decisions block dispatch — no advisory
   phase, no "log-and-allow" mode. The gate is authoritative.
4. **Structured audit trail.** Every gate consultation produces a queryable record:
   who attempted what verb, on which namespace, with what decision, what reason. Stored
   in the substrate (`EventStore`) and emitted via structured logging.
5. **Fail-open on infrastructure errors.** A misconfigured Rego policy must not take
   down the whole server. Gate-infrastructure failures log a warning and proceed; only
   explicit `Deny` decisions block.
6. **License clean.** The default impl lives in Apache-2.0. Custom Gate backends
   ship under their own licenses without any coupling to this crate's license.

## Decision

### `Gate` trait in `khive-gate` (Apache-2.0)

```rust
// crates/khive-gate/src/lib.rs
pub trait Gate: Send + Sync + std::fmt::Debug {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError>;

    /// Stable identifier for this gate impl. Used in audit records.
    /// Default returns the type name; impls may override for clarity.
    fn impl_name(&self) -> &'static str { std::any::type_name::<Self>() }
}

pub struct GateRequest {
    pub actor:     ActorRef,
    pub namespace: Namespace,
    pub verb:      String,
    pub args:      serde_json::Value,
    pub context:   GateContext,
}

pub struct ActorRef {
    pub kind: String,  // "user" | "agent" | "lambda" | "anonymous" | custom
    pub id:   String,
}

pub struct GateContext {
    pub session_id: Option<String>,
    pub timestamp:  Option<DateTime<Utc>>,
    pub source:     Option<String>,   // "mcp" | "cli" | "http" | etc.
}

pub enum GateDecision {
    Allow { obligations: Vec<Obligation> },
    Deny  { reason: String },
}

pub enum Obligation {
    Audit     { tag: String },
    RateLimit { window_secs: u64, max: u32 },
    Custom    { value: serde_json::Value },
}
```

The JSON projection of `GateRequest` is the **public contract**. Field names
(`input.actor.kind`, `input.namespace`, `input.verb`, etc.) are what policies receive
as input. Changing a field name is a breaking change requiring an ADR amendment.

### `AllowAllGate`: the default gate

```rust
// crates/khive-gate/src/lib.rs
#[derive(Debug)]
pub struct AllowAllGate;

impl Gate for AllowAllGate {
    fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::Allow { obligations: vec![] })
    }
}
```

`RuntimeConfig::default()` sets `gate: Arc::new(AllowAllGate)`. Personal-local users
who do not explicitly configure a policy backend see no change from earlier khive
behavior — every verb is allowed.

The documentation warns that `AllowAllGate` is a footgun in multi-user or
networked contexts. Deployments serving multiple users should configure `RegoGate` or a custom backend before
serving traffic.

### `RegoGate`: canonical policy backend in `khive-gate-rego`

```rust
// crates/khive-gate-rego/src/lib.rs
pub struct RegoGate {
    engine: Mutex<regorus::Engine>,
    entrypoint: String,
}

pub const DEFAULT_ENTRYPOINT: &str = "data.khive.gate.decision";

impl RegoGate {
    pub fn from_policy_str(source: &str) -> Result<Self, GateError>;
    pub fn from_dir(path: &Path) -> Result<Self, GateError>;
    pub fn with_entrypoint(mut self, entrypoint: impl Into<String>) -> Self;
}

impl Gate for RegoGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        let input = serde_json::to_value(req)?;
        let mut engine = self.engine.lock();
        engine.set_input(input);
        let result = engine.eval_rule(&self.entrypoint)?;
        // Deserialize result as GateDecision...
    }

    fn impl_name(&self) -> &'static str { "RegoGate" }
}
```

Policies live in Rego files under a directory the operator configures:

```rego
# example.rego
package khive.gate

import rego.v1

# Fail-closed default. Without this, missing rule = evaluation error.
default decision := {"decision": "deny", "reason": "no rule matched"}

decision := {"decision": "allow", "obligations": []} if {
    input.verb == "search"
}

decision := {
    "decision": "allow",
    "obligations": [{"kind": "audit", "tag": sprintf("verb.%s", [input.verb])}],
} if {
    input.actor.kind == "user"
    input.namespace == "operator"
}

decision := {"decision": "deny", "reason": "anonymous callers cannot write"} if {
    input.actor.kind == "anonymous"
    input.verb == "create"
}
```

`khive-gate-rego` is a sibling crate, not a feature flag on `khive-gate`. Operators add
the dependency to opt in. Consumers that don't need Rego (e.g., a personal-local
binary) don't compile regorus at all.

Rego is the policy language because OPA (Open Policy Agent) is the cross-industry
standard. Policies authored against `RegoGate` are portable to any OPA consumer.

### Hard enforcement: `Deny` is authoritative

`VerbRegistry::dispatch` consults the gate before invoking the pack handler. If
`gate.check` returns `Deny`, dispatch aborts with `RuntimeError::PermissionDenied`:

```rust
// In VerbRegistry::dispatch
let decision = self.gate.check(&gate_req);

match &decision {
    Ok(GateDecision::Allow { .. }) => {
        // Continue to pack dispatch
    }
    Ok(GateDecision::Deny { reason }) => {
        // Emit audit event (see below), then return error
        emit_audit(&gate_req, &decision);
        return Err(RuntimeError::PermissionDenied {
            verb: gate_req.verb.clone(),
            reason: reason.clone(),
        });
    }
    Err(err) => {
        // Gate infrastructure failed — log and proceed (fail-open)
        tracing::warn!(error = ?err, "gate check failed; proceeding (fail-open)");
        // No audit event — no decision was produced
    }
}

// Continue to pack handler...
```

`RuntimeError::PermissionDenied { verb, reason }` is a first-class error variant.
Callers can match on it to distinguish authorization failures from operational errors.

### Fail-open on gate `Err`

A gate that errors out before it can produce a decision (e.g. request serialization
failure, a poisoned internal lock surfaced as `GateError::Internal`) returns
`Err(GateError)`. This is treated as **infrastructure failure**, not an authorization
decision. The dispatch proceeds; the error is logged.

Rationale: blocking dispatch on infrastructure failures means every verb depends on
gate-infrastructure availability. A misconfigured policy in production would take down
the entire khive surface. Fail-open at the infra layer keeps khive serving traffic;
operators see the warning and fix the policy.

This is distinct from `Deny`, which is an explicit policy decision. Deny blocks; Err
does not. Future deployments needing strict-mode (block on gate Err) can wrap `Gate`
in a `StrictGate { inner: Arc<dyn Gate> }` adapter — but that's not the default.

**`RegoGate` narrows this further than the base contract implies.** Policy-load
failures (a Rego file that fails to compile, or an empty policy directory) surface as
`Err(GateError::Policy)` at construction time, before the gate is ever installed —
consistent with fail-open-on-infra-error above. But at _dispatch_ time, `RegoGate`
never returns `Err` for policy-evaluation uncertainty: an undefined `decision` rule, a
`regorus` evaluation throw, a poisoned engine mutex, or a decision value that fails to
serialize or doesn't match the `GateDecision` shape are all converted to an explicit
`Ok(GateDecision::Deny)` with a diagnostic reason (`crates/khive-gate-rego/src/gate.rs`).
Only pre-evaluation failures (request serialization) reach `Err(GateError)` for
`RegoGate`. See `crates/khive-gate-rego/README.md` for the full breakdown.

### `AuditEvent` envelope

Every gate consultation that produces a decision emits an `AuditEvent`:

```rust
// crates/khive-gate/src/lib.rs
pub struct AuditEvent {
    pub timestamp:   DateTime<Utc>,
    pub actor:       ActorRef,
    pub namespace:   String,
    pub verb:        String,
    pub decision:    AuditDecision,           // "allow" | "deny"
    pub deny_reason: Option<String>,          // present only on Deny
    pub obligations: Vec<Obligation>,         // from Allow; empty on Deny
    pub gate_impl:   String,                  // from Gate::impl_name()
    pub session_id:  Option<String>,
}

pub enum AuditDecision {
    Allow,
    Deny,
}

impl AuditEvent {
    pub fn from_check(
        req: &GateRequest,
        decision: &GateDecision,
        gate_impl: &str,
    ) -> Self { ... }
}
```

`AuditEvent` lives in `khive-gate`, not `khive-runtime` — it's part of the gate's
public contract. Any `Gate` impl can construct audit events without depending on the
runtime; downstream tooling (alert pipelines, dashboards) deserializes the JSON shape
without coupling to storage or query crates.

The wire shape is stable:

```json
{
  "timestamp": "2026-05-23T01:50:00Z",
  "actor": { "kind": "user", "id": "operator" },
  "namespace": "research",
  "verb": "create",
  "decision": "allow",
  "obligations": [{ "kind": "audit", "tag": "verb.create" }],
  "gate_impl": "RegoGate"
}
```

`deny_reason` and `session_id` are `Option<String>` — omitted from JSON when `None`.
`obligations` is always serialized, as `[]` when empty.

### Audit emission: two sinks

Each `AuditEvent` lands in two sinks:

1. **Structured tracing** — `tracing::info!(audit_event = serde_json::to_string(&event)?, "gate.check")`.
   Log aggregators (Loki, CloudWatch, etc.) consume the JSON from the structured
   field. The message name `"gate.check"` is stable.

2. **EventStore persistence** — `VerbRegistry::dispatch` writes the `AuditEvent` as an
   `Event` substrate record (ADR-004). `Event.kind = "gate.check"`, `Event.outcome =
   EventOutcome::Allowed` or `EventOutcome::Denied`, `Event.data = serde_json::to_value(&audit_event)`.

The two sinks are independent. Operators can use either or both. The tracing path is
always live; the EventStore path is opt-in per `VerbRegistryBuilder` configuration.

### EventStore wiring: narrow coupling

`VerbRegistry` does NOT hold a `KhiveRuntime` handle. It holds exactly the capabilities
it needs:

```rust
pub struct VerbRegistryBuilder {
    packs: Vec<Arc<dyn PackRuntime>>,
    gate: Option<Arc<dyn Gate>>,
    event_store: Option<Arc<dyn EventStore>>,
    default_namespace: Namespace,
}

impl VerbRegistryBuilder {
    pub fn with_gate(self, gate: Arc<dyn Gate>) -> Self { ... }
    pub fn with_event_store(self, store: Arc<dyn EventStore>) -> Self { ... }
}
```

The `event_store` field is `Option<Arc<dyn EventStore>>`. Callers that don't configure
it run with tracing-only audit. Configured callers get both tracing and persistence.

This is "Option B" from the original ADR-035 design — narrower coupling than handing
the registry a full `KhiveRuntime`. The registry gains exactly one storage capability
(append-only event writes); it does not gain read access, query access, or any other
runtime surface.

The MCP server (`khive-mcp` / `kkernel mcp`) wires the event store from the runtime
during pack registration:

```rust
// In kkernel mcp startup
let runtime = KhiveRuntime::new(config.clone())?;
let registry = VerbRegistry::builder()
    .with_gate(config.gate.clone())
    .with_event_store(runtime.events()?)
    .with_default_namespace(runtime.config().default_namespace.clone())
    /* ... register packs ... */
    .build()?;
```

The cost is one extra line in server setup. The benefit is a clean dependency surface
on the registry.

### Audit storage shape

When the EventStore is wired, each gate consultation writes one `Event`:

```rust
let event = Event {
    id: Uuid::now_v7(),
    namespace: audit_event.namespace.clone(),
    kind: "gate.check".to_string(),
    actor: audit_event.actor.id.clone(),
    outcome: match audit_event.decision {
        AuditDecision::Allow => EventOutcome::Allowed,
        AuditDecision::Deny  => EventOutcome::Denied,
    },
    data: serde_json::to_value(&audit_event)?,
    created_at: audit_event.timestamp,
};
event_store.append_event(event).await?;
```

Audit events are queryable via `EventStore::query_events` and `khive-query` GQL/SPARQL
against the `events` table. Operators can answer "how many denies in the last hour?"
or "which verbs did tenant X attempt?" with structured queries.

### Storage write failure semantics

If `EventStore::append_event` fails (disk full, lock contention, etc.):

1. Log a warning via `tracing::warn!`.
2. Do NOT propagate the error to the caller.
3. The original verb dispatch outcome (Allow → proceed; Deny → PermissionDenied) is
   unaffected.

Audit storage is observability, not control flow. A failed audit write is a degraded
observability state, not a reason to fail the verb.

### Why no obligation enforcement in v1?

`Obligation::Audit { tag }` and `Obligation::RateLimit { ... }` are returned by
policies but NOT enforced by the runtime in v1. They're declarative signals:

- `Audit { tag }` — the policy explicitly wants this dispatch traced. The audit event
  contains the obligation list, so log aggregators can filter on tag presence.
  Enforcement: implicit (audit always fires when configured).
- `RateLimit { window_secs, max }` — policy wants this verb rate-limited per actor.
  The runtime does not enforce rate limits in v1. A future rate-limiter ADR adds
  per-actor counters consulted at dispatch.
- `Custom { value }` — arbitrary obligation payload. Consumer (the runtime, a custom
  wrapper, an external system) decides whether and how to enforce.

The shape is locked now so policy authors can write `allow { ... obligations: [...]
}` today. Enforcement subsystems land independently.

### Trust boundary alignment with ADR-003

Per ADR-003, the gate enforcement layer exists only in the agent binary (`khive-mcp`
or `kkernel mcp`). The operator binary (`kkernel sync`, `kkernel db migrate`, `kkernel
pack list`) runs without the gate — `AllowAllGate` is installed by default in
operator mode.

This matches the trust model: operators are trusted by definition (they have local
shell access); agents are untrusted by design (they speak through MCP from
potentially-untrusted clients).

The boundary is structural, not configurational. Operator commands cannot accidentally
be gated; agent commands cannot accidentally bypass the gate.

### `GateRequest.namespace` and runtime alignment

Policy `input.namespace` must reflect what the runtime sees. If the runtime treats
namespace `""` as "use default", the gate must see `""` too — coercing it to the
default in the gate layer would create an authorization blind spot on a field that
ADR-029 declares public.

`VerbRegistryBuilder::with_default_namespace` populates the registry's default. The
namespace passed to the gate is:

1. If `op.args["namespace"]` is present (including explicit `""` or `null`) — that
   value.
2. Otherwise — the registry's configured default.

Both the gate and the runtime resolve namespace through the same logic. Drift between
them is a security bug.

### Pack policy extension point

The authorization gate exposes a generic policy enforcement hook for packs that
need authorization decisions beyond static role/scope checks. Packs register
concrete policies:

```rust
pub trait PackGatePolicy: Send + Sync {
    fn evaluate(
        &self,
        input: &GateRequest,
        ctx: &GateContext,
    ) -> GateDecision;
}
```

The gate invokes the registered policy for verbs declared by the pack. Pack
policy implementations encapsulate per-pack rules (e.g., the proposal pack's
self-approve policy; see ADR-046).

Hierarchy:

- ADR-018 defines `GateRequest`, `GateContext`, `GateDecision`, and the
  `PackGatePolicy` hook.
- Packs (e.g., the proposal pack in ADR-046) define concrete policies and
  register them with the gate via `VerbRegistryBuilder::with_pack_policy`.
- The gate is the authoritative trust boundary. Handlers should defensively
  assert invariants but should not be the sole enforcement point.

`PackGatePolicy::evaluate` is called after the base `Gate::check` returns
`Allow` and the verb is owned by the registering pack. A `Deny` from a pack
policy is treated identically to a base-gate `Deny`: dispatch aborts with
`RuntimeError::PermissionDenied`. A pack policy error (infrastructure failure)
follows the same fail-open rule as the base gate.

## Rationale

### Why hard enforcement from day 1?

The previous design (ADR-029) shipped advisory enforcement in v0.2 and deferred hard
enforcement to v0.3. The rationale was de-risking: validate the trait shape, generate
audit data, allow reversal without breaking the contract.

In an architectural rewrite we get to skip the advisory phase. The trait shape is
settled, the audit shape is settled, the storage path is settled. Shipping advisory
mode would mean another phase of "we'll enforce later" — and "later" has a way of not
arriving.

Hard enforcement is the honest behavior: if a policy says Deny, we deny. The
`AllowAllGate` default preserves personal-local behavior; deployments that
configure a policy backend get the policy they configured. No magic.

### Why fail-open on gate Err?

A misconfigured Rego policy is an operations problem, not a security policy. If the
operator's policy file has a syntax error, the gate fails to evaluate — but that's
the operator's bug, not an authorization decision. Blocking dispatch would take down
the entire server until the operator fixes the policy.

Fail-open at the infra layer keeps khive serving during a misconfiguration. The
warning logs surface the bug; the operator fixes it; the gate goes back to evaluating
correctly. No global outage from one bad Rego file.

Deployments that prefer strict mode wrap `Gate` in a `StrictGate { inner: Arc<dyn
Gate> }` adapter that converts `Err` to `Deny`. The choice belongs to the operator.

### Why two audit sinks (tracing + EventStore)?

Tracing is the universal observability protocol. Log aggregators already consume
structured log fields; no extra infrastructure needed. Audit events fire through
tracing even when the EventStore is not configured (personal-local deployments).

EventStore persistence is for deployments that need queryable, persistent audit logs
— incident retrospectives, "what happened on day X" investigations, and any auditability
requirement. SQL/GQL queries over the `events` table answer those questions directly.

Both sinks are independent. Neither replaces the other.

### Why Option B (narrow capability) over Option A (full runtime)?

Coupling the registry to a full `KhiveRuntime` would drag every runtime capability
(storage, query, embedding, gate, scoring, retrieval) into the registry's dependency
surface. That's wrong: the registry needs exactly one storage capability (event
append) for audit persistence.

Option B (`Arc<dyn EventStore>` on the builder) is proportional: one capability per
responsibility. The registry stays narrow. The MCP server has both runtime and
registry in scope, so threading one to the other is one line of setup code.

### Why audit storage failures don't propagate?

A failed audit write is degraded observability. A propagated error would make verb
dispatch fail because the audit log was full — wrong cause-and-effect. The verb
dispatch outcome (Allow → proceed; Deny → reject) reflects policy. The audit write
is observability about that decision, not part of the decision itself.

This is the standard split between control plane (the decision) and observability
plane (the record of the decision). Failures in observability don't affect control.

### Why Rego as the canonical policy backend?

OPA (Open Policy Agent) is the established cross-industry policy language. Rego
policies are used in Kubernetes admission, Envoy auth, Istio, AWS Cedar (near-peer),
and dozens of other systems. The `regorus` Rust engine (Microsoft, MIT/Apache-2.0) is
maintained and production-ready.

Policies authored against khive's `RegoGate` are portable to any OPA consumer.
Operators with existing OPA expertise can apply it directly. Designing a custom DSL
would re-litigate every decision OPA already made.

### Why `Gate::impl_name`?

When multiple gates wrap each other (e.g., `OuterGate<RegoGate>`), the audit event
needs to identify which gate produced the decision. `impl_name` is a stable string
per gate type. Default returns the Rust type name; specific impls override for
clarity.

This is observability metadata, not part of the policy contract. Policy authors don't
care; alert pipelines do.

## Alternatives Considered

| Alternative                                         | Why rejected                                                                       |
| --------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Advisory mode (log, don't block)                    | Sets up "we'll enforce later" trap; rewrite gets to skip this phase.               |
| Homegrown policy DSL                                | Re-invents OPA; non-portable policies.                                             |
| Trait in `khive-types`                              | `khive-types` is substrate-narrow data types; trait is a runtime concern.          |
| Feature flag inside `khive-gate` for Rego           | Pulls regorus into every consumer's dep tree.                                      |
| `KhiveRuntime` on `VerbRegistry` (Option A)         | Couples registry to entire runtime stack.                                          |
| Fail-closed on gate Err                             | Takes down server when policy file has syntax error.                               |
| Single sink (tracing only or EventStore only)       | Loses one audience: aggregators need tracing; compliance queries need persistence. |
| Async `Gate` trait                                  | None of the known impls need async; would force `.await` on dispatch hot path.     |
| Block dispatch on audit storage failure             | Conflates control plane and observability.                                         |
| Coercing empty namespace at gate layer              | Creates authorization blind spot on declared-public field.                         |
| Static policy embedded in binary                    | Operators need to update policies without rebuilding khive.                        |
| Reuse `GateDecision` as a field inside `AuditEvent` | Double-tagged JSON shape; cleaner with separate `AuditDecision` enum.              |

## Consequences

### Positive

- Pluggable policy engine. Deployments configure Rego, capability-based, OAuth-scope,
  or custom backends without modifying khive core.
- Default is zero-config (`AllowAllGate`). Personal-local users see no behavior
  change.
- Hard enforcement from day 1. Deny means deny; no "we'll enforce later" debt.
- Structured audit trail. Tracing for log aggregators, EventStore for persistent
  queries. Both formats locked.
- Fail-open on gate Err. Infra failures don't cascade into server outages.
- Narrow registry coupling (Option B). No transitive dependency explosion.
- Audit shape locked before deployment scales. No retroactive schema churn.
- Rego portability. Policies move between khive and any OPA consumer.

### Negative

- `RuntimeError::PermissionDenied { verb, reason }` is a new error variant. Existing
  exhaustive matches must add an arm.
  Mitigated: standard Rust enum-extension migration; tooling catches it at compile.
- `Mutex<regorus::Engine>` serializes Rego evaluations. Throughput is bounded by
  single-threaded eval.
  Mitigated: bench-driven; switch to compiled-policy or engine pool when contention
  becomes measurable.
- `AllowAllGate` is a footgun in multi-user environments. The default exists for
  personal use, but operators who forget to configure a policy in a networked deployment
  have no protection.
  Mitigated: documentation emphasis; future "strict mode" config flag that requires
  an explicit non-default gate.
- `Obligation::RateLimit` is declared but not enforced in v1. Policy authors who
  return rate-limit obligations get audit visibility but no actual rate limiting.
  Mitigated: shape is locked so future rate-limiter ADR can wire enforcement without
  breaking policies.

### Neutral

- Tracing emission costs one `serde_json::to_string` per dispatch (~5 µs measured).
  Not measurable against storage I/O that follows.
- `AuditEvent` adds ~80 LOC to `khive-gate` for a public-contract type. Reasonable.
- The two-sink design means operators choose which they want. Both fire when both are
  configured.
- Personal-local use is unchanged: `AllowAllGate` allows everything, audit fires
  through tracing, no `EventStore` configuration needed.

## Implementation

- `crates/khive-gate/src/lib.rs`:
  - `Gate` trait + `GateRequest`, `GateDecision`, `Obligation`, `GateError`.
  - `AllowAllGate` default impl.
  - `AuditEvent`, `AuditDecision`, `AuditEvent::from_check`.
- `crates/khive-gate-rego/src/lib.rs`:
  - `RegoGate` (Apache-2.0, depends on `regorus`).
  - `DEFAULT_ENTRYPOINT = "data.khive.gate.decision"`.
  - `from_policy_str` / `from_dir` / `with_entrypoint`.
- `crates/khive-runtime/src/pack.rs`:
  - `VerbRegistry::dispatch` — gate consultation, audit emission, hard enforcement.
  - `VerbRegistryBuilder::with_gate` / `with_event_store`.
- `crates/khive-runtime/src/error.rs`:
  - `RuntimeError::PermissionDenied { verb, reason }` variant.
- `crates/khive-runtime/src/lib.rs`:
  - Re-exports `Gate`, `GateDecision`, `Obligation`, `AuditEvent`, `AuditDecision`.
- `crates/kkernel/src/server.rs` (or wherever MCP setup lives):
  - Wires `event_store` from runtime into the registry builder.

## References

- ADR-003: System Architecture — gate enforcement is the agent-binary boundary;
  operator binary (`kkernel`) runs `AllowAllGate`.
- ADR-004: Substrate Observables — `Event` substrate stores audit records.
- ADR-005: Storage Capability Traits — `EventStore::append_event` is the persistence
  primitive.
- ADR-007: Namespace — `GateRequest.namespace` reflects what the runtime sees; no
  coercion drift.
- ADR-016: Request DSL — dispatch path that consults the gate per parsed op.
- ADR-017: Pack Standard — `VerbRegistry` is the dispatch site where the gate fires.
- Open Policy Agent: https://www.openpolicyagent.org/
- Rego policy language: https://www.openpolicyagent.org/docs/latest/policy-language/
- `regorus` Rust engine: https://github.com/microsoft/regorus

---

## Amendment 1 (2026-07-07) — Canonical Verb Identity in `GateRequest`

### Context

`GateRequest` is constructed from the raw wire verb at the dispatch site
(`crates/khive-runtime/src/pack.rs`), before the registry resolves which pack owns
that verb. The wire surface accepts bare aliases for some verbs (for example, bare
`create` for `kg.create`) alongside fully-qualified `pack.verb` forms. A policy
authored against the canonical `pack.verb` id — for example, a rule denying
`kg.delete` — has no defined relationship to a wire call using the bare alias,
because nothing canonicalizes the verb before it reaches the gate. The
canonicalization step does not exist on the pre-gate path today.

Any policy layer built on top of `GateRequest.verb` inherits this gap as an
alias-bypass vulnerability: a deny rule written against the canonical id can be
missed by a request using the alias form.

### Decision

1. A single, registry-backed canonicalization function resolves a wire verb to its
   registered `pack.verb` id. This function is shared by exactly four call sites:
   dispatch, the gate, `verbs()`, and metering. No call site maintains its own
   alias-resolution logic.
2. Pack-ownership resolution moves ahead of the gate check in the dispatch path, so
   the canonical id is available when `GateRequest` is constructed.
3. `GateRequest.verb` carries the canonical `pack.verb` id. The original wire alias,
   if one was used, is recorded separately as an audit-only field — it is never the
   value policy rules match against.
4. A wire verb that does not resolve to any registered canonical id fails **closed**:
   the request is denied before dispatch, not passed through un-normalized to the
   pack layer or to the gate.

### Consequences

- Every policy that names a canonical `pack.verb` id is enforced consistently
  regardless of which wire form the caller used.
- Dispatch, gate, `verbs()`, and metering cannot drift into disagreement about what
  a given wire call resolves to, because they share one resolver.
- Pack-ownership resolution runs on every dispatch, including calls that would have
  been allowed under `AllowAllGate`; this is a small fixed cost of a lookup class the
  dispatch path already pays elsewhere.
- Policies authored against alias forms (if any) must be re-authored against
  canonical ids: a one-time migration cost at adoption.
