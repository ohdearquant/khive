# ADR-129: Fail-closed authorization gate default

- Status: accepted (2026-07-25)
- Date: 2026-07-25
- Amends: [ADR-018](ADR-018-authorization-gate.md), superseding its original
  `AllowAllGate` default and fail-open gate-error behaviour while leaving the
  remainder of that record in force
- Depends on: ADR-127 (authenticated actor and grant primitive), the capability
  substrate this design builds on — the published `lion-core` crate (crates.io,
  Apache-2.0) plus `khive-capability`
- Amended by: [ADR-143](ADR-143-store-held-caller-grants.md), which supersedes
  Amendment 2's configuration-text roster and "no runtime registration API"
  invariant with store-held caller grants and hierarchical subactor identity

> **Implementation status (2026-08-08):** This accepted staged design is not
> fully shipped. The current runtime default remains `AllowAllGate`, and the
> ADR-143 store-held caller-grant model has not been implemented. Because
> silently accepting Amendment 2's now-superseded `[gate]` roster would claim
> enforcement that does not exist, this build rejects every `[gate]` table at
> configuration load. This note records implementation state only; it does not
> change the accepted fail-closed decision or ADR-143's superseding design.

## Context

Before this ADR, the runtime's authorization gate was consulted before every
verb dispatch, but its default implementation was permissive:
`RuntimeConfig::default()` wired `AllowAllGate`, which allowed every request
with no obligations. That was the correct default while the enforcement layer
lived outside the substrate. That condition no longer holds:
the capability substrate (`lion-core` plus `khive-capability`, ADR-127) provides sealed
capability tokens, explicit-grant validation, and attenuating delegation.

A permissive default is a deny-list posture: everything is allowed except what
some outer layer refuses. The security model this substrate is built around is
the opposite — explicit granting, where absence of a grant is denial, enforced
through capability tokens as specified in the Lion microkernel, with
deployment-specific policy layered at the deployment edge.

Three facts about the current code shape the staging below; a decision that
ignored them would not be implementable as written:

1. **Two dispatch seams fail open today.**
   `dispatch_intercepted_with_identity` documents "gate errors fail open" and,
   on a `GateError`, logs and invokes the intercepted operation anyway
   (`crates/khive-runtime/src/pack.rs:1153-1206`); the ordinary dispatch path
   advertises the same contract in its public API doc and its error arm does
   the same (`crates/khive-runtime/src/pack.rs:1315-1319,1463-1468`). Both are
   masked by the permissive default — `AllowAllGate::check` cannot error — and
   become live authorization bypasses the moment any fallible gate is wired.
2. **No runtime `Gate` implementation exists over the capability layer.**
   `RuntimeConfig` accepts a `GateRef`; `khive-runtime` depends on neither
   capability crate, and `khive-capability`'s validator requires
   `(PluginId, CapId, namespace, Operation)` — two identifiers a
   `GateRequest` does not carry. An adapter with its own actor-to-capability
   registry is required, and is specified here rather than assumed.
3. **"Namespace" is three distinct config concepts** — `default_namespace`,
   the read-visibility set, and the outbound-delivery set
   (`crates/khive-runtime/src/config.rs:190-192,239-255`) — while a
   capability is scoped to exactly one namespace and validation rejects any
   non-identical target. A root grant must therefore be an enumerated set of
   single-namespace capabilities under an explicit authority rule.

## Decision

Staged. Stage 1 items are prerequisites; the default flip is Stage 2 and does
not ship until every Stage 1 item is merged and tested.

### Stage 1a — every dispatch seam fails closed

Every `Gate::check` call-site in `khive-runtime` treats `GateError` as a
refusal, never as permission to proceed. Both known seams change from their
documented fail-open behaviour to returning a typed gate-unavailable error
without invoking the operation: `dispatch_intercepted_with_identity` and the
ordinary `dispatch_with_identity` path. Every changed seam's `GateError`
documentation — doc comments and any prose contract that states fail-open
behaviour (`crates/khive-runtime/src/pack.rs:1315-1319` today) — changes in
the same commit as its behaviour; a seam whose code fails closed while its
public doc still promises fail-open is a defect of this stage. Denials and
gate-unavailable refusals remain distinguishable in errors and audit events.
A regression test per seam pins both that the dispatch closure is never
invoked after a gate error and the typed gate-unavailable result the caller
observes. This stage is a behaviour change only for gates that can error —
the permissive default cannot — so it is safe to land ahead of the flip.

### Stage 1b — a concrete `CapabilityGate`

`khive-capability` gains a `CapabilityGate` implementing the
`khive-gate::Gate` trait (the dependency points from `khive-capability` to
`khive-gate`; `khive-runtime` is untouched by this stage):

- **Ownership:** the gate owns its `CapabilityManager` behind a lock; the
  kernel holder table remains the in-process authority.
- **Principal registry:** the gate maintains the structured-`ActorRef` to
  (`PluginId`, held `CapId` per namespace) mapping. The registry key is the
  `ActorRef` itself, never a flattened `kind:id` rendering: the flattening is
  not injective when a component contains `:`, so two distinct actors could
  collide on one entry and inherit each other's capabilities. The flattened
  label is display-only, for audit and error text. Entries are created by
  the boot mint (Stage 1c) and by future grant flows; a request whose actor
  has no entry is denied before any capability lookup.
- **Operation classification:** the verb-to-`Operation` classification moves
  into `khive-capability` as the single substrate-owned table (exhaustive
  match, no wildcard arm). The gate maps `GateRequest.verb` through it to the
  required right.
- **Error discipline:** a failed capability validation is a `Deny` with the
  validator's reason; an infrastructure failure (poisoned lock, missing
  registry state) is a `GateError`, which Stage 1a turns into a refusal at
  every seam.
- **Seal precondition:** in-process capabilities are vouched for by the
  holder table. Any capability that was persisted, serialized, or received
  from another process must pass seal verification on load before entering
  the registry; one that cannot be verified is unavailable (deny). Root
  bootstrap minting stays in-process-only; boundary-crossing capabilities
  are minted through the sealing delegation path exclusively.

### Stage 1c — local boot mints an enumerated root grant

At composition-root boot (the local binary, not library construction), the
local process principal is registered and granted one capability per
namespace under this authority rule:

| Config source         | Namespaces            | Rights granted                           |
| --------------------- | --------------------- | ---------------------------------------- |
| `default_namespace`   | that one namespace    | full (read, create, write, delete, send) |
| read-visibility set   | each listed namespace | read only                                |
| outbound-delivery set | each listed namespace | send only                                |

The three sources overlap by design — config resolution folds the actor's
own namespace into the read-visibility set, and nothing stops a namespace
from appearing in all three (`crates/khive-runtime/src/config.rs:568-615`).
The table's rows are therefore inputs to a **normalization step, not the
minted set**: before minting, entries are deduplicated by namespace and the
rights for each namespace are the **union** of every source that lists it
(full absorbs the others). Exactly one capability per namespace exists after
normalization, which is what the principal registry stores. The audit log
records the normalized namespace-to-rights enumeration, not source rows.
Tests cover each pairwise source overlap, a namespace in all three sources,
and empty read-visibility / outbound-delivery sets.

The grant set is derived from configuration at boot, logged as that auditable
enumeration, and revocable at runtime. Boot-mint failure is a boot failure,
never a silent fallback to permissive. The single-user experience is
unchanged because the grants exist, not because the gate is bypassed.

**Construction ordering (the invariant Stage 2 depends on):** configuration
is fully resolved first; the composition root then creates one
`Arc<CapabilityGate>`, registers the resolved local principal, and mints the
normalized grant set into that same instance; only then is the runtime
constructed carrying that gate reference. Mint and dispatch share one gate
instance by construction — there is no path on which the runtime holds a
different gate than the one minted into.

### Stage 2 — the default flips

`RuntimeConfig::default()` wires an **unprovisioned** `CapabilityGate`
instead of `AllowAllGate`. This requires `khive-runtime` to depend on
`khive-capability`; that dependency edge is part of this stage and lands with
it. `AllowAllGate` remains available for tests and embedders that opt out,
selected explicitly in code — never the silent default.

This stage supersedes ADR-018's original `AllowAllGate` default, its related
personal-local and operator-mode consequence text, and its claim that migration
would be compile-guided. ADR-018's `Gate` contract, hard-deny semantics, audit
shape, and policy-extension decisions remain active.

An unprovisioned gate has an empty principal registry and denies everything:
that is explicit-grant semantics, not a defect. The Stage 1c boot path is
what provisions it — the local binary resolves config, mints into the gate
instance the config carries, and only then serves (the ordering invariant in
Stage 1c). Library callers constructing `RuntimeConfig::default()` or
`KhiveRuntime::{new, from_backend}` directly, without running the boot mint,
get a deny-all runtime **intentionally**; their choices are the explicit
`AllowAllGate` opt-out or performing the mint themselves through the same
grant API the boot path uses. An end-to-end regression test pins both sides:
a boot-minted default runtime serves the default-namespace request, and a
directly constructed unminted runtime refuses it.

**This flip is a silent behaviour change for existing `Default` users, and
that is its point.** The `gate` field is defaulted, so construction sites
compile unchanged and begin enforcing at runtime. The release notes for the
tag that ships Stage 2 must say exactly that, and name the explicit
`AllowAllGate` opt-out. The earlier claim that migration is compile-guided
is wrong and is not made here.

### Deployment policy stays at the deployment edge

The substrate gate answers capability questions only: may this principal
perform this operation on this namespace. Tenant-level policy — access
entitlements, account state, quota — is the deployment's own gate, composed
after the capability check. The substrate never learns tenant account state;
the deployment never re-derives verb-to-right classification.

## Consequences

- Stage 1a fixes a latent authorization bypass independently of the flip: a
  fail-open dispatch seam guarded only by the fact that the then-default gate
  could not error.
- Every embedder constructing `RuntimeConfig` without an explicit gate gets
  enforcement at Stage 2, silently at runtime. Embedders needing permissive
  behaviour state it in code, which makes every permissive runtime findable
  by search.
- The boot path gains a deterministic, enumerated capability-mint step whose
  failure is a boot failure.
- The verb-to-right classification gains a single substrate-owned home;
  downstream deployments consume it rather than maintaining a parallel table.
- A future revocation or re-keying epoch invalidates outstanding capabilities
  without a restart, because validation consults live kernel state rather
  than a boot-time snapshot.

## Amendment 1 — pseudo-verb classification (accepted 2026-07-25)

Stage 2 implementation surfaced a gap in the Stage 1b classification table's
coverage. The table is exhaustive over **registered verbs**, but the runtime
also consults the gate with **pseudo-verbs**: synthetic verb strings that never
pass through the verb registry. The concrete instance is `"authorize"` —
`KhiveRuntime::authorize` and `authorize_with_visibility` build a
`GateRequest` with that verb when minting a `NamespaceToken` for direct store
access. Because classification is resolved before the principal lookup, an
unclassified pseudo-verb is denied identically by an unprovisioned gate and a
fully minted one; no grant can reach it. Left unresolved, the Stage 2 flip
would deny every `authorize`-mediated path even after a successful boot mint.

### The invariant

**A pseudo-verb is checked against the full authority its result GRANTS, not
the right its caller happens to need in the moment.** Two clauses, both
normative:

1. **Right selection.** The classification is the strongest right the
   resulting authority carries. Checking below the granted authority lets a
   weakly-entitled principal mint a stronger capability (fail-open one level
   down); checking above it refuses callers the grant would have served,
   which is the fail-closed direction and therefore the acceptable error
   side.
2. **Namespace enumeration.** Granted authority may span namespaces. The
   check enumerates **every (namespace, right) pair the result grants** and
   requires all of them; a single unsatisfied pair denies the whole
   operation. A gate request that carries one namespace while the result
   grants authority in several is the same fail-open defect as clause 1,
   moved sideways.

Any future pseudo-verb resolves under these two clauses without a new
decision.

### Application to `authorize`

`KhiveRuntime::authorize` mints a token carrying read **and** write authority
for its primary namespace: under clause 1 it is checked against the **Write**
right for that namespace.

`authorize_with_visibility` additionally grants **read** authority over every
caller-supplied extra visible namespace. Under clause 2 the mint requires
**Write on the primary namespace AND Read on each extra visible namespace**,
each checked against the gate before the token exists; any failing component
denies the whole mint — there are no partial tokens. Checking only the
primary would let a principal with no read grant on a namespace mint a token
that reads it.

Consequences, stated so they are chosen rather than discovered:

- Single-user deployments are unchanged: the boot mint grants the local
  principal full rights on its enumerated namespaces, so `authorize` passes
  wherever dispatch already passes.
- A multi-actor principal holding read-only rights can no longer mint a
  namespace token at all. That is the honest semantics of a read+write token,
  not a regression; a read-scoped token variant, if ever needed, is a new
  pseudo-verb classified to Read under the same invariant.
- Handler-internal `authorize` calls run under the already-gated dispatch
  principal (no authority elevation), so minted runtimes serve normally.

### Direct-store composition roots must mint (pending requirement)

This section states a REQUIREMENT on the Stage 2 implementation, not a current
fact: at the time this amendment is accepted, the paths below construct their
runtime from the default configuration and do not mint.

The dispatch-site roots (exec, MCP serve, the coordinator daemon) mint at boot
as of Stage 2's implementation branch. The Stage 2 change that ships the flip
MUST also wire the same mint-and-keep seam into the remaining production paths
that reach the gate only through `authorize` — the KG archive/status commands,
the reindexer, and the VCS sync path — each a serving composition root in its
own right, before constructing its runtime.

**Acceptance condition for the flip:** every named root serves minted and
refuses unminted, demonstrated by executed tests; none is exempted with a
permissive gate. The flip does not ship while any named root would deny under
a completed boot.

### Test obligation

The pinned test asserting that a minted gate denies `authorize`
(`minted_gate_still_denies_the_authorize_pseudo_verb_pending_classification`)
is retired by the change that classifies it, replaced by assertions covering
every clause of the invariant:

- a minted gate serves `authorize` for a principal granted Write on the
  primary namespace, and denies it for a principal granted Read only;
- `authorize_with_visibility` is DENIED when the principal lacks Read on any
  requested extra visible namespace, even with Write on the primary — the
  cross-namespace fail-open case this amendment closes;
- `authorize_with_visibility` serves when Write-on-primary and Read-on-every-
  extra all hold.

## Amendment 2 — caller-principal provisioning for shared serving processes (accepted 2026-07-26)

### The gap

Stage 1c provisions exactly one principal: the serving process's own resolved
identity. That is complete for a single-caller process and structurally
incomplete for a shared serving process that dispatches on behalf of
per-request identities. Such a process presents the request's actor to the
gate, and Stage 1c's own contract is explicit that a dispatch supplying its
own identity "holds only what was separately granted — that is a denial, not
an inheritance." No mechanism to separately grant existed: principal
registration is reachable only from the boot mint, by design.

Under the Stage 2 default this is a total denial of service for every caller
whose identity differs from the process's boot identity, on every verb, read
and write alike. The failure is invisible from inside the boot identity: the
one caller that can run a post-deploy check is the one caller the gate
admits. Verification of a multi-caller deployment therefore requires a probe
from a second identity (see acceptance below).

### Decision

Configuration enumerates the caller principals a serving process provisions
at boot, in a `[gate]` section:

```toml
[gate]
granted_actors = ["<caller-id>", "..."]   # default: []
grant_unattributed = false                 # default: false
```

At boot, after the process principal's root grant is minted, the mint
iterates the deduplicated `granted_actors` list and mints **the same
normalized enumeration** (Stage 1c's authority table and normalization,
unchanged) for each listed caller. `grant_unattributed = true` additionally
provisions the anonymous fallback identity, preserving access for
identity-less local callers; it is an explicit deployment-posture opt-in,
never a default.

Uniform grants are deliberate. This amendment provisions _who may call_, not
_who may do what_: every enumerated caller receives the boot enumeration, and
per-caller differentiation (distinct namespace sets or rights per caller) is
a later stage with its own design record. Callers absent from the enumeration
remain denied — the fail-closed default is narrowed, not weakened.

Invariants carried forward unchanged:

- Mint failure is boot failure, for the caller list exactly as for the
  process principal. A malformed entry aborts boot; there is no
  partial-mint continue.
- One gate instance, minted at the boot seam, no outside-gate constructor,
  and no runtime registration API. The caller list is the only added input.
- The audit record logs one normalized enumeration per provisioned
  principal.

### Acceptance for any deployment serving multiple callers

A deployment of the Stage 2 default to a multi-caller topology is verified
only by both halves of a second-identity probe, executed against the serving
process:

1. a real verb dispatched under an enumerated non-boot identity returns
   success;
2. the same dispatch under a non-enumerated identity is denied with the
   distinguishable no-principal error.

A check run only under the boot identity — pack presence, version output, or
a verb probe as the process's own actor — cannot detect an unprovisioned
caller set and does not count as verification.

### Consequences

- The caller enumeration is deployment configuration, so the resolved config
  a shared process boots from must carry the full caller set regardless of
  which caller spawned it; deployments place the `[gate]` section in
  configuration common to all spawn paths.
- The no-principal denial remains a stable, machine-distinguishable error
  class, so automated callers can report "denied by gate" rather than
  misreporting their own work as failed. A zero-privilege "am I gated?"
  probe surface is follow-up work tracked separately.
