# ADR-127: Authenticated actor and grant primitive

Status: accepted 2026-07-25
Date: 2026-07-25
Supersedes: none
Amends: none. Resolves one item recorded as UNRESOLVED in ADR-125.
Depends on: ADR-007 (namespace as attribution; the Gate as the single enforcement seam),
ADR-125 (reserved property keys), ADR-039 (merge conflict semantics, referenced only for
the store-mutation adversary).
Dependency exception: ADR-125 remains Proposed. ADR-127 depends only on the reserved-key
write-seam machinery already shipped and recorded there; it does not accept ADR-125's
remaining unresolved policy choices by implication.
Amended by: [ADR-128](ADR-128-custody-party-pairs-and-slot-authenticity.md), which
extends the custody-home evaluation criterion with slot authenticity.

## Context

Two problems arrived from different directions and reduce to one root cause.

**The provenance problem.** `comm.forward` emits a block citing the original message's
author-side id, read from that record's `outbound_ref`. A generic caller can create a note
of a pack-owned kind carrying an arbitrary `outbound_ref`, so the citation attests nothing.
ADR-125 closed the _mutation_ half — those keys are refused on writes to an existing record
— and recorded the _creation_ half as explicitly unresolved, naming it the `OwnerOnly`
question: there is no way to say "only the owning pack may establish this key at creation,"
because the owning pack's handlers and a generic caller reach the store as the same thing.

**The authorization problem.** A consumer acting on a directive from another party has no way
to verify, at the moment of acting, that the directive is genuine — so a cautious consumer
round-trips to the sender to confirm it first. That round-trip is the standing cost of the
missing verification. Retiring it requires seven properties, each stated by the consumer with
the negative test that discriminates: sender authenticity, content integrity, non-replay,
authorization bound to a specific action, durable cross-namespace attribution, revocation
that lands before the action, and authorization policy distinct from identity.

**The single root cause: the caller's actor identity is ambient.** It is derived from where
the process runs — a config file in the working directory, or an environment variable — not
from anything the caller presents and the substrate verifies. Every downstream guarantee
inherits that. Possession of an actor's working directory is possession of its voice, and no
message-layer check can see the difference. A stamp derived this way is trustworthy against
a confused caller and not against a determined one.

Both problems are therefore one question: **what makes a principal claim verifiable at the
point the substrate acts on it.**

## Decision

Introduce two things, and keep them distinct because they answer different questions at
different moments.

### 1. Authenticated principal at the Gate

The dispatch boundary authenticates the caller from a presented credential, derives the
principal, classifies the **assurance level** of that derivation, and mints an
`AuthenticatedContext` whose constructor is private to dispatch. Config- or
environment-derived actor labels MAY select _which_ credential to present. They MAY NOT
establish a verified principal.

Verification returns an assurance-bearing result, never a boolean:

```
PASS { principal, assurance, proof_id }
REJECT { stable_reason }
UNAVAILABLE { stable_reason, retryability }
```

Two assurance classes ship:

- `ActorSignature` — an actor key signed the exact canonical bytes. Survives an untrusted
  store.
- `DaemonBearer` — the daemon accepted a bearer credential from its own table. Survives an
  unauthenticated client. Does not survive an untrusted store, and cannot be upgraded into
  the former retrospectively.

**The classes never collapse to the same result.** A policy for consequential actions
requires `ActorSignature`; unknown schemes, insufficient assurance, absent keys and
unavailable key status all reject.

### 2. Grant extends a capability kernel; it is not a new design

The capability substrate this design builds on is the published
[`lion-core`](https://crates.io/crates/lion-core) crate (Apache-2.0), with `khive-capability`
layering khive semantics over it. A grant is an extension of `lion-core`'s `Capability`, not a
parallel primitive. What it already provides is adopted rather than re-specified:

| Property                            | Provided by        | Mechanism                                                                                                    |
| ----------------------------------- | ------------------ | ------------------------------------------------------------------------------------------------------------ |
| Unforgeable by holders              | `lion-core`        | HMAC-SHA256 seal over a deterministically serialized payload; all fields `pub(crate)`, no public constructor |
| Delegation can only narrow          | `lion-core`        | `delegate_cap` intersects requested rights with the parent's, then seals the result                          |
| Transitive revocation               | `lion-core`        | `revoke_cap` BFS over the children index                                                                     |
| Key rotation with a grace window    | `lion-core`        | `verify_seal` tries current key, then previous                                                               |
| Absence of a grant is denial        | `khive-capability` | `validate` rejects on `!plugin_holds` before inspecting anything else                                        |
| New verbs cannot inherit permission | `khive-capability` | verb-to-right match is exhaustive with no wildcard arm; a new `Operation` fails to compile until classified  |

**A grant is a `Capability` plus the five fields below.** Nothing above is redesigned.

```
+ subject_descriptor_digest    -- binds to a canonical revision-bound descriptor (§4),
                                 not to lion-core's coarse ResourceId
+ not_before, expires_at       -- lion-core capabilities have no time bounds
+ state(open|consumed|revoked|expired), consumed_at, consumption_id
                              -- lion-core has `valid: bool`; there is no consumed state
                                 and no single-use semantics
+ audience                     -- evaluated at the Gate as an authenticated field
+ proof_envelope               -- carries an ActorSignature proof where one exists (below)
+ assurance_class              -- the class the grant was ISSUED under, derived at issuance
                                 and re-checked at consume
```

`assurance_class` is a field rather than something a consumer infers by parsing
`proof_envelope` internals. Consumer policy must be able to refuse a `DaemonBearer`-issued
grant without understanding envelope formats, and the audit record must show which assurance
the authority was minted under rather than only which one was presented. A consumer that has
to parse an envelope to learn this will eventually skip the parse.

`consume(grant_id, exact_descriptor)` performs proof verification, status and policy checks,
and the `open -> consumed` compare-and-set **in one transaction**. A separate `verify` is
informational and cannot authorize an action: authority comes only from consumption, which
closes the verify-then-act race. This is the single most important addition, because a
lion-core capability is _reusable authority_ by design and a consequential action needs
one-shot authority.

Two further deltas are deployment properties rather than record fields. Kernel state is
in-memory, so grants that must outlive a process or cross an actor need a durable store.

#### Precondition: the seal is not verified on the path as it stands

This is stated here, not only in the crate's documentation, because a consumer reads this
record and inherits its assumptions.

`khive-capability::validate` checks holder, liveness, namespace and rights. **It does not
call `verify_cap_seal`.** `bootstrap_root_capability` mints its root capability through
`insert_cap_raw` — documented in the kernel as bypassing kernel minting, for internal and
test use — with a zero tag.

That is sound _today_ for a specific and fragile reason: capability ids never leave the
process that minted them, so the kernel's in-memory holder table is the authority and the
seal is redundant. The holder table is what actually decides, and it cannot be forged by a
caller because a caller cannot reach it.

**A grant breaks that condition by definition.** It is persisted, it outlives the minting
process, and it is presented by a party the kernel did not mint it for. The holder table no
longer travels with it, so the seal becomes the only evidence. Therefore, binding on any
grant path:

1. Every load of a capability the loading process did not mint **must** verify the seal
   before the capability is treated as authority.
2. Grants **must** be minted through `delegate_cap`, never `insert_cap_raw`.
3. A seal that fails to verify is a `REJECT`; a seal that _cannot_ be verified — missing key
   epoch, unreachable key history — is `UNAVAILABLE`, and specifically not a pass.

Adopting the kernel without these three is the failure this record's rejected-alternative 2
describes, arriving by a different route: the API would look like verified capability
authority while the verification step that makes it so is absent.

#### Why the kernel cannot supply `ActorSignature`, and why that is the whole point

`lion-core` seals with **HMAC-SHA256**, and it has no asymmetric dependency —
`verify_seal(key, payload, tag)` takes the _same_ key as `seal_payload(key, payload)`.

A symmetric MAC proves that _a holder of the key_ produced the tag. Every party who can
verify can also mint. So the kernel can establish "the daemon minted this" and can never
establish "actor X authorized this" to anyone who is not the daemon.

That maps exactly onto the two assurance classes in §1, and it is why they cannot be allowed
to collapse:

- **`DaemonBearer` is what the existing kernel provides**, completely and with formal
  verification behind it. It survives an unauthenticated client. It does not survive an
  untrusted store, because the store's operator holds the verification key.
- **`ActorSignature` is not expressible in the current kernel at all.** It requires an
  asymmetric signature over the canonical descriptor bytes, made with a key the daemon
  cannot use to mint. Adding it is a new primitive, not a configuration of the old one.

The consequence for sequencing is concrete: adopting the kernel delivers the `DaemonBearer`
half immediately and delivers **none** of the `ActorSignature` half. Any claim that the
substrate "has capability tokens now" therefore says nothing about P1. The gap is
cryptographic, not procedural, so it cannot be closed by policy, review, or convention.

Grant ids are globally resolvable, consistent with ADR-007 Rule 2 — by-ID ops have no
namespace check at any layer. Audience and tenant policy are authenticated grant _fields_
evaluated at the Gate, never post-fetch namespace checks. **This is not namespace isolation
returning under a new name.**

### 3. Verification sits in two places, deliberately

- **At the Gate**: credential authentication, principal derivation, assurance
  classification, verb-level authorization, audience and tenant policy.
- **At the consequential action seam**: exact descriptor verification and atomic
  consumption, using the context the Gate minted.

This remains one authorization architecture. The dispatch boundary cannot know the final
canonical action subject at the start of an arbitrary pack operation, nor atomically join
consumption to the action; treating both as one check creates a check/use gap. The action
seam discharges an obligation the Gate issued — it is not a second ambient identity
resolver.

Packs that write through the store trait directly are unaffected and remain able to do so.
They are constrained by construction rather than by a new check: grant state is absent from
generic note properties, so raw note-store mutation cannot create authority, and handler
identity is daemon registry metadata rather than a caller-asserted value.

**Registry construction fails if a protected verb is registered without an armed
authenticator and grant service.** This is a boot invariant, not author discipline.

### 4. Action binding is a canonical revision-bound descriptor

A grant binds to a descriptor digest, not to a record id:

```
protocol_version, action_type, target_kind + target_id,
target_revision_digest, canonical_parameters_digest,
audience, policy_class, lane_or_session_epoch, declared_constraints
```

Verification is mechanical equality of the digest plus signature coverage. Any target
revision or parameter change requires a new grant.

An id is strict only if it denotes immutable bytes; for a mutable record, id-equality
accepts a revised subject under an old grant. Binding to a freshly generated revision id
alone reverses the problem and rewards coarse parent subjects.

**Coarsening is resisted by schema, not by judgment.** Each `action_type` has a closed
descriptor schema with required material fields, no wildcards, bounded collection sizes and
a maximum policy class. A grant cannot say "do anything with project X" unless a separately
named high-risk action type defines that scope and recipient policy permits it. Semantic
judgment moves to reviewed schema design once, instead of runtime interpretation on every
request.

## Key custody is the load-bearing constraint

A signing scheme delivers two different properties that are routinely collapsed:

1. evidence that stored bytes were not mutated after signing;
2. evidence that a specific principal produced them.

Property 1 survives an untrusted store. **Property 2 survives only if the private key is
unreachable by the parties being distinguished.** Where every party can read every key, the
scheme looks identical — same algorithm, same envelope, same `PASS` — and the entire
difference lives in filesystem permissions, which no test of the cryptography surfaces.

Therefore: **for any authentication design, name which parties it distinguishes, then ask
whether those parties can read each other's credential material.** If they can, the
authenticity claim is decorative between them regardless of key length.

### The OS keychain was evaluated as a custody home and is not sufficient alone

The OS keychain is the obvious existing custody home for a secret injected at process
launch, so it was evaluated rather than passed over in favour of inventing a new store, and
it was **measured** rather than assumed.

Result: the item is readable **non-interactively**, with no prompt, by a process running as
the host user, and every actor runs as that same user. A keychain item therefore
protects against a _different_ OS user and against a stolen disk image. It does **not**
separate one actor from another, because actor identity has no representation at the OS layer
that the keychain can key an ACL against — keychain ACLs are per-application, and the actors
are the same application.

This does not disqualify the keychain; it disqualifies the keychain **as the whole answer**.
Actor separation requires either distinct OS principals per actor, or custody in a component
that authenticates the actor by something other than its ability to open a file.

### Per-workspace agent configuration is a distribution mechanism, not a boundary

The obvious alternative is per-workspace agent configuration: give each actor's directory its
own config file carrying its own credential. This was evaluated as the second candidate.

It is the correct **distribution** mechanism, and better than storing a literal secret,
because the config format supports indirect references (`${VAR}`, `${VAR:-default}`) so the
credential need not be written into the file at all.

It is **not** a custody boundary, and the agent platform's own documented threat model says
so: the working-directory constraint is a _write_ boundary rather than an isolation
boundary; per-directory configuration is organisational; and isolation between concurrent
sessions of the same OS user is out of scope, belonging to the OS layer. The stated security
model assumes all processes running as one OS user are equally trusted, because that is what
the OS enforces.

Three independent lines therefore agree: a measurement of keychain retrieval, a measurement
of config-file readability, and the platform's stated scope. **On a single OS user there is
no actor separation available at the credential-store layer, the configuration layer, or the
agent-platform layer.** The adversary is not exotic — an actor here can run arbitrary shell
commands, so "can actor A read actor B's key" reduces to "can actor A run a file read."

Actor separation is therefore an OS-layer problem, and the remedy is **topological rather
than architectural**. When a deployment places actors on distinct hosts or remotes,
separation is a property of the deployment
and needs no mechanism from this record. A distinct host is a distinct OS principal with a
distinct filesystem, which is the same custody property the hosted profile obtains for a
different reason.

This is a decision about what NOT to build. A broker that authenticates an actor by something
other than file access, or per-actor sandboxing on one host, would be infrastructure for a
configuration with an expiry date. **Do not build it.** Let the topology close the gap, and
record the state honestly until it does.

The profile axis in the table below is therefore not "local versus hosted." The
load-bearing variable is **whether the parties being distinguished share an OS principal**.
Co-tenancy is the condition that defeats P1; separation is what restores it, whether by
moving an actor to its own machine or by tenant separation in a hosted deployment.

Consequently: **in the local deployment profile, P1 against a malicious peer actor is
NOT-DELIVERED**, and it is recorded as such in the table below rather than in prose a reader
can skip.

## When disposition suffices, and precisely what ends that

The substrate does not prevent one actor from claiming another's identity. A deployment
whose actors all share a single trust boundary can operate without that enforcement, so
long as no actor within the boundary is inclined to fabricate an identity.

**That safety is a property of the population, not of the protocol.** It is real,
and it is not a guarantee, because nothing in the substrate produces it and nothing in the
substrate would notice its absence.

**The trigger is a change in trust boundary, not scale.** More actors that share the same
trust assumptions change nothing. The exposure begins the moment an actor identity is held by
a party that cannot be presumed to share that trust — an expected consequence of a deployment
becoming more heterogeneous over time. **A protocol must not depend on courtesy from either
side.**

So the activation condition for this record is stated as a population event rather than a
date or an actor count:

> **When the first actor identity is held by a party outside the deployment's trust boundary, the
> properties in the table below stop being cushioned by disposition and must be carried by
> mechanism.**

### The wider exposure: norms that are load-bearing but unenforced

Identity is the most formalisable of the deployment's conventions and it is not the only one
doing real work. Several disciplines currently prevent real errors and are enforced by
nothing but inclination:

- a not-found-by-method may not be upgraded to an absence by anyone downstream of the search;
- an attributed claim carries its originating id or is unresolved;
- a check that cannot fail loudly is not a check;
- unavailable is reported as unavailable, never as an absence of objection.

Each is honoured only because the actors are disposed to honour it. All of them degrade at
the same moment identity does, and a population that follows the letter without the spirit
is the harder case, because its output still looks conformant.

This record does not attempt to mechanise the others. It names them so the scope of the
transition is not understated: **heterogeneity does not merely introduce an impostor risk,
it withdraws the epistemic discipline such a deployment uses to catch its own errors.**
Deciding which of those norms must become mechanisms before the population changes is
separate work, and it should not wait for the first actor outside the trust boundary to be
scheduled.

## Disposition table

Read as a triple: a property is shipped **per assurance class per deployment profile**. A
clause of the consuming actor's protocol may be retired only against a matching triple. A
global "authentication enabled" flag is not a valid basis for retiring anything.

Profiles:

- **Local** — single host, actors share an OS user, store and any key material readable by
  every actor.
- **Hosted** — tenant-separated key custody and policy administration; store not
  readable across tenants.

| Property                               | Assurance required                                         | Local                                                                   | Hosted                   | Holds against / does not hold against                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| -------------------------------------- | ---------------------------------------------------------- | ----------------------------------------------------------------------- | ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| P1 sender authenticity                 | `ActorSignature`                                           | **NOT-DELIVERED vs peer actor**; delivered vs an unauthenticated client | Shipped                  | Holds: a caller forging another actor without that actor's key; a store-only attacker. Does not hold: key theft, a malicious actor, any component that can read the key. **Local fails on custody, not on cryptography.** `DaemonBearer` never carries this label.                                                                                                                                                                                                                         |
| P2 content integrity                   | `ActorSignature`                                           | Shipped for signed bytes                                                | Shipped                  | Holds: byte or property mutation after signing. Does not hold: mutation before signing; execution built from fields outside the signed descriptor; a compromised signer; unsigned projections.                                                                                                                                                                                                                                                                                             |
| P3 non-replay                          | `ActorSignature` + atomic consume + epoch                  | Shipped                                                                 | Shipped                  | Holds: message replay, concurrent double-use, a genuine grant from a closed epoch. Does not hold: store rollback/restore without a monotonic epoch authority outside the snapshot; an administrator able to rewrite both state and epoch. **A signature alone does not deliver P3.**                                                                                                                                                                                                       |
| P4 action binding                      | `ActorSignature` + registered action schema                | Shipped                                                                 | Shipped                  | Holds: substitution of target revision, action type, parameters, audience or policy class. Does not hold: a deliberately coarse schema that policy approved; semantics omitted from the descriptor; execution using different bytes than were signed.                                                                                                                                                                                                                                      |
| P5 durable cross-namespace attribution | Global grant id + `ActorSignature`                         | Shipped for khive-origin                                                | Shipped for khive-origin | Holds: namespace change, transport loss, projection movement. Does not hold: an external-origin ruling with no signing bridge; unavailable key history. **Global lookup alone is attribution availability, not authenticity.**                                                                                                                                                                                                                                                             |
| P6 revocation before action            | Online authoritative consume, zero positive-validity cache | Shipped                                                                 | Shipped                  | Holds: revocation committed before consume's serialization point. Does not hold: revocation racing after that point; a partition where authority is unreachable (which is `UNAVAILABLE`, never a pass); already-executed actions. "Instant" means a transaction ordering, not wall-clock. **Shipped-Local is scoped to grants that exist**: a peer able to mint fresh grants after a revocation is a P1-local failure, not a P6 one — see the P1 row rather than re-deriving the boundary. |
| P7 authorization policy                | Gate policy + registered action                            | Deferred                                                                | Deferred                 | Reason for deferral is separability, not difficulty: the policy vocabulary needs its own design round and is meaningless before P1. Authentication is not P7.                                                                                                                                                                                                                                                                                                                              |

Two properties are worth stating plainly because a reader skimming for green cells will
otherwise mis-read them. **P1 is the property the whole design exists for, and it is the one
the local profile does not deliver.** **P7 is deferred**, so any protocol clause resting on
policy-at-the-boundary is retained in full.

## Consumers

A capability with no named consumer is debt. Each consumer below names the artifact that
will consume it and the exact triple it consumes against. **A consumer may retire a
protocol clause only against a triple this record marks Shipped for that consumer's actual
deployment profile** — not against the feature existing.

| Consumer                          | Artifact                                                                                                                                                      | Consumes against                  | Retires                                                                        | Status under Local                                                                                                                                                                                                               |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------- | ------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Confirm-back retirement           | The counterparty's confirm-back protocol clause, which today requires a human-readable acknowledgement because the substrate cannot establish who sent a mark | P1 + P3, `ActorSignature`, Hosted | The confirm-back round-trip on a coordinated substrate window                  | **Blocked.** P1-local is NOT-DELIVERED, so the clause is retained in full. Retiring it under Local would remove a working control and substitute a mechanism that does not hold against the peer actor it is protecting against. |
| `comm.forward`                    | The forwarding path and its Critical finding: a forwarded message's origin cannot be distinguished from the forwarder's assertion of it                       | P1 + P2 + P5, `ActorSignature`    | Origin-as-asserted-field, replaced by origin-as-verified-principal             | **Blocked**, and this is why that work stays parked rather than shipping behind a flag. Under Local the forwarder and the origin share a key, so a forwarded origin claim is exactly as strong as an unforwarded one.            |
| ADR-125 creation-half enforcement | The reserved-property-key enforcement at record creation, which currently trusts the writer to not set owner-established properties                           | P4 + P1, `ActorSignature`         | Writer-side discipline, replaced by a descriptor-bound check at the write seam | **Partial.** P4 is Shipped once schemas are registered, so the descriptor binding half is available under Local; the _identity_ half is not, so enforcement can prevent a mis-set property but cannot attribute a violation.     |
| Substrate windows generally       | Any go/mark handshake between two actors during a coordinated change                                                                                          | P1 + P3 + P6                      | Manual out-of-band confirmation                                                | **Blocked under Local**, for the same custody reason as row 1.                                                                                                                                                                   |

Every consumer under the Local profile is blocked on the same cell: **P1-local**. That is
not four independent problems; it is one custody problem with four names. If P1-local is
never delivered, the honest outcome is that this record ships the Hosted profile and
leaves the Local protocol clauses exactly as they are, which is the withdrawal trigger below.

## Failure semantics

Three outcomes, never two. `REJECT` and `UNAVAILABLE` are **identical in consequence** — no
action, no consumption — and **distinct in report**. `UNAVAILABLE` names its instrument and
its reason. An error is never presentable as an absence of objection, and there is no
boolean conversion anywhere on this path.

A failed message is **quarantined**, not delivered-and-marked and not merely refused at
send:

- Refused-at-send alone is insufficient: the evidence lands with the sender, who in the
  adversarial case is the attacker, so a prober gets feedback and the target gets none. It
  is a valid addition, never the mechanism.
- Delivered-and-marked is the dangerous option: it puts a failed message in the normal inbox
  differing from a genuine one by a field the recipient must remember to check, which is the
  human round-trip this ADR exists to retire, wearing a machine's clothes.

Quarantine carries four requirements:

1. A separate surface, not reachable by the verb that reads ordinary traffic; retrieval is a
   deliberate act.
2. **Visible** — a recipient can see _that_ something was quarantined without reading it. A
   silent quarantine is indistinguishable from no-attack and, worse, from an authentication
   layer rejecting genuine traffic while the deployment appears to have gone quiet.
3. The visible signal appears on a surface the recipient already reads, not one they must
   remember to check. Otherwise requirement 2 decays into requirement 3's failure by another
   route.
4. Empty-quarantine, quarantine-holding-rejections, and authority-unavailable are three
   visibly distinct states.

### Who enforces which failure

Sender authenticity is established **before delivery**: by the time a message reaches a
recipient, its attribution was stamped by the substrate and could not have been supplied by
the caller. Its failure semantics belong to the substrate.

Grant verification is a call the actor makes **at the moment of acting**, because revocation
and expiry are time-dependent and a pre-delivery check is a cached-validity window — which
P6 forbids. Its failure semantics belong to the caller, and the verb returns the three-way
outcome so the caller can enforce them.

The split is not a hedge. P6 and a pre-established authorization check are incompatible by
construction.

## Known false-PASS paths, fenced

Each is a way this design could report success while establishing nothing.

1. **Unarmed registry defaults to allow.** The live instance is `AllowAllGate`, wired by
   `RuntimeConfig::default` (`crates/khive-runtime/src/config.rs`, the `gate` field), which
   is what every actor runs today. Fix: protected-verb registration makes a missing or
   permissive authenticator a boot error, plus a startup canary that submits a forged
   principal and must receive `REJECT`. The default flip activates at that registration
   point, not as a global flip ahead of it — nothing is protected until a protected verb
   exists to protect.
2. **Ambient actor fills an authentication failure.** Fix: config actor selects a credential,
   never establishes a principal.
3. **Unknown signature scheme falls back to bearer.** Fix: the scheme is signed and
   domain-separated; unknown or insufficient assurance rejects.
4. **A missing key reads as unsigned legacy data.** Fix: protected records require a proof
   envelope; an unknown key is `REJECT`, a key-service timeout is `UNAVAILABLE`.
5. **A positive cache survives revocation.** Fix: consume performs an authoritative status
   check with no positive-validity window; an unreachable dependency is `UNAVAILABLE`.
6. **Verify then act.** Fix: authority comes only from atomic consume.
7. **Lane closure is absent from signed or authoritative state.** Fix: bind the epoch and
   consult authoritative closed state during consume.
8. **Canonicalization drift.** The displayed action differs from the signed bytes; duplicate
   keys resolve differently; optional fields are injected post-verification. Fix: one
   canonical encoder, reject duplicate and unknown fields, sign the exact digest execution
   uses.
9. **Direct store mutation produces a PASS-shaped row.** Fix: validity requires a signature
   plus authoritative state; a raw row never becomes valid by shape alone.
10. **Audit emitted before commit.** Fix: success evidence is transactionally coupled or
    explicitly provisional.
11. **The test observes the wrong seam.** A downstream attribution assertion passes while the
    Gate saw an anonymous principal. Fix: negative tests capture the Gate request, the
    derived context and the consume result — not a downstream field.
12. **Clock uncertainty becomes grace acceptance.** Fix: define the clock source and skew;
    outside the interval rejects, and an unavailable clock is `UNAVAILABLE`, never "not known
    expired."

## Verification

Negative-first. A scheme exercised only on genuine traffic is untested; the discriminating
case is always the forgery.

- Mutate one signed byte; substitute actor, audience, action, expiry, nonce and scheme;
  remove the key. Each must produce a typed rejection naming what failed.
- Repeat every case with the verifier unavailable and confirm the action consequence is
  identical to rejection while the report class differs.
- Concurrent double-consume permits exactly one success.
- Revoke racing consume resolves under a specified serialization order.
- A genuine grant from a closed epoch is rejected.
- Cross-namespace by-id resolution succeeds with no namespace authorization check.
- Boot mutation: remove the authenticator, the grant service, or an action schema, and
  startup must fail before any request is served.
- Changing any descriptor field changes the digest; unknown, duplicate or non-canonical
  fields reject; every consequential verb maps to exactly one registered action schema.
- A deployment attestation proves key custody matches the adversary the active profile
  claims.

## Unresolved

- **Custody home for the local profile.** Both evaluated candidates are negative for actor
  separation: the OS keychain (readable non-interactively by the shared user) and
  per-workspace agent configuration (organisational, with isolation explicitly out of the
  platform's scope). Remaining candidates are all OS-layer: distinct OS principals per actor,
  process sandboxing, or a broker authenticating an actor by something other than file access.
  **This is a gate blocker for claiming P1 locally, not an implementation detail.**
- **Crash and retry semantics** where the authorized action cannot commit in the same
  transaction as consumption. An execution receipt bound to the exact invocation is the
  proposed shape; its idempotency protocol is unspecified.
- **Restore and rollback replay.** Restoring an old store snapshot may resurrect an open
  grant unless epochs have a monotonic authority outside that snapshot.
- **Key history availability** for P5, including a distinction between "invalid at issuance"
  and "revoked later."
- **External-origin rulings** (P5): the proposed shape is a receipt-time mint that cites the
  external wire id, which makes the grant's authenticity only as strong as that channel's own
  authentication. That is a channel property, not a substrate one, and it must not be
  presented as substrate-delivered P5.

## Sequencing

Nothing in this record is urgent in a deployment where every actor shares the same trust
boundary. That condition typically ends by intent rather than by accident — broadening the
actor population is a choice — so the date can be chosen rather than discovered after the
fact.

The practical consequence is that the mechanism should exist before an actor outside the
trust boundary is scheduled rather than after it appears, because the gap is discovered by
the thing it was supposed to prevent.

Build order follows from §2. The capability kernel is already in the workspace, so the
`DaemonBearer` class, attenuating delegation and transitive revocation are available now and
need adoption rather than design. The work that remains is, in order: seal verification on
any path that loads a capability it did not mint; the consumption state machine with atomic
`open -> consumed`; descriptor binding; and only then the asymmetric `ActorSignature`
primitive, which is the one piece with no existing implementation to build on.

**That order deliberately puts the hardest piece last, and that is a risk to state rather
than hide.** The first three are tractable and will make the substrate look substantially
more authenticated while delivering nothing for P1. The temptation at that point will be to
describe the result as done. It is not: everything before `ActorSignature` improves the
`DaemonBearer` column only, and no consumer in the Consumers table can retire anything
against that column under the Local profile.

## Withdrawal trigger

If, by the time the first protected action ships, the local profile still has no custody home
that separates actors, this record's P1 row must be re-stated as NOT-DELIVERED in both
profiles or the record withdrawn. Owner: the maintainers. Moment: the first `ActorSignature`
policy landing on a protected verb.

The reason is the failure this ADR is most exposed to. A design that authenticates correctly
against an adversary nobody faces, while reading as delivered against the adversary everybody
faces, is worse than no design, because a downstream consumer deletes a working safeguard on
the strength of it.

## Rejected alternatives

1. **Bearer tokens only.** Useful for revocable admission and local ergonomics. Does not
   deliver P2 or P5 against an untrusted store, and cannot justify retiring confirm-back.
2. **Bearer first, signatures later, under one undifferentiated API.** Rejected. It is one
   storage migration and not one trust-contract migration. A daemon lookup proves the daemon
   accepted a bearer; a signature proves possession of a key over exact bytes; bearer-era
   records cannot acquire the latter retrospectively. If both return the same `PASS`, the
   staged design **launders weaker evidence through the stronger API**. Acceptable only as an
   explicitly lower-assurance mode that consumer policy can refuse — which is what the
   `assurance_class` field in §2 exists to be refused against. Note that the existing kernel
   can _only_ issue the lower-assurance class, so this alternative is not hypothetical: it is
   the shape the work naturally takes if `ActorSignature` is deferred and the API is not
   differentiated.
3. **Sign every write in every pack.** Rejected as unnecessary scope. The properties require
   the _authorization_ to carry verifiable provenance, not every KG write.
4. **Grant as a note kind.** Rejected for the authority-bearing record. It depends on two
   controls that do not exist: ADR-125 records the owning-pack-versus-generic capability
   distinction as unestablished, and pack note-kind specs are collected for introspection and
   future enforcement rather than current lifecycle enforcement. Using the grant note to
   justify the primitive reverses the dependency. A read-only note or event projection for
   search and audit is fine.
5. **Authorization as recipient-side discipline.** Rejected: cannot centrally deliver P3, P6
   or P7, and leaves the confirm-back cost in place.
6. **Verification inside every pack.** Rejected: duplicates identity resolution and failure
   semantics, and cannot land without rewrites.
7. **Credential verification in the store.** Rejected: the store lacks verb and action
   semantics, conflicts with ADR-007's dumb-storage contract, and does not cover non-store
   actions.
8. **Semantic equivalence for action binding.** Rejected: non-deterministic, hard to
   negative-test, and invites fail-open ambiguity.

## Consequences

- ADR-125's `OwnerOnly` question is resolved: an owning-pack write path is one holding the
  pack's capability in an authenticated context, which a generic caller cannot construct.
  The creation half of the reserved-key policy becomes enforceable.
- `comm.forward` can cite `outbound_ref` as provenance once generic callers can no longer
  establish one. Until then its citation remains unverified and the feature stays blocked on
  this record rather than on a message-level patch.
- The term "non-repudiation" does not appear in the normative body and must not be used for
  this scheme. Actor signatures do not provide non-repudiation against the actor itself or
  against a compromised key.
- Verification results are assurance-bearing throughout. Any consumer treating the result as
  a boolean is defective by construction.

## Related decisions

**ADR-053** (Authorization Gate — ActorStore, SessionStore, and Caller Propagation) is superseded
as of 2026-07-25; its authentication half is this record. Its §4 composition seam is reproduced
verbatim below, unmodified, because this is where the ADR-127 implementation lane reads it — a
superseded record is not a place an implementation lane takes its contract from. Origin: ADR-053
§4. `TenantGate` is a shape, not a named crate to build: the multi-actor authority home is
ADR-129's `CapabilityGate`, and the seam property this text fixes is that a policy gate composes
behind the existing `Gate` trait, reads the resolved actor, and requires no pack or handler
change.

### 4. TenantGate — multi-actor deployments

An operator-supplied `TenantGate` (a custom crate behind the Apache-2.0 `Gate` trait) uses the
resolved `ActorRef` to enforce per-verb ACLs and feed the usage accounting stage. Because it
implements the existing `Gate` trait with its existing
`check(&self, req: &GateRequest) -> Result<GateDecision, GateError>` signature, swapping
`AllowAllGate` for `TenantGate` changes no pack and no handler.

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
that operator-supplied implementations carry no restrictively-licensed dependency. The
`khive-gate-rego` and any future custom gate crates depend on `khive-gate`, not the
other way around.
