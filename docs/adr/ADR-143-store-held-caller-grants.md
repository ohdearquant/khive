# ADR-143: Store-held caller grants and hierarchical subactor identity

- Status: Proposed
- Date: 2026-08-03
- Amends: [ADR-129](ADR-129-fail-closed-gate-default.md) — supersedes Amendment 2's
  configuration-text caller enumeration and its "no runtime registration API"
  invariant, and amends Stage 1c's boot-mint authority table with the
  grant-administration pairs defined below; every other ADR-129 decision remains
  in force. This is the amendment ADR-129's header announces on hierarchical
  subactor identity. Also amends [ADR-018](ADR-018-authorization-gate.md) in two
  named places: `ActorRef` gains an optional `leg` field (additive), and the
  grant-change history defined here is exempted from ADR-018's optional-audit
  posture (it is transactional, and the exemption is stated where it is made).
- Depends on: [ADR-018](ADR-018-authorization-gate.md) (gate contract and audit
  shape), ADR-127 (authenticated actor and grant primitive — the capability,
  delegation, sealing, and transitive-revocation substrate this record builds
  on), [ADR-022](ADR-022-events-query-surface.md) (grant-change records are
  queryable events)

## Context

ADR-129 Amendment 2 provisions the callers of a shared serving process from
configuration text: a `[gate] granted_actors` list that the boot mint reads and
converts into uniform principal registrations. That closed the total-denial gap
it targeted, and it deliberately deferred two things: per-caller differentiation
and any grant mechanism beyond boot. Both deferrals are now due, and the
configuration-text carrier is the wrong place to deliver either.

Three structural properties a configuration file cannot supply:

1. **Attribution.** A config edit records no authorizing principal. The gate's
   entire audit posture (ADR-018 requirement 4) is that authorization decisions
   are attributable; the input that decides who may call at all is the one input
   with no attribution.
2. **History.** There is no durable record of when a caller was added or removed,
   by whom, or why. The audit trail starts after the decision it most needs to
   cover.
3. **Revocation.** Removing a list entry changes nothing until the next boot.
   There is no revocation event, no point in time at which access verifiably
   ended, and no way to revoke without restarting the serving process.

A fourth property follows from deployment shape rather than from the file
format: grant state is a per-process parse. Two serving processes over one
store may boot from divergent configuration and enforce different caller sets
against the same data. Store-held grant state is consistent by construction —
there is one set of records, and every process consults it.

The severity of the defect being fixed is bounded, and the bound is part of
this record: the caller-minted gate protects cooperative processes from
misconfiguration and was never a boundary against a local adversary. A
principal able to author the serving process's configuration can equally open
the backing store directly. This amendment improves the auditability,
consistency, and revocability of cooperative-process authorization; it does not
claim to contain a local adversary, and nothing below should be read as that
claim.

Two further gaps motivate the identity half of this amendment:

- **Subactor identity.** A provisioned caller frequently runs bounded sub-tasks
  that need attributable identity distinct from the parent — audit records that
  say which sub-task acted, not merely which caller — without a separate
  top-level enrollment per sub-task. ADR-129 Stage 1b already establishes the
  ground rule: flattened principal renderings are not injective (a component may
  contain the delimiter), so identity is structural and flattened labels are
  display-only.
- **Intake validation.** Principals enter the system as serialized strings on
  several surfaces. A malformed principal that is stored and discovered at read
  time is an unattributable defect; the submitter is long gone. Validated at the
  point the stamp is created, it is a refused operation with the submitter still
  on the line.

## Decision

### 1. Caller grants are store-held ADR-127 grants

A caller grant is a substrate record, not a line of configuration, and it is
not a new authority primitive: it is an ADR-127 grant — a sealed `lion-core`
capability plus ADR-127's grant fields — in durable form.

- **Record content.** One record covers one grantee principal (structural
  form), one namespace, and a rights set for that namespace, matching Stage
  1b's one-capability-per-namespace normalization. Each record additionally
  carries the granting principal, grant time, its delegation lineage (the
  identity of the grant or boot-minted root capability it was delegated from),
  and — once revoked — revocation time and revoking principal.
- **Minting and loading are bound by ADR-127's three grant-path rules.**
  Store-held grants are minted through the delegation path (`delegate_cap`),
  never raw insertion, so narrowing is enforced by the substrate's intersection
  at mint. On load by any process that did not mint the record, the seal is
  verified before the capability is treated as authority; a seal that fails
  verification is a refusal, and one that cannot be verified is unavailable —
  which is also a refusal, never a pass.
- **Revocation is transitive through the recorded lineage.** The substrate's
  revocation walks the delegation children; because every store-held record
  names its parent, that walk reaches durable children across processes.
  Revoking a grant revokes everything delegated from it, effective no later
  than the next gate consultation, without a process restart.
- **Check-time view.** A principal's effective authority is computed at gate
  consultation as a view over its live (unrevoked, seal-valid) grant records.
  There is no boot-time snapshot of caller authority to go stale. This is the
  per-view form of per-caller differentiation ADR-129's header note announces:
  callers differ because their record sets differ; Amendment 2's
  uniform-enumeration constraint is retired with the config carrier that
  forced it.
- **The process root grant is unchanged in mechanism, amended in content.**
  Stage 1c's boot mint of the serving process's own principal, derived from
  its resolved configuration, remains the trust anchor. Its authority table is
  amended by §2 below: the boot mint now also mints grant-administration
  pairs. What moves to the store is authority for principals other than the
  booting process.

### 2. One authorized grant surface

Amendment 2's invariant "no runtime registration API" is superseded by exactly
one: the grant surface. The rest of the invariant is carried forward unchanged —
one gate instance, minted at the boot seam, no outside-gate constructor.

- **Grant administration is a distinct, pair-scoped right, and this is an
  explicit amendment to Stage 1c.** `GrantAdmin(namespace, right)` is held per
  `(namespace, right)` pair, never as a namespace-level blanket. The Stage 1c
  boot mint is amended as follows: for exactly each `(namespace, right)` pair
  in the process principal's normalized enumeration, the mint also mints
  `GrantAdmin` for that pair. Boot-minted administration therefore never
  exceeds boot-minted authority: a process whose enumeration carries Read-only
  on a namespace can administer Read there and can never mint or delegate
  Write there. After boot the two rights are independent — holding a right
  does not imply administering it, and administering a pair does not grant the
  ordinary right; a principal that needs both holds both.
- **The surface is gated under Amendment 1's pseudo-verb invariant.** Creating
  or revoking a grant is checked against the full authority the resulting
  record carries: every `(namespace, right)` pair in the request requires the
  caller hold `GrantAdmin` for that pair (clause 1 — the check is on what the
  result grants), and a request covering several pairs is checked on every one
  of them, any failure denying the whole request with no partial record
  (clause 2).
- **Administration is self-administering, which closes the delegation
  regress.** Delegating `GrantAdmin(namespace, right)` requires holding
  `GrantAdmin(namespace, right)`. There is no higher administrative right, so
  the delegation chain grounds in the boot mint, and because delegation is the
  substrate's narrowing delegation, no caller can pass on administration it
  does not hold.
- **Subactors are never grantors.** `GrantAdmin` is never minted to, and never
  delegable to, a subactor principal, and the grant surface denies any request
  whose caller is a subactor. A bounded leg that needs a new principal
  provisioned asks its parent. Without this rule a leg could mint a top-level
  principal whose authority survives the leg's parent being revoked; with it,
  every grant's lineage passes only through top-level principals, and
  transitive revocation (§1) covers the rest.
- **Configuration text never carries caller grant content again.** From this
  amendment forward, the only writer of caller authority is the grant surface
  plus the one-time import in §5, and every write either performs is
  attributed and recorded per §4a.

### 3. Hierarchical subactor identity

- **Structure.** A subactor is the pair (parent principal, leg label), keyed
  structurally in the principal registry per Stage 1b. Exactly one level
  exists: a subactor cannot have subactors, and every intake surface refuses a
  subactor whose parent is itself a subactor.
- **Wire and audit representation.** ADR-018's `ActorRef` gains one optional
  field, additively: `leg: Option<String>`. Absent means a top-level
  principal; present means the subactor of `(kind, id)` named by the label.
  Parent recovery is field removal, not string parsing. Because the label
  lives in its own field, `kind` and `id` remain fully opaque — a delimiter
  inside either never interacts with the label, and no escaping scheme
  exists or is needed. The flattened parent-then-label rendering remains
  display-only per Stage 1b. Audit records carry the three-field form;
  existing two-field records remain valid (the field is optional), which is
  the additive ADR-018 amendment named in the header.
- **Leg label grammar (closed).** Lowercase ASCII letters, digits, and
  hyphen; length 1 to 64; first and last character alphanumeric. The label is
  an identifier, not a carrier: no rights, namespaces, or routing semantics
  are encoded in it.
- **Authority.** A subactor holds no implicit authority. Its effective
  authority at any consultation is the intersection of the grants minted to it
  through the grant surface and its parent's live effective authority at the
  same instant. Two consequences are the point: the grant surface's
  attenuation rule means a parent can only ever mint a subactor up to what the
  parent holds, and the live-intersection rule means authority a parent loses
  is lost to its subactors at the same consultation — a revoked parent has no
  live legs.
- **Attribution.** Audit records for a subactor carry the full structural
  identity. The parent is recoverable from every record its legs produce.

### 4. Stamp-time parse validation

Every surface that accepts a principal — the grant surface, request
attribution, the configuration bootstrap, and any import path — validates it
at intake against the closed rules above: structured intakes validate fields
(`kind` and `id` non-empty; `leg`, when present, matching the label grammar);
the one legacy string intake (§5's import of the deprecated `granted_actors`
list) accepts top-level principals only and never parses a leg out of a
string. A principal that fails validation is a typed refusal at that surface:
the operation does not proceed, and nothing is stored. The refusal is itself
auditable and names the transport-level identity that submitted it.

This is a load-bearing detector, not hygiene. Malformed identity surfaces at
the moment of introduction, attributed to its submitter, instead of surfacing
at read time as a stored string no one can explain. A grammar violation
discovered anywhere downstream of an intake surface is a defect of that
surface, not of the reader that found it.

### 4a. Grant-change history is transactional, not advisory

Gate consultations continue to audit per ADR-018. Mutations of grant state get
their own record class, because a consultation event cannot carry the history
the Context section promises:

- **A grant-change record** is durable, immutable, and queryable per ADR-022.
  It carries: the change kind (`grant`, `revoke`, or `import`); the id of
  every grant record created or revoked; the grantee's full structural
  identity; the complete set of `(namespace, rights)` covered; the grantor's
  or revoker's full structural identity; a required free-text rationale; the
  timestamp; and a correlation to the gate decision that authorized the
  mutation. A multi-pair action produces one record naming all its pairs.
- **The record is written in the same transaction as the mutation it
  describes.** If the record cannot be written, the mutation does not happen.
  This is a stated exemption from ADR-018's optional-audit posture, and the
  reason is the difference in role: a consultation event observes a decision
  that stands on its own, while a grant-change record IS the attributable
  history this amendment exists to create — grant state with no history is
  the configuration file again, one layer down.
- Refused requests produce ADR-018 consultation denials only; grant-change
  records exist exclusively for mutations that happened.

### 5. Configuration transition: one import epoch

Both legacy `[gate]` keys become inputs to a single import epoch and are inert
afterward.

- **The epoch is a store-held marker with a uniqueness constraint, written
  atomically with the imported records in one transaction.** First-boot
  detection keys on the marker's absence, never on "no caller-grant records
  exist". Two processes booting concurrently against an empty store both
  attempt the import; the uniqueness constraint admits one transaction, the
  loser's transaction fails whole, and the loser proceeds by reading the
  winner's records — no duplicates, no union of divergent lists.
- **Import content.** For each caller in the deduplicated `granted_actors`
  list: one grant record per namespace of the Stage 1c normalized enumeration
  (the §1 record shape), attributed to the boot principal, delegated from the
  boot-minted root. If `grant_unattributed = true`, the anonymous principal is
  imported the same way. One `import`-kind grant-change record (§4a) names the
  entire imported set. An empty or absent list writes the marker with an empty
  set: the epoch records that import ran and imported nothing.
- **After the epoch both keys are inert.** A later boot that observes a
  non-empty `granted_actors` list, or a `grant_unattributed` value whose
  effect differs from live store state, changes no records and logs a
  divergence warning naming the ignored entries. In particular, a revoked
  anonymous grant is never recreated by a boot with the flag still true;
  re-enabling anonymous access after revocation is a grant-surface act like
  any other. Both keys' documentation states the deprecation.

## Acceptance

All conditions are executed tests, not review assertions:

1. **Grant/revoke round trip, no restart.** A caller granted through the
   surface serves a real verb; after revocation the same dispatch is denied
   with the distinguishable no-principal (or revoked-grant) error, against the
   same still-running process. The no-restart property is the observable
   difference between store-held and configuration-held state.
2. **Second-identity probe, restated for the store model.** A store-granted
   non-boot identity serves; an identity with no store-held grant is denied
   distinguishably; a check run only under the boot identity does not count as
   verification of a multi-caller deployment. **Differentiation arm:** two
   principals with deliberately different record sets — distinct rights or
   namespaces — are observed to differ at the same consultation point: each
   serves where its records reach and is denied where only the other's do. A
   uniform-grant implementation must fail this arm.
3. **Attenuation, both signs.** A caller lacking `GrantAdmin` on a
   `(namespace, right)` pair cannot create a grant covering it; a multi-pair
   request with one uncovered pair is denied whole, and no partial record
   exists afterward. A principal whose enumeration is Read-only on a namespace
   mints Read there (positive control) and is denied minting Write there
   (the boot-admin-never-exceeds-authority arm).
4. **Subactor bounds.** A leg serves only where its parent currently serves;
   revoking the parent denies the leg at the next consultation with no
   restart; a leg label violating the grammar is refused at intake; a
   subactor of a subactor is refused at every intake surface; a grant-surface
   request whose caller is a subactor is denied regardless of the pairs
   requested.
5. **Stamp-time refusal with a positive control, and lossless identity.**
   Each intake surface refuses a malformed principal with the typed error,
   and the same surface accepts a well-formed principal in the same test. A
   principal whose `kind` or `id` contains the display delimiter round-trips
   through grant, audit, and recall with the leg field intact and the parent
   recoverable by field removal.
6. **Import epoch.** Two concurrent first boots against one empty store
   produce exactly one marker and one imported set, with no duplicates and no
   union. A first boot with an empty list writes the marker; a second boot
   after the list is edited imports nothing and logs the divergence. An
   anonymous grant revoked at runtime stays revoked across a restart with
   `grant_unattributed = true`.
7. **Grant-change history.** A grant and a revoke each produce one queryable
   grant-change record carrying grantee, pairs, actor, and rationale; a
   multi-pair action produces one record naming all pairs; a simulated
   failure to write the record aborts the mutation (fail-closed arm); a
   refused request produces no grant-change record. **Transitive revocation
   arm:** with a delegation chain root → A → B, revoking A's grant removes
   B's derived authority at the next consultation.

## Consequences

- Caller authority becomes shared, attributed, historied, and revocable at
  runtime; multiple serving processes over one store enforce one caller set by
  construction, and the seal-on-load rule means a store record is evidence
  only after verification, per ADR-127.
- The gate's consultation path gains a read dependency on live grant state.
  Whatever caching an implementation adds is bounded by acceptance conditions
  1, 4, and 7 — a cache that can serve a revoked principal, or a revoked
  delegation chain, past the next consultation is a defect, not a tuning
  choice.
- The grant surface is a new authorization-bearing API and inherits no safety
  from having been built for safety: it is reviewed adversarially as an
  authorization surface in its own right, and its acceptance arms include the
  refusal cases above, not only the serving cases.
- Amendment 2's uniform grants disappear as a constraint. Deployments that
  want uniform callers still get them — by granting uniformly — but the
  substrate no longer forces it.
- The one-level subactor cap and the subactors-are-never-grantors rule are
  deliberate ceilings. If a real need for deeper hierarchies or leg-initiated
  provisioning arises, that is a new decision record; neither ceiling is
  relaxed in an implementation PR.
- ADR-018's `ActorRef` and audit shape change additively (the optional `leg`
  field); readers of existing records are unaffected, and writers that
  predate this amendment emit valid records with the field absent.
