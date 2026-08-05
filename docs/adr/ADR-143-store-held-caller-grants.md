# ADR-143: Store-held caller grants and hierarchical subactor identity

- Status: Accepted
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
  Also amends [ADR-094](ADR-094-lifecycle-telemetry-events.md)'s event-kind
  taxonomy: the closed `EventKind` enum gains one variant, `GrantChange`, added
  through ADR-094's additive-variant mechanism — the variant joins
  `EventKind::ALL` and the `FromStr`/`Display` round-trip coverage that
  mechanism requires (§4a). ADR-004 is NOT amended: the substrate set is
  unchanged, and `GrantChange` events use the existing `Event` substrate,
  following ADR-094's own precedent for operation-audit events whose subject
  is neither a note nor an entity.
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
- **Revocation is transitive, and the authoritative mechanism is gate-time
  ancestor validation.** A store-held record contributes to effective
  authority only if its complete recorded ancestor chain — every record from
  it up to a boot-minted root — is live (unrevoked) and seal-valid at the
  same consultation, evaluated from the store. Persisting a parent id does
  not rebuild any process's in-memory children index, and no process is
  required to rebuild one: a non-minting process materializes nothing ahead
  of time — at consultation it loads and seal-verifies the record and each
  recorded ancestor, and any missing, revoked, or seal-invalid ancestor makes
  the grant contribute nothing. Revoking a grant therefore denies everything
  delegated from it at every process's next consultation, without restart and
  without cross-process coordination. The substrate's in-process
  children-index revocation walk remains a valid optimization inside the
  minting process; it is not the contract. Chains are shallow by
  construction: §2 forbids subactors as grantors, so lineage passes only
  through top-level principals.
- **The chain's terminal is a durable root anchor.** The in-process boot root
  stays exactly what ADR-127 makes it: an in-memory, raw-inserted bootstrap
  capability that never leaves its process. A cold process therefore cannot
  validate a chain against it, so the boot mint additionally writes a **root
  anchor** to the store: a record carrying the boot principal's structural
  identity, a root-anchor id, creation time, and liveness state. The anchor
  is not a capability and carries no rights; it is the store-side
  registration that a given boot root exists and is live. Like every store
  write, the registration carries a namespace under ADR-007 attribution —
  the booting process's own write attribution, not a new configuration
  knob — and that namespace is where the anchor's §4a history lands. Every durable
  grant's recorded lineage terminates at a root-anchor id, and gate-time
  chain validation requires every intermediate record live and seal-valid
  AND the terminal anchor live. Retiring an anchor — an administrative act
  of the anchor's own boot principal, performed through the grant surface
  and recorded per §4a — denies every chain terminating at it at every
  process's next consultation; this is also how a decommissioned process's
  outstanding delegations are extinguished. Retirement's authorization rule
  is identity, not administration: the surface authorizes it solely by
  structural equality between the caller and the anchor's boot principal —
  no `GrantAdmin` is involved, and under Amendment 1 the classification is
  trivial because the result grants nothing. Cross-process seal verification
  itself presupposes the deployment provisions seal-key material per
  ADR-127's durable-grant deployment obligation; this record consumes that
  obligation and adds only the anchor and its terminal rule, redefining no
  key management.
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
one: the grant surface. Its mutation operations — grant, revoke, and anchor
retirement (§1) — are the only runtime writers of caller authority; the
surface additionally carries its own scope-bounded read for grant-change
reconstruction (§4a), which registers nothing. The rest of the invariant is
carried forward unchanged — one gate instance, minted at the boot seam, no
outside-gate constructor.

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
- **The surface is gated under Amendment 1's pseudo-verb invariant, and the
  right-selection rule is stated so only one implementation is admissible.**
  Creating a grant for a `(namespace, right)` pair requires the caller hold
  BOTH: a live, seal-valid ordinary capability of its own covering that pair —
  and that capability, not one selected by the surface, is the `delegate_cap`
  parent of the minted grant — AND `GrantAdmin` for the same pair. This
  satisfies Amendment 1 clause 1 literally: the caller is checked against the
  full ordinary authority the result grants, because it must hold that
  authority to serve as the delegation parent. An admin-only principal —
  `GrantAdmin` without the ordinary right — can mint nothing; administration
  is permission to delegate what you hold, never a source of authority. A
  request covering several pairs is checked on every one of them, any failure
  denying the whole request with no partial record (clause 2). **Revocation**
  is checked against `GrantAdmin` for every pair in the target's complete
  pre-revocation record set — the pair set is derived by enumerating the
  target records at request time, never taken from the request text — and a
  request whose caller lacks `GrantAdmin` on any derived pair is denied
  whole. Revocation does not require the ordinary right: it removes authority
  and grants none.
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
- **Only the parent grants to its own leg.** A subactor's grants are minted by
  its parent and by no one else: the grant surface refuses a mint whose grantee
  is a `(P, leg)` subactor unless the caller IS `P`. This is the grantee-side
  counterpart to §2's grantor-side rule (subactors are never grantors); §2
  closes who may grant, this closes to whom a non-parent may grant. Both
  readings of the attenuation prose are authority-safe under the
  live-intersection rule, but this record's standard is that only one
  implementation is admissible, so the parent-only rule is stated rather than
  left to inference. Acceptance 3 gains an arm: a non-parent principal holding
  everything §2 requires of a grantor — the ordinary capability AND
  `GrantAdmin` — still cannot mint to another principal's leg, paired with a
  positive control that the parent can. Both prerequisites are named because a
  negative principal missing either one is already denied by an existing
  attenuation check — the admin-only rule when it lacks the ordinary
  capability, the lack-`GrantAdmin` rule when it lacks `GrantAdmin` — so such
  an arm would pass with this rule unimplemented.
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

- **Shape: one grant-change record per affected record.** The affected
  record is a grant record for `grant`, `revoke`, and `import`, and the
  anchor registration record for `retire_anchor` (§1). A grant record covers
  one grantee and one namespace (§1), and its grant-change record does the
  same — this fits the substrate's single-namespace Event envelope rather
  than fighting it; a retirement affects exactly one anchor registration and
  produces exactly one record, in that registration's namespace. An action
  that touches several records (a multi-pair grant, a multi-record revoke,
  the import) produces one grant-change record per affected record, all
  carrying the same **action id** — a fresh identifier minted once per
  surface request (for the import, the epoch marker's id). The whole action
  is reconstructed by querying the action id; no single record claims to
  describe more than the one record it names.
- **Envelope and payload, completely — within the existing Event struct.**
  Each grant-change record is a substrate Event whose envelope uses only
  fields the Event contract already has: `kind` is **`GrantChange`**, a new
  variant on the closed `EventKind` enum — the ADR-094-mechanism addition
  named in this record's Amends header, carrying `EventKind::ALL` and
  `FromStr`/`Display` round-trip coverage, not an informal string; `verb` is
  the surface operation (`grant`, `revoke`, `import`, or `retire_anchor`);
  `substrate` is `Event` — ADR-094's own precedent for operation-audit
  records whose subject is neither a note nor an entity; the substrate set
  is unchanged and no fourth value is invented. The envelope's `actor` field
  is a string by contract, and §3 (via Stage 1b) is explicit that flattened
  principal renderings are not injective — so the envelope string is the
  Stage 1b display rendering and is display-only, while the **authoritative
  acting-principal identity lives in the payload in §3's three-field audit
  form** (`kind`, `id`, and the optional leg label as separate structured
  members). No consumer parses the envelope string; a consumer needing
  identity reads the structured payload members, which is exactly the rule
  §3 already sets for audit records.
  `outcome` is success — records exist only for mutations that happened —
  and `namespace` is the namespace of the record described. Versioning is
  the envelope's existing `payload_schema_version` field, set to 1; there is
  no separate payload version member. The payload carries, for grant-record
  changes: the action id; the grant record id; the grantee's structural
  identity; the rights set; a required rationale; and the authorization
  snapshot below. For `retire_anchor`: the action id; the root-anchor id;
  the anchor's boot principal structural identity; the liveness transition
  (live to retired); a required rationale; and the authorization snapshot in
  its retirement form (defined with the snapshot below). Per-record visibility follows
  ADR-022 unchanged: a record is readable by callers whose read scope covers
  its namespace — for grant records, exactly the namespace whose authority
  the change moved; for a retirement, the anchor registration's attribution
  namespace (§1). Gate-time liveness never depends on history visibility:
  the validator reads the anchor's liveness state directly.
- **Whole-action reconstruction is a grant-surface read, not an ADR-022
  query.** ADR-022's filter set (kind, verb, outcome, actor, substrate,
  time) gains no payload predicate from this record, and its
  caller-namespace scoping is not weakened. Instead: the action id is an
  **indexed field of the grant-change store**, never payload-only, and the
  grant surface itself exposes the lookup — given an action id, it returns
  the grant-change records whose namespace falls within the caller's read
  scope. A caller whose read scope covers every affected namespace
  reconstructs the whole action; any other caller sees exactly the lawful
  per-namespace subset. Reconstruction is scope-bounded by design, not a
  visibility bypass.
- **Authorization is embedded, not referenced.** ADR-018 defines no stable
  decision identifier and its consultation-event persistence is optional and
  non-transactional, so a reference to a gate decision would point at an
  object this contract cannot guarantee exists. Each grant-change record
  therefore embeds an immutable authorization snapshot: the checked pairs,
  the decision, the gate implementation name, the request's actor and verb,
  and the check timestamp. For the import — which no caller requested — the
  snapshot is the synthetic boot authorization: the boot principal, the epoch
  marker id, the configuration source, and the fixed rationale
  `configuration import at first boot`. For `retire_anchor` — whose
  authorization is identity, not pairs (§1) — the snapshot records the
  identity rule instead: the caller's structural identity, the
  structural-equality check against the anchor's boot principal, and the
  check timestamp.
- **The records are written in the same transaction as the mutation they
  describe.** If any grant-change record of the action cannot be written, the
  whole action does not happen. This is a stated exemption from ADR-018's
  optional-audit posture, and the reason is the difference in role: a
  consultation event observes a decision that stands on its own, while
  grant-change records ARE the attributable history this amendment exists to
  create — grant state with no history is the configuration file again, one
  layer down.
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
- **Import content.** The imported principal set is the union of the
  deduplicated `granted_actors` list and — when `grant_unattributed = true` —
  the anonymous principal; the flag's contribution is independent of whether
  the list is empty. For each principal in that union: one grant record per
  namespace of the Stage 1c normalized enumeration (the §1 record shape),
  attributed to the boot principal, delegated from the boot-minted root, with
  `import`-kind grant-change records per §4a sharing the epoch marker's id as
  their action id. The empty-set case — the marker recording that import ran
  and imported nothing — occurs only when BOTH the list is empty or absent
  AND `grant_unattributed` is false.
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
3. **Attenuation, both signs and both requirements.** A caller lacking
   `GrantAdmin` on a `(namespace, right)` pair cannot create a grant covering
   it; a multi-pair request with one uncovered pair is denied whole, and no
   partial record exists afterward. A principal whose enumeration is
   Read-only on a namespace mints Read there (positive control) and is denied
   minting Write there (the boot-admin-never-exceeds-authority arm).
   **Admin-only arm:** a principal holding `GrantAdmin(namespace, right)` but
   no ordinary capability for that pair is denied minting it — administration
   alone is never a source of authority. **Ordinary-source arm:** a minted
   grant's delegation parent is verified to be the grantor's own ordinary
   capability, not a root selected by the surface. **Multi-pair revoke arm:**
   a revoke whose derived target pair set includes one pair the revoker lacks
   `GrantAdmin` for is denied whole, with the pair set enumerated from the
   pre-revocation records rather than the request text. **Parent-only-grantee
   arm:** a non-parent principal `Q` holding everything §2 requires of a
   grantor — a live ordinary capability AND `GrantAdmin`, on every requested
   pair — is still denied minting a grant whose grantee is another principal's
   `(P, leg)` subactor. `Q` must be denied for the parent mismatch itself: an
   arm whose negative principal lacks the ordinary capability is satisfied by
   the admin-only denial above, and one lacking `GrantAdmin` by the
   lack-`GrantAdmin` denial at the head of this condition; either way the arm
   passes whether or not the parent-only rule is implemented. Paired positive
   control: `P`, holding those same two
   prerequisites, does mint to `(P, leg)`, so an implementation that refuses
   every subactor grantee fails this arm rather than passing it.
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
   `grant_unattributed = true`. **Combined-input arm:** a first boot with
   `granted_actors = []` and `grant_unattributed = true` imports the
   anonymous principal — the empty-set clause must not suppress the flag.
7. **Grant-change history.** A single-pair grant and a revoke each produce
   one queryable `GrantChange` Event carrying grantee, rights, actor,
   rationale, and the embedded authorization snapshot; a multi-pair action
   produces one record per affected grant record, all sharing one action id.
   **Reconstruction arms:** the grant surface's action-id lookup returns the
   whole action to a caller whose read scope covers every affected namespace,
   and returns exactly the one lawful record to a caller scoped to a single
   affected namespace (scope-bounded arm); the lookup resolves through the
   indexed action-id field, demonstrated by reconstructing an action whose
   records span at least two namespaces. Import records resolve their
   synthetic boot authorization (boot principal, epoch marker id,
   configuration source, fixed rationale); a simulated failure to write any
   record of an action aborts the whole action (fail-closed arm); a refused
   request produces no grant-change record. **Retirement arm:** an anchor
   retirement produces one `retire_anchor` record in the anchor
   registration's namespace, resolvable through the action-id lookup,
   carrying the liveness transition and the identity-rule authorization
   snapshot; a simulated failure to write that record aborts the retirement
   and the anchor stays live.
8. **Cross-process transitive revocation, with a cold-load positive
   control.** With a delegation chain root → A → B and two serving processes
   over one store: FIRST, the second process — which minted none of the
   chain and holds no in-memory state for it — serves B at its next
   consultation while the chain is fully live, proving the cold-load path
   validates a healthy chain end to end including the terminal root anchor.
   THEN revoke A's grant in the first process: the second process denies B
   at its next consultation, without restart. THEN, on a fresh chain, the
   first process's boot principal retires its root anchor: the second
   process denies both A and B. A test suite containing only the revocation
   arms does not satisfy this condition.

## Consequences

- Caller authority becomes shared, attributed, historied, and revocable at
  runtime; multiple serving processes over one store enforce one caller set by
  construction, and the seal-on-load rule means a store record is evidence
  only after verification, per ADR-127.
- The gate's consultation path gains a read dependency on live grant state.
  Whatever caching an implementation adds is bounded by acceptance conditions
  1, 4, and 8 — a cache that can serve a revoked principal, or a revoked
  delegation chain, past the next consultation is a defect, not a tuning
  choice. The gate-time ancestor validation in §1 makes chain depth a
  consultation cost; the no-subactor-grantor rule keeps that depth to
  top-level delegation chains only.
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
