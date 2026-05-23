# ADR-029: Authorization Gate — Pluggable Policy Trait

**Status**: accepted (enforcement now hard, per ADR-035)\
**Date**: 2026-05-18\
**Authors**: Ocean, lambda:khive

## Context

khive-oss through v0.2 has no authorization layer. Whoever has stdio access to `khive-mcp` can
invoke any verb on any namespace. This is fine for a personal local server but inadequate for:

1. **Multi-tenant deployments** — a single khive process serving more than one user or agent.
2. **Compliance-bound workloads** — SOC2, FedRAMP, financial — where "who did what" must be
   auditable and gated by policy.

Design forces:

- **License boundary.** khive is Apache-2.0. Any downstream impl with a different license must
  not be required by the OSS crate. We need a trait in OSS and impls downstream.
- **Policy language.** Rego (Open Policy Agent) is the established cross-industry policy
  language; a Rust engine exists (`regorus`). Standardizing on Rego makes policies portable
  across deployments.

This ADR introduces a pluggable gate trait + permissive default, leaving impl bodies to
downstream crates per their licensing.

## Decision

### Trait + default impl in `khive-gate` (Apache-2.0)

A new crate `khive-gate` defines:

```rust
pub trait Gate: Send + Sync + std::fmt::Debug {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError>;
}

pub struct GateRequest {
    pub actor:     ActorRef,            // { kind, id }
    pub namespace: Namespace,           // khive_types::Namespace
    pub verb:      String,
    pub args:      serde_json::Value,
    pub context:   GateContext,         // session id, timestamp, source
}

pub enum GateDecision {
    Allow { obligations: Vec<Obligation> },
    Deny  { reason: String },
}

pub enum Obligation {
    Audit     { tag: String },
    RateLimit { window_secs: u64, max: u32 },
    Custom    { value: serde_json::Value },  // arbitrary JSON; struct-like shape
                                             // is required because the enum is
                                             // internally tagged on `kind` —
                                             // serde cannot merge the tag into
                                             // a non-object newtype payload
}
```

The JSON projection of `GateRequest` is the **public contract** — it is the input shape policies
(Rego or otherwise) receive. Changing field names is a breaking change and requires a new ADR.

### Default: `AllowAllGate`

Ships in `khive-gate`. Returns `Allow { obligations: [] }` for every request. This is the
runtime default — `RuntimeConfig::default()` sets `gate: Arc::new(AllowAllGate)`. Zero behavior
change for current personal/local users.

### Runtime wiring

`RuntimeConfig` gains a `gate: GateRef = Arc<dyn Gate>` field. The runtime consults it before
dispatching each verb at the single dispatch site established by
[ADR-027](ADR-027-single-tool-mcp-surface.md). In v0.2 the gate is **advisory** — the dispatcher
logs deny reasons but does not yet block. v0.3 makes the gate authoritative (deny → error).

### Three impl paths (only the first ships in this ADR)

| Impl                        | Crate             | License    | Status             |
| --------------------------- | ----------------- | ---------- | ------------------ |
| `AllowAllGate`              | `khive-gate`      | Apache-2.0 | shipped (this ADR) |
| `RegoGate` (regorus-backed) | `khive-gate-rego` | Apache-2.0 | shipped (ADR-032)  |

Each impl ships as a sibling crate so consumers opt in by adding the dep rather than toggling a
feature flag. Same shape as the `khive-pack-*` series. Downstream consumers can implement the
trait against their own policy engine without modifying OSS.

### Obligations are advisory in v0.2

`Obligation::Audit` and `Obligation::RateLimit` are returned with `Allow` but the dispatcher is
not required to enforce them. They establish the shape so policy authors can write
`allow { ... obligations: [{kind: "audit", tag: "search"}] }` today; we enforce later when the
audit / rate-limit subsystems land.

## Rationale

### Why a trait, not a config-driven engine

The three concrete impls have different shapes — `AllowAllGate` has no state, `RegoGate` carries
a regorus engine + policies, `LionGate` carries a capability manager. A trait lets each present
the same surface while keeping internals private. A config-driven engine would force a union
type that knows about every backend.

### Why Rego, not a homegrown policy DSL

Rego is the cross-industry standard (Kubernetes admission, Envoy, Istio; AWS Cedar is a
near-peer). The Rust engine (`regorus`) is maintained by Microsoft. Policy authors can move
policies between khive and other Rego consumers. Designing a custom DSL would re-litigate every
decision OPA already made.

### Why JSON shape of `GateRequest` is the contract

Policies (Rego or otherwise) receive a structured input. Once external policies are written
against `input.actor`, `input.verb`, `input.args`, those field names are de facto API. Locking
the shape in this ADR makes it explicit.

### Why advisory in v0.2

Wiring the check is the change that touches every dispatch site. Making it advisory first lets
us:

- Verify the trait shape against downstream consumers.
- Generate audit data for policy authors to write against.
- Reverse course in v0.3 without churning the trait shape.

A hard cutover from no-gate to enforce-gate would risk locking out personal users who never
wrote a policy.

### Why a sync check (not async)

`AllowAllGate` is sync. `RegoGate` (regorus eval) is sync. `LionGate` capability check is sync.
The three known impls don't need async. Making the trait sync keeps the dispatch hot path free
of `.await` and lets callers use the gate from non-async contexts (CLI, FFI). If an async-only
backend emerges, we can add a sibling `AsyncGate` trait — but speculatively widening the surface
today is the wrong default.

## Alternatives Considered

| Alternative                             | Pros                            | Cons                                               | Why rejected                                |
| --------------------------------------- | ------------------------------- | -------------------------------------------------- | ------------------------------------------- |
| Build homegrown policy DSL              | Full control over shape         | Re-invents OPA; non-portable policies              | Rego ecosystem too large to ignore          |
| Bake auth into existing namespace check | Smallest change                 | Couples auth to storage; no policy authoring story | Doesn't generalize beyond namespace scoping |
| Skip auth entirely in OSS               | Smallest surface                | Blocks production multi-tenant deployments         | Real users need this seam                   |
| Async-only `Gate` trait                 | Future-proof for async backends | Forces async-runtime onto sync callers (CLI, FFI)  | No known async impl; widen later if needed  |

## Consequences

### Positive

- khive can grow real authorization without forcing a single impl.
- Downstream consumers plug in their own gate impls via the trait.
- Rego compatibility means policies are portable across deployments.
- Audit obligations have a defined shape before the audit subsystem ships.
- The single-tool surface ([ADR-027](ADR-027-single-tool-mcp-surface.md)) makes gate wiring
  trivial — one dispatch site to consult the gate.

### Negative

- `AllowAllGate` is a footgun in a multi-user environment. Documentation must be loud: this is
  permissive on purpose for personal use.
- The trait locks in `GateRequest` JSON shape early. Field-name changes need a deprecation
  cycle.
- Gate is sync. Async policy backends would need to block or run an inner runtime. Acceptable
  for the known impls; revisit if an async-only backend emerges.

### Neutral

- v0.2 ships the trait + default only. Real enforcement (`RegoGate`, `LionGate` migration)
  lands in follow-up ADRs.
- `Obligation` is an open-ish enum (`Custom { value }` carries arbitrary JSON — objects, arrays,
  scalars, and null all round-trip). Real obligation types may earn dedicated variants later;
  `Custom` is the escape hatch.
- `GateRequest.namespace` reflects the operation's tenant: pulled from `params["namespace"]`
  when present (including an explicit empty string, which `KhiveRuntime::ns` also preserves),
  otherwise the registry's configured default namespace
  (`VerbRegistryBuilder::with_default_namespace`, populated by the MCP transport from
  `RuntimeConfig.default_namespace`). Gate-visible namespace and runtime-visible namespace
  MUST stay aligned — coercing an empty string at the gate layer while runtime continues to
  honor `""` would create an authorization/audit blind spot on the field ADR-029 declares
  public. Policy `input.namespace` is therefore authoritative even while gate decisions are
  advisory.

## Implementation Status

| Step                                               | Where                                         | Status             |
| -------------------------------------------------- | --------------------------------------------- | ------------------ |
| `khive-gate` crate: trait + types + `AllowAllGate` | `crates/khive-gate/`                          | done               |
| `RuntimeConfig::gate` field + `Default::default`   | `crates/khive-runtime/src/runtime.rs`         | done               |
| Re-export gate types from `khive-runtime`          | `crates/khive-runtime/src/lib.rs`             | done               |
| Dispatch-site gate consultation (advisory)         | `crates/khive-runtime/src/pack.rs` (registry) | done               |
| `khive-gate-rego` crate (`RegoGate`)               | `crates/khive-gate-rego/`                     | done (ADR-032)     |
| Audit envelope (`AuditEvent` via tracing)          | `crates/khive-gate/src/lib.rs` (type)         | accepted (ADR-033) |
| Hard enforcement (deny → dispatch error)           | `crates/khive-runtime/src/pack.rs`            | accepted (ADR-035) |

## Open Questions

1. **Dispatch-site placement.** The gate could be called in `VerbRegistry::dispatch` (one site,
   transport-agnostic) or in `khive-mcp`'s request handler (one site, transport-specific).
   Dispatch-site keeps non-MCP transports gated for free; transport-site keeps the registry
   transport-agnostic. Lean: dispatch-site. Resolve in the wiring PR.
2. **Audit envelope schema.** ~~Resolved by ADR-033 (AuditEvent type) and ADR-035 (storage
   shape: `SubstrateKind::Event` with the full `AuditEvent` serialized into `Event.data`).~~
   _Resolved: ADR-033 defines `AuditEvent`; ADR-035 specifies persistence as
   `SubstrateKind::Event` with the full `AuditEvent` JSON in `Event.data`._
3. **Multi-gate composition.** Should `LionGate<G>` chain — wrap another gate, both consulted?
   Or is single wrapping enough? Defer until a real second stack emerges.
4. **Anonymous actor semantics.** `AllowAllGate` accepts `ActorRef::anonymous()`. A real
   policy backend will need a convention for "unauthenticated local"; `kind="anonymous"` is
   load-bearing but not yet contract.

## References

- [ADR-007](ADR-007-namespace-as-open-string.md): Namespace model (gate scopes against namespace)
- [ADR-025](ADR-025-pack-standard.md): Pack standard (the verbs that get gated)
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single tool surface (where gate consultation
  lands)
- Open Policy Agent / Rego: <https://www.openpolicyagent.org/docs/latest/policy-language/>
- `regorus` Rust Rego engine: <https://github.com/microsoft/regorus>
