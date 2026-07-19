# ADR-117a: Session Identity and Tenant Isolation

**Status**: proposed
**Date**: 2026-07-19
**Authors**: khive maintainers
**Implements**: [ADR-117](ADR-117-session-continuity-search.md) D1, D2, D4 (the direction ADR names
this follow-on as the carrier of the scoped-identity migration, the fail-closed handler-seam
predicate, the ADR-018 amendment, and `session.search` itself)
**Amends**: [ADR-018](ADR-018-authorization-gate.md) — adds Amendment 2, a fail-closed verb class
**Depends on**:

- [ADR-007](ADR-007-namespace.md) — Namespace as attribution (the tenant scope is the request's
  resolved storage namespace; a caller-supplied namespace string is never the scope)
- [ADR-018](ADR-018-authorization-gate.md) — Authorization Gate (pre-dispatch, row-blind, fail-open
  on infra error; this ADR carves the first fail-closed verb class out of that default)
- [ADR-021](ADR-021-memory-pack.md) — Memory Pack (the FTS5 + RRF retrieval primitives `session.search`
  reuses)
- [ADR-025](ADR-025-verb-speech-acts.md) — Verb Speech Acts (`session.search` is Assertive)
- [ADR-028](ADR-028-pack-scoped-backends.md) — Pack-Scoped Backends (the session mirror schema is
  pack-owned; this migration evolves that pack schema, not the core `migrations.rs`)
- [ADR-083](ADR-083-session-pack-t1-verbs.md) — Session Pack T1 Verbs (the existing verbs and the
  mirror aux tables this ADR scopes and searches)
- [ADR-096](ADR-096-warm-daemon-per-request-identity.md) — Warm-Daemon Per-Request Identity (the
  `RequestIdentity` → `NamespaceToken` seam this ADR's predicate keys to; and the **deferred
  connection-identity mechanism** that authenticates that scope at the hosted bar)

---

## Context

[ADR-117](ADR-117-session-continuity-search.md) is a direction ADR. It fixes four requirements that
this follow-on carries with proof: message identity must be tenant-scoped (D2), tenant isolation must
be fail-closed and enforced where rows exist (D4), and the `session.search` verb (D1) does not ship
until both are proven in the same change. This ADR delivers the schema migration, the enforcement
predicate, the ADR-018 amendment, the verb, and the two tests — as one PR.

Three source facts set the mechanism:

1. **The mirror schema is a bare global primary key with a nullable namespace.** `sessions.id` and
   `session_messages.id` are each `TEXT PRIMARY KEY` with no tenant component, and both tables carry
   a nullable `namespace TEXT` column that is part of no key
   (`crates/khive-pack-session/src/vocab.rs`, `SESSION_SCHEMA_PLAN_STMTS`). `session_messages.id` is a
   provider event id (Claude Code top-level `uuid`, ChatGPT `message.id`, or the synthesized
   `"{session_id}:{byte_offset}"` for Codex — `mirror/parse.rs`), unique enough for the single-writer
   mirror's idempotency but not across tenants. The mirror is single-tenant by construction; the
   latent tenant scope is exactly that unused `namespace` column.

2. **The session schema is pack-owned and applied idempotently.** `SESSION_SCHEMA_PLAN_STMTS` is a set
   of `CREATE TABLE/INDEX IF NOT EXISTS` statements applied at boot via the pack's `schema_plan` hook
   (ADR-028), and lazily in tests via `execute_script`. Evolving it — adding a column, adding a
   uniqueness constraint over existing rows — cannot be a new idempotent `CREATE`; it needs a pack-level
   migration step with a backfill for rows already on disk.

3. **Per-request identity resolves to a `NamespaceToken`, but connection authentication at the hosted
   bar does not exist yet.** ADR-096 Fork 1 threads a `RequestIdentity` (`namespace`, `actor_id`,
   `visible_namespaces`) on the daemon frame; dispatch mints the request `NamespaceToken` from it
   (`authorize_with_visibility`, `runtime.rs:475`). On the single-principal `0600` socket that scope is
   trusted by socket-uid. But ADR-096 is explicit (Open question 1, Acceptance condition 2) that the
   frame fields are **self-reported client fields**, that no peer-credential capture exists at `accept`,
   and that hosted/multi-tenant enablement is **blocked on a separately gated connection-identity ADR**
   that must build authenticated connection identity before a shared socket serves more than one
   principal. This ADR's isolation is correct and fail-closed under both regimes; what differs is only
   what authenticates the scope — socket-uid locally, the deferred peer-credential mechanism when hosted.

---

## Decision

### D1 — The `session.search` verb (FTS5, tenant-scoped)

One Assertive verb (ADR-025), added to the session pack:

```
session.search(query, limit?, since?, source?, cwd?)
  -> [{ session, score, snippets }]
```

- **Corpus**: the mirror's `session_messages.text`, via an FTS5 index and a triggered content table —
  the same pattern the note substrate already uses. Ranking reuses the memory pack's FTS + RRF
  primitives (ADR-021); it is not re-implemented, and a later vector signal (ADR-117 D2, gated) fuses in
  without a signature change.
- **Scope**: every query is bound to the caller's resolved tenant scope by the D4 predicate — never a
  caller argument.
- **Filters**: `since` (recency over `created_at`), `source` (originating tool), `cwd` (working
  directory), each an additional `AND` term, never a way to widen scope.
- **Result**: matching sessions ranked by score with the matching snippets; a hit's identity is what
  ADR-117c bridges back to `resume`/`export`.

`session.search` is registered but its handler refuses to serve unless the D2 migration and the D4
predicate are present — it does not ship as a searchable surface before this PR's enforcement lands
(the ADR-117 D1 hard gate, discharged here).

### D2 — Scoped-identity migration

A pack-level migration (a versioned step in the session pack's schema evolution, not a new idempotent
`CREATE`) transforms the mirror identity from bare-global to tenant-scoped:

1. **Promote `namespace` to the tenant-scope column.** Backfill every existing `NULL` namespace on
   `sessions` and `session_messages` to the deployment's local default namespace (ADR-007), then make
   the column `NOT NULL`. Existing single-tenant local data lands under the local scope; no row is
   dropped.
2. **Add the scoped uniqueness contract.** Uniqueness becomes tenant-scoped, per ADR-117 D2:
   - `sessions`: `UNIQUE(namespace, provider_session_id)`.
   - `session_messages`: `UNIQUE(namespace, session_id, id)` — the `(account, provider_session_id, event)`
     contract, where `namespace` is the account, `session_id` ties the event to its transcript, and `id`
     is the provider event id. Two accounts producing the same provider event id no longer collide.
3. **Add a content-hash adjunct — not the identity.** Add `content_hash TEXT` on `session_messages`
   (a hash of the parsed text plus raw line). Per ADR-117 D2 this is a **dedup / integrity adjunct on the
   scoped id, never the id**: it detects whether an idempotent re-stream of the same scoped event carries
   the same content, and it gates a future embedding backfill so an unchanged event is not re-embedded. It
   is deliberately not unique and not part of any key — identical text is two legitimate events under one
   scoped id space.
4. **Embedding-ready.** The scoped per-event id plus the content hash make a later vector backfill
   (ADR-117 D2, gated on khive#1121) idempotent and re-runnable. No embedding is written in v1.

The migration preserves every existing row and provider event id and keeps the mirror's re-stream
idempotency intact under the new scoped key. Enforcing `NOT NULL` and the new uniqueness constraint over
existing rows is a table rebuild under SQLite (create-scoped, copy, swap), backfilled in the same step —
the mechanics are the implementation's; the contract fixed here is the scoped uniqueness key, the
non-null tenant scope, and the content-hash adjunct.

### D4 — Fail-closed tenant-isolation predicate at the handler seam

Isolation is enforced **where the rows exist** — in the `session.search` handler — not at the Gate
(which is pre-dispatch, row-blind, and fail-open per ADR-018). The enforcement is **construction-primary**:
the safety property holds in the shipped seam by how the query is built, independent of any policy or
amendment landing first.

1. **The scope is the request's resolved `NamespaceToken`, never a caller argument.** The handler reads
   the tenant scope from the per-request identity minted by dispatch (ADR-096 Fork 1), exactly as the
   other pack handlers receive their token. There is no `namespace`/`account` parameter on
   `session.search`; a caller cannot supply or widen the scope. This closes the ADR-007 forgeable-namespace
   anti-pattern by construction.
2. **The predicate is non-widenable and always present.** The search SQL is constructed with
   `WHERE namespace = :scope` as a fixed, non-optional term bound from the token scope; the FTS `MATCH`
   term and the `since`/`source`/`cwd` filters are only ever additional `AND` conjuncts. There is no code
   path that emits the query without the scope term. Isolation is a property of query construction, not of
   a filter a caller or a policy could omit.
3. **Fail-closed by construction: no scope → no rows.** `session.search` requires a positive tenant
   scope to execute. If the request yields no resolved scope, the handler refuses (returns
   `PermissionDenied`) rather than running an unscoped query. This is what makes ADR-018's fail-open
   default non-leaking here: even if the Gate errored and failed open (allowed the verb), the handler still
   cannot produce cross-tenant rows, because fail-open yields no authenticated scope and no scope yields a
   refusal, not a widened query.

**Authentication of the scope is deployment-regime-specific, and this ADR composes with ADR-096 rather
than re-solving it.** On the single-principal `0600` socket the scope is trusted by socket-uid, and
`session.search` is safe to expose. Serving `session.search` to **more than one connection principal over a
shared socket** inherits ADR-096 Acceptance condition 2: it stays blocked until the separately gated
connection-identity ADR builds authenticated connection identity (peer-credential capture at `accept`, a
connection principal threaded into `GateRequest`, `frame.namespace` threaded into dispatch). The D4
predicate is correct and fail-closed in both regimes — the deferred ADR only changes what authenticates the
`:scope` value it binds, never whether the predicate is present.

### The ADR-018 amendment — a fail-closed verb class (Amendment 2)

ADR-018 §Fail-open establishes that a gate `Err` (infra failure) proceeds; only explicit `Deny` blocks.
ADR-018 Amendment 1 §4 already carved one fail-closed exception (an unresolvable wire verb is denied
before dispatch). This ADR adds Amendment 2: a **fail-closed verb class**.

> **ADR-018 Amendment 2 (2026-07-19) — Fail-closed verb class.** A verb may declare membership in a
> fail-closed class. A verb in this class carries a handler-seam requirement of a **positive authenticated
> tenant scope**: its handler must refuse to execute without one. For these verbs the Gate's fail-open
> default (proceed on infra `Err`) cannot leak data, because the handler's construction-primary refusal
> stands independently of the Gate decision — a failed-open allow still yields no scope and therefore no
> rows. The amendment is a contract-level codification of a property the member's handler seam already
> guarantees by construction; it does not move enforcement into the Gate (which remains pre-dispatch and
> row-blind). `session.search` is the first member. Future verbs that return cross-tenant-sensitive rows
> join the class by the same handler-seam contract.

This keeps ADR-018's model intact — the Gate is still the pre-dispatch authorization seam, still fail-open
on infra error — and names, at the contract level, the class of verbs whose safety must not depend on the
Gate at all.

### D-tests — isolation and no-scope-refusal land in the same PR

Per ADR-117 D4's hard condition, isolation is a property proven by test, in the same change as the verb:

1. **Cross-account isolation test.** Seed `session_messages` under two distinct tenant scopes with text
   that both would match. A `session.search` issued under scope A returns only scope-A sessions and never a
   scope-B row, and vice versa. The assertion is on rows returned, not on a filter being present.
2. **No-scope-refusal test.** A `session.search` request that resolves to no positive tenant scope is
   refused (`PermissionDenied`), not served as an unscoped query — the construction-primary fail-closed
   property, exercised directly. A companion assertion drives the Gate-failed-open path (an
   `AllowAllGate`-equivalent allow with no scope) and confirms the handler still refuses.

Both tests ship in this PR alongside the migration and the predicate. `session.search` does not become a
searchable surface until they pass.

---

## Consequences

**Delivered.** A tenant-scoped mirror identity (scoped uniqueness + content-hash adjunct, embedding-ready);
a `session.search` verb whose every query is scope-bound by construction; a fail-closed handler seam that
does not leak under ADR-018's fail-open default; an ADR-018 amendment naming the fail-closed verb class; and
the isolation + no-scope-refusal tests that prove it. This discharges ADR-117 D1/D2/D4 with proof at source.

**Not delivered here.** Deletion and retention across the derived surfaces (ADR-117b); the resume/export
continuity bridge for a hit's identity (ADR-117c); cross-machine ingestion (ADR-117d); and any vector signal
(ADR-117 D2, gated on khive#1121). The FTS index this ADR adds is one of the surfaces ADR-117b's deletion
contract must cover — noted here as the forward dependency, specified there.

**The hosted-tenant authentication dependency is explicit, not hidden.** `session.search` is safe to expose
on the single-principal local socket now. Multi-principal exposure over a shared socket is gated on ADR-096's
deferred connection-identity ADR, the same gate ADR-096 places on all hosted enablement. This ADR does not
weaken that gate and does not claim to authenticate connections; it makes the isolation predicate correct so
that when authenticated connection identity lands, `session.search` is already fail-closed by construction.

**Cost.** The migration backfills the `namespace` column and rebuilds the uniqueness constraint over
existing rows once; the FTS index and its triggered content table cost the standard write-amplification the
note substrate already pays. Retrieval latency is the `memory.recall` shape (ADR-117 Cost), so khive#1116
and khive#1121 are the cost levers.

---

## Alternatives considered

**A new surrogate id column as the scoped identity.** Rejected as unnecessary churn: the existing nullable
`namespace` column is the latent tenant scope, and promoting it (backfill → `NOT NULL` → into the uniqueness
key) is a smaller, backfillable migration than introducing and populating a surrogate key while retaining the
provider event id for re-stream idempotency.

**Content-hash message identity.** Rejected per ADR-117 D2: it collides identical messages across tenants and
collapses legitimate repeated events. Retained as an integrity/dedup adjunct on the scoped id only.

**Enforcing isolation in the Gate (or relying on Gate fail-safety).** Rejected per ADR-117 D4 and confirmed
against ADR-018: the Gate is pre-dispatch, sees no matched rows, and fails open on infra error, so row
isolation can be neither a property of its decision nor a consequence of it failing safely. Enforcement is a
construction-primary handler-seam predicate; the Gate amendment only codifies the class contract.

**A handler filter on a caller-supplied namespace argument.** Rejected as forgeable and as the ADR-007 v1
anti-pattern: the scope is the request's resolved `NamespaceToken`, and `session.search` has no scope
parameter to forge.

**Shipping `session.search` before the migration/predicate/tests.** Rejected — it is the ADR-117 D1 hard gate.
A search verb that returns transcript rows is unsafe before proven tenant isolation exists; the four
deliverables land together or not at all.

**Solving hosted connection authentication in this ADR.** Out of scope by ADR-096 Acceptance condition 2: no
connection-identity mechanism exists today, and building one (peer-credential capture, connection principal in
`GateRequest`) is a separately gated ADR. This ADR makes the predicate correct and fail-closed so it composes
with that work; it does not pre-empt it.
