# ADR-143: Store-held caller grants and hierarchical subactor identity

- Status: Proposed
- Date: 2026-08-03
- Amends: [ADR-129](ADR-129-fail-closed-gate-default.md) — supersedes Amendment 2's
  configuration-text caller enumeration and its "no runtime registration API"
  invariant; every other ADR-129 decision remains in force. This is the amendment
  ADR-129's header announces on hierarchical subactor identity and an audited
  grant surface.
- Depends on: [ADR-018](ADR-018-authorization-gate.md) (gate contract and audit
  shape), ADR-127 (capability substrate), [ADR-022](ADR-022-events-query-surface.md)
  (audit records are queryable events)

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

### 1. Caller grants are store-held records

A caller grant is a substrate record, not a line of configuration:

- **Record content.** Grantee principal (structural form), namespace, rights
  set, granting principal, grant time, and — once revoked — revocation time and
  revoking principal. Creation and revocation each produce an audit event in the
  event store, in ADR-018's audit shape, queryable per ADR-022.
- **Check-time view.** A principal's effective authority is computed at gate
  consultation as a view over its live (unrevoked) grant records. There is no
  boot-time snapshot of caller authority to go stale. This is the per-view form
  of per-caller differentiation ADR-129's header note announces: callers differ
  because their record sets differ; Amendment 2's uniform-enumeration constraint
  is retired with the config carrier that forced it.
- **Revocation semantics.** Revocation marks the record and emits the event;
  it never deletes. A revoked grant takes effect no later than the next gate
  consultation, without a process restart. An implementation may cache the view
  only if it preserves that observable property (the acceptance tests below pin
  it).
- **The process root grant is unchanged.** Stage 1c's boot mint of the serving
  process's own principal, derived from its resolved configuration, remains the
  trust anchor exactly as specified. What moves to the store is authority for
  principals other than the booting process.

### 2. One authorized grant surface

Amendment 2's invariant "no runtime registration API" is superseded by exactly
one: the grant surface. The rest of the invariant is carried forward unchanged —
one gate instance, minted at the boot seam, no outside-gate constructor.

- **The surface is itself gated.** Grant and revoke are checked under Amendment
  1's pseudo-verb invariant, against the full authority the resulting record
  carries: granting or revoking a `(namespace, right)` pair requires the caller
  hold **grant administration** for that pair (clause 1 — the check is on what
  the result grants, not what the grantor momentarily needs), and a request
  covering several pairs is checked on every one of them, any failure denying
  the whole request — there are no partial grants (clause 2).
- **Grant administration is a distinct right.** The boot mint grants it to the
  process principal over the namespaces of its Stage 1c enumeration. Delegating
  grant administration goes through the same surface under the same clauses,
  which makes delegation attenuating by construction: no caller can pass on
  authority it does not hold.
- **Configuration text never carries caller grant content again.** From this
  amendment forward, the only writer of caller authority is the grant surface,
  and every write it performs is attributed and audited.

### 3. Hierarchical subactor identity

- **Structure.** A subactor is the pair (parent principal, leg label), keyed
  structurally in the principal registry per Stage 1b. The flattened
  parent-then-label rendering is display-only. Exactly one level exists:
  a subactor cannot have subactors, and the grant surface refuses a subactor
  grantee whose parent is itself a subactor.
- **Leg label grammar.** Non-empty, bounded length, drawn from a closed
  character set that excludes the principal delimiter. The label is an
  identifier, not a carrier: no rights, namespaces, or routing semantics are
  encoded in it.
- **Authority.** A subactor holds no implicit authority. Its effective
  authority at any consultation is the intersection of the grants minted to it
  through the grant surface and its parent's live effective authority at the
  same instant. Two consequences are the point: the grant surface's attenuation
  rule means a parent can only ever mint a subactor up to what the parent
  holds, and the live-intersection rule means authority a parent loses is lost
  to its subactors at the same consultation — a revoked parent has no live
  legs.
- **Attribution.** Audit records for a subactor carry the full structural
  identity. The parent is recoverable from every record its legs produce.

### 4. Stamp-time parse validation

Every surface that accepts a serialized principal — the grant surface, request
attribution, the configuration bootstrap, and any import path — parses it
against the closed grammar at intake. A string that does not parse is a typed
refusal at that surface: the operation does not proceed, and nothing is stored.
The refusal is itself auditable and names the transport-level identity that
submitted it.

This is a load-bearing detector, not hygiene. Malformed identity surfaces at
the moment of introduction, attributed to its submitter, instead of surfacing
at read time as a stored string no one can explain. A grammar violation
discovered anywhere downstream of an intake surface is a defect of that
surface, not of the reader that found it.

### 5. Configuration transition

- **`granted_actors` is deprecated with import-once semantics.** On the first
  boot where the store holds no caller-grant records and the resolved
  configuration carries a non-empty `granted_actors` list, the boot mint
  imports the list: one store-held grant record per caller carrying the Stage
  1c normalized enumeration, attributed to the boot principal, with the import
  recorded as an audit event. On every subsequent boot the list is inert; a
  boot that observes both a non-empty list and existing store records reads
  only the store and logs a divergence warning naming the ignored entries. The
  key's documentation states the deprecation.
- **`grant_unattributed` remains configuration, with its effect moved into the
  store.** The flag is a deployment posture — whether the anonymous fallback
  identity participates at all — and posture is legitimately configuration.
  When true, the boot mint provisions the anonymous principal by creating a
  store-held grant record for it, so even this path is audited and revocable
  at runtime like any other caller grant. It remains an explicit opt-in, never
  a default.

## Acceptance

All conditions are executed tests, not review assertions:

1. **Grant/revoke round trip, no restart.** A caller granted through the
   surface serves a real verb; after revocation the same dispatch is denied
   with the distinguishable no-principal (or revoked-grant) error, against the
   same still-running process. The no-restart property is the observable
   difference between store-held and configuration-held state.
2. **Second-identity probe, carried forward.** Amendment 2's acceptance holds
   verbatim: an enumerated non-boot identity serves, a non-enumerated identity
   is denied distinguishably, and a check run only under the boot identity does
   not count as verification of a multi-caller deployment.
3. **Attenuation.** A caller lacking grant administration on a
   `(namespace, right)` pair cannot create a grant covering it; a multi-pair
   request with one uncovered pair is denied whole, and no partial record
   exists afterward.
4. **Subactor bounds.** A leg serves only where its parent currently serves;
   revoking the parent denies the leg at the next consultation with no
   restart; a leg label violating the grammar is refused at intake; a grant
   naming a subactor of a subactor is refused.
5. **Stamp-time refusal with a positive control.** Each intake surface refuses
   a malformed principal with the typed error, and the same surface accepts a
   well-formed principal in the same test — the refusal arm is meaningful only
   beside a passing arm.
6. **Import-once.** A first boot against an empty store imports and audits the
   configured list; a second boot with a divergent list changes no records and
   logs the divergence warning.

## Consequences

- Caller authority becomes shared, attributed, historied, and revocable at
  runtime; multiple serving processes over one store enforce one caller set by
  construction.
- The gate's consultation path gains a read dependency on live grant state.
  Whatever caching an implementation adds is bounded by acceptance conditions
  1 and 4 — a cache that can serve a revoked principal past the next
  consultation is a defect, not a tuning choice.
- The grant surface is a new authorization-bearing API and inherits no safety
  from having been built for safety: it is reviewed adversarially as an
  authorization surface in its own right, and its acceptance arms include the
  refusal cases above, not only the serving cases.
- Amendment 2's uniform grants disappear as a constraint. Deployments that
  want uniform callers still get them — by granting uniformly — but the
  substrate no longer forces it.
- The one-level subactor cap is a deliberate ceiling. If a real need for
  deeper hierarchies arises, that is a new decision record; the cap is not
  relaxed in an implementation PR.
