# ADR-133: Reduce writer acquisitions on the request path

- **Status:** Proposed
- **Date:** 2026-07-29
- **Relates to:** ADR-094 (audit row construction), ADR-103 (resource attribution), ADR-131
  (admission control for parallel write batches)

## Context

### The measurement

Every verb registered by the packs was classified against source for whether it acquires the
SQLite writer. The classification was fail-closed — absence of evidence for a write was recorded
as `UNKNOWN`, never as read-only — and carried a known-positive control (`comm.read` must
classify as a writer, since it ends in `update_note_properties`; a method that misses it is void).

| Classification        | Count   |
| --------------------- | ------- |
| WRITER                | 51      |
| WRITER-COND           | 45      |
| UNKNOWN               | 4       |
| **NO-WRITER**         | **0**   |
| **Total inventoried** | **100** |

No verb was established as read-only. The 45 conditional entries are conditional on one fact:
whether an `EventStore` is configured. The served daemon wires one whenever `runtime.authorize`
and `runtime.events` succeed, which is the normal path
(`crates/khive-mcp/src/server.rs:433-438`).

**In a deployed daemon, every verb dispatch acquires the writer at least once.**

That census is a floor in three independent ways, and the three should not be collapsed:

1. **Per-verb floor.** It classified verbs by their own handler and did not trace nested
   dispatch. At least one verb (`memory.recall`) reaches additional writers through a background
   dispatch, described below.
2. **Population floor.** The inventory is a sweep of pack source. It records that the served MCP
   surface advertises further comm and brain compatibility verbs that the sweep did not
   reconcile, so 100 is a lower bound on the population, not a count of it.
3. **Population is deployment-dependent.** A live `verbs()` call against a daemon with eleven
   packs loaded returns 90. That is not a contradiction of the 100 and neither number corrects the
   other: pack loading is configurable, so the two figures describe different populations. Any
   count is meaningful only against a stated pack set.

Six rows carry a further caveat distinct from their classification: `blob.put`, the three external
`git` write verbs, `code.ingest`, and `db_diagnostics` have no completed _SQLite writer
acquisition_ trace. Four of those are classified `UNKNOWN`; `blob.put` is classified `WRITER` on
its intrinsic object-store write and `db_diagnostics` `WRITER-COND` on its audit path, so
"incomplete SQLite trace" and "classified UNKNOWN" are different sets and must not be quoted
interchangeably.

These are the reasons acceptance requires a manifest pinned to both a source revision and a pack
set rather than a prose count.

### The incidental writes

An _incidental_ write is one the caller did not ask for: bookkeeping attached to an operation
whose own semantics are read-only.

1. **Per-dispatch audit row.** Appended on the verb-dispatch path
   (`crates/khive-runtime/src/pack.rs`, via `append_audit_event_best_effort`).
2. **Read-flag mutation.** `comm.read` performs a real `UPDATE` of the message row
   (`crates/khive-pack-comm/src/handlers.rs::handle_read`), so a sweep of N messages is N
   acquisitions.
3. **Recall telemetry.** `memory.recall` persists a `RecallExecuted` event per call
   (`crates/khive-pack-memory/src/handlers/recall.rs`).
4. **Serve ledger.** `memory.recall` also dispatches `brain.record_serve` from a background task
   (`crates/khive-pack-memory/src/handlers/recall.rs:900-936`), and that handler acquires
   `sql.writer()` and inserts into `brain_serve_ledger` **once per returned target**
   (`crates/khive-pack-brain/src/handlers.rs:1942-1970`,
   `crates/khive-pack-brain/src/serve_ledger.rs:85-124`).

Item 4 scales with _result count_, not call count, and is therefore the largest of the four for a
recall returning many targets. It is listed last because it was found last — by review, after the
first draft of this record asserted the list was complete. The list is now believed complete for
the read path and is not assumed to be.

### The distinction this record turns on

An earlier draft of this record conflated two independent goals:

- **Reducing writer acquisitions.** Batching N appends into one transaction. Costs latency.
  **Loses nothing.**
- **Reducing durability.** Buffering appends in memory and accepting their loss on abrupt
  termination. Loses data.

Only the first is needed. The contention problem is the number of times the writer is _acquired_,
not the number of rows eventually written. Treating these as the same thing produced a decision
that traded durability for a benefit that durable batching already provides.

### Why that mattered concretely

ADR-103 establishes that **accounting rides the per-dispatch audit row**: _"Accounting rides the
audit row ADR-094 already established as the daemon's default construction"_
(`ADR-103:405-410`), and the usage object _"lands under the existing per-dispatch audit row's
`resource` payload"_ (`ADR-103:814-822`), read by four consumers including accounting and Gate
quota.

So the per-dispatch audit row **is** the accounting record. Any decision that makes that row lossy
makes accounting lossy. An earlier draft proposed exactly that while simultaneously asserting an
invariant forbidding it — a self-contradiction, reachable only because the draft classified events
by `EventKind` while the property that decides the answer (does this row carry an accounting
payload) lives at the payload level, not the kind level.

## Decision

### D1 — Batch audit appends share one transaction, and the dispatch waits for its own batch to commit

Audit appends are accumulated and written as a **single transaction**. The flush is a real
committed (store-visible) transaction, not an in-memory retention with best-effort persistence.

**A dispatch does not return before the batch carrying its audit row has committed.** This is the
load-bearing clause of the whole record and an earlier draft left it unstated, which made the
draft readable two incompatible ways.

**Scope of that clause, which an earlier draft got wrong by stating it without qualification.** It
binds unconditionally for **obligation-bearing rows** (accounting, authorization, security audit):
such a dispatch never returns success without its row committed, and a persistent commit failure
fails it. Amendment 1 below carves out one narrow, named exception to this binding: for the
`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS` read set, and only when the row's own commit did not
happen because the audit lane's admission was transiently exhausted or the caller's bounded wait
for it elapsed (never a persistent commit failure), the dispatch reports its already-computed
result without waiting on that row.

For **pure observability rows** it binds on the success path only. The dispatch waits for the batch
in the normal case — so observability rows are batched, share the acquisition reduction, and are
committed (store-visible) at return whenever the commit succeeds — but a **persistent** commit
failure releases the dispatch successfully and routes the failure to instrumentation.

That carve-out is not a durability regression, and the reason is worth stating because it is the
only thing that makes the carve-out legitimate. The current implementation already swallows a failed
audit append and returns success from the verb. So on a persistent failure today's code keeps
nothing either; the carve-out reproduces today's outcome for the one case where today's outcome is
correct, and improves every other case. Stating D1 universally while keeping this carve-out made the
two mutually unsatisfiable, which is a wording defect rather than a design one — but it was
executable, and an implementation could have passed the acceptance suite by resolving it the wrong
way.

Batches form under contention rather than on a fixed schedule: a row is committed immediately when
the writer is free, and rows arriving while a commit is in flight accumulate and commit together
as one transaction when it completes. A size cap forces a split. A time threshold exists only as an
upper bound, not as the normal trigger.

The consequences of the waiting clause, all of which follow from it directly:

- **Store visibility is unchanged at the moment of return.** A row that is committed today before
  the dispatch returns is still committed before the dispatch returns. INV-4 holds by construction
  rather than by assertion. "Committed" here is write-path store visibility under the store's
  configured posture; whether a committed row survives an OS crash is the store's durability
  posture, which is ADR-134's subject, not this clause's.
- **Acquisitions stop scaling with request rate.** Under concurrency the acquisition rate is
  bounded by one per commit cycle instead of one per dispatch. This is the entire objective.
- **Under contention this reduces latency rather than adding it.** Today each dispatch queues for
  its own exclusive writer acquisition, and the writer-acquisition timeout is a real risk under
  ordinary concurrent traffic. Waiting for a shared commit that is already in flight is cheaper
  than waiting for an exclusive turn.

  Stated exactly, because the loose version undercounts: a dispatch arriving while the writer is
  idle commits immediately and waits approximately nothing. A dispatch arriving mid-commit waits the
  **tail of the in-flight commit plus its own batch's commit**, so the worst case is up to **two**
  commit durations, not one.

### D1a — A malformed row must not discard its batch

Sharing one transaction means one failing statement rolls back every row in it. **Under D1 the
blast radius is cross-caller**: a batch spans the concurrent dispatches whose rows it carries, so
one malformed row can fail unrelated callers' audit writes. An earlier draft asserted the opposite
on the assumption of a single producer; the waiting clause makes that assumption false and
strengthens what this decision must guarantee.

- A row that fails validation is rejected **before** it enters the batch, never at flush time. With
  a cross-caller blast radius this is a correctness requirement, not hygiene.
- A commit that fails for a **transient** reason (writer acquisition timeout, busy) is retried as a
  batch, with bounded attempts, **subject to D1c**.
- A commit that fails **persistently** is surfaced, never discarded silently. Discarding would
  reproduce the swallowed-failure behaviour this record exists to remove, at larger granularity.
  What "surfaced" means to the caller differs by record class and is D2's subject.

### D1c — Retry requires record identity, because an ambiguous commit can duplicate accounting

D1a's retry introduces a failure mode in the opposite direction from every other one in this
record, and the record does not otherwise cover it.

A commit can **succeed in the store while its acknowledgement is lost or unknown to the caller**.
Retrying the batch in that state appends a second audit row carrying the same ADR-103 accounting
payload. The record is not dropped, not deferred, and not falsely acknowledged — it is
**duplicated**, and the accounted usage is double-counted. Every clause of INV-1 as previously
written can be satisfied while this happens.

The direction matters and is the reason this is treated as a first-class decision rather than an
implementation note. Loss produces an undercount, which errs in the accounted party's favour and is
detectable as missing usage. Duplication errs against the accounted party and is not detectable after
the fact from the accounting records themselves, because a duplicate is indistinguishable from a
second genuine dispatch unless identity was established at production time.

Therefore:

- Every audit row carries a **stable identity minted when the row is produced**, not when it is
  committed. An identity assigned at commit time is a different value on the retry and cannot
  deduplicate anything.
- That identity is **unique per produced row**, enforced by a unique constraint in the store rather
  than by the generator's reputation. Idempotency without enforced uniqueness is the failure below.
- The insert is **idempotent on that identity**, so a retry after an ambiguous outcome cannot
  create a second row.
- **A conflict is accepted only when it proves the incoming row is the same logical record** — the
  stored payload is compared, by equality or by a content digest carried with the row. A conflict
  that does not prove sameness **fails the affected dispatch, fail-closed**.
- **Retry is not permitted for an obligation-bearing row whose identity cannot be established.**
  In that case the failure is surfaced instead. This is the fail-closed direction: a surfaced error
  is recoverable by the caller, and a duplicate accounting row is not recoverable at all once written.

**Durability scope of the identity, stated so nobody builds more than this needs.** The ambiguity
D1c exists for is an **in-process** one: a commit call returns an error or no answer while the
transaction actually committed, and the retry is issued by the same live process holding the same
in-memory batch. After a process crash the unacknowledged batch dies together with the dispatches
waiting on it, so no retry occurs and no post-restart reconciliation is required.

Therefore:

- **No dedupe state needs to survive a process restart**, and no separate dedupe store or
  reconciliation index is required. Building one would be over-engineering for a mode this design
  cannot reach.
- **But the identity is persisted with the row and uniquely constrained in the store**, and that is
  not the same claim. The idempotent insert has to detect a _prior commit_, and a prior commit lives
  in the store rather than in memory — an in-memory-only identity cannot tell "already committed"
  from "never written". The constraint is a unique index on the identity column of the rows this
  record already writes; it is not additional durable machinery.
- **Caller-level verb retries are out of scope** for de-duplication, per D1d: they are new
  obligations, not repeats.

**Why the conflict rule is not pedantry.** The obvious implementation of "idempotent insert" is
`INSERT ... ON CONFLICT DO NOTHING`. That handles a retry of the same row and _also_ silently turns a
genuinely different later row that happens to share an identity into a no-op. Both dispatches then
observe a successful commit while only one obligation was recorded. That is a **dropped** record —
INV-1 clause 1 — reached with no retry and no ambiguous acknowledgement, produced by the very
mechanism added to prevent duplication. Guarding one clause of an invariant with a mechanism that
breaks another is worse than guarding neither, because the record would claim coverage.

### D1d — A caller-level retry is a new obligation; only the internal re-commit reuses identity

The two retries in this design are different operations and conflating them decides the duplication
question in the wrong direction:

- **Internal re-commit** — the same _already-produced_ row is committed again after a transient or
  ambiguous failure. It reuses the identity. This is what D1c deduplicates.
- **Caller-level retry** — the caller reissues a dispatch that did not return success. The verb
  executes again and **produces a new row with a new identity**.

The second is correct rather than a leak. If a dispatch did not return success, no obligation was
acknowledged to the caller; when they reissue, the work is genuinely performed a second time and
accounting follows work performed. This covers the lifecycle _produce row, process dies before commit,
caller retries_: the post-crash records are correctly distinct, not a duplicated obligation.

The boundary is therefore: **identity is per produced row, and de-duplication covers only the
re-commit of a row that was already produced.** Stated because it was previously implied, and an
implementation that minted identity per logical caller operation instead would under-count genuine
repeated work while believing it was preventing duplication.

### D1b — This is intra-process group commit, and the scope is load-bearing

**Lane note (ADR-134 D2a).** "One connection" describes the mechanism's scope, not a global
count: once ADR-134's posture target lands, obligation-bearing rows commit on a second,
pool-owned connection running a durable-sync posture, and this shared connection carries pure
observability rows only. Lane selection rides D1's classification at enqueue, so a batch on
either connection is single-lane by construction — a mixed batch on the shared writer would
bypass ADR-134's durability guarantee while passing every invariant here, which is why the
split is stated in both records. Each lane runs this same group-commit mechanism on its own
connection.

The batching in D1 is one daemon process committing, on **one connection per lane**, rows
produced by the many concurrent dispatches it is serving. Those dispatches are tasks inside one process, so
sharing a transaction between them is ordinary intra-process group commit and it is available.

It is **not** group commit across client processes. SQLite has no cross-process commit
coordinator: separate client processes serialise on the WAL write lock rather than sharing an
fsync, so "batch N client processes' transactions into one fsync" is not available on this
substrate at all. Any record proposing that must first introduce a single writer process that owns
the store, which moves acknowledgement across a process boundary and reopens durability as a
design question rather than a tuning parameter. That is explicitly out of scope here.

The distinction has a deployment consequence that must not be assumed away: **the reduction is
per-process**. A deployment running M daemon processes against one store file gets M independent
batching domains, and contention between those domains is untouched by this record. Whether a
given deployment is one daemon or many is a property of that deployment, and the group-commit
follow-on inherits the multi-daemon case.

### D2 — A classifier at the event-production seam decides commit-failure handling

An earlier draft used the classifier to decide **routing** in the sense of which rows may be
batched at all. That question is gone under D1 — every row is batched, and no row is deferred
past its own return — but a narrower routing decision exists again under ADR-134 D2a:
classification selects the **lane**, and with it the committing connection, at enqueue
(obligation-bearing rows to the durable-sync writer, pure observability rows to the shared
writer). Within a lane there is still nothing to route away from: every row in the lane rides
that lane's committed (store-visible) batch, and on the success path every dispatch waits for
it. D1's carve-out — a pure-observability row whose commit fails persistently — remains a
failure outcome rather than a routing choice.

The classifier therefore has exactly two jobs, decided at the same seam: lane selection
(ADR-134 D2a) and **what a failed commit does to the caller**:

- **Accounting-, authorization-, and security-audit-bearing rows.** A dispatch must not report
  success when the record that accounts for, authorizes, or audits it did not commit. Reporting
  success over an uncommitted accounting row is precisely the loss INV-1 forbids, so for this class a
  persistent commit failure fails the dispatch. This is a deliberate, scoped reversal of the
  existing best-effort contract that a store write failure never fails the verb it audits.
- **Pure observability rows.** Failing a caller's verb because an observability row could not be
  written is wrong. Here the best-effort behaviour is correct, and the failure goes to
  instrumentation (D8) rather than to the caller.

**Why this cannot be deferred behind monitoring instead.** The other loss modes announce themselves.
A dropped record leaves a gap countable against a known input. A record lost to power failure leaves
a gap correlated with an incident.

A falsely acknowledged one leaves nothing anomalous at all, and the reason is structural rather than
a matter of difficulty. **Detection requires a disagreement between two records, and this failure
produces agreement.** The caller wrote down success; the store has no row; and no reconciliation
between them can fire, because reconciliation compares what the two sides _say_, and they say the
same thing. The missing row is visible only against an external truth the system does not have.

So it is not that this mode is hard to detect. There is no observable to detect it with, and no
amount of additional monitoring creates one. A mechanism that _could_ create one does exist, and the
distinction matters enough to state: an independently committed receipt, reconciled by identity
against the audit row, would supply the missing disagreement. But that is a second authoritative
write path, not a monitor, and it carries the same acknowledgement problem one level further out. It
does not remove the need for the guarantee, it relocates it. That is why the guarantee belongs in the
write path: failing the dispatch at the moment its commit fails is the only thing that closes the
mode without recreating it somewhere else.

It also means a monitoring-based mitigation here is **actively harmful when it is treated as
coverage**, and merely insufficient otherwise. The record should not overstate that: a metric
counting observed flush failures is a useful non-authoritative supplement, and D8 specifies one.
What makes the harmful reading the likely one rather than a hypothetical misuse is that this
instrument's clean output is indistinguishable from a covered system. Nothing in a green panel says
_this cannot see the class you are asking about_, so remembering it falls to every future reader, in
every later incident, indefinitely. That is not a place to put a safety property.

`EventKind` alone is not a valid discriminator for this, because one `Audit` kind covers both
classes: ADR-103 routes accounting through the per-dispatch audit row's `resource` payload, so the
deciding property lives at payload level while `EventKind` lives one level above it. A check run at
a level where the deciding property does not appear cannot decide anything, and it passes in the
same shape as a check that works.

The classifier is a total function over the classification input, implemented as an **exhaustive
match with no wildcard arm**, so adding a variant fails to compile until its class is stated.

### D3 — An accounting-bearing dispatch does not return before its accounting row is committed (store-visible)

A dispatch whose audit row carries an accounting payload (`resource.units` and the associated
`cost_unit` / `cpu_us` fields per ADR-103) does not report success until that row is committed
(store-visible). Whether that commit also survives an OS crash or power loss is a distinct
property, governed by the store's `synchronous` posture — see ADR-134, in particular its INV-3
prerequisite before any consumer may depend on the row for a resource-usage outcome.

Under D1 this is satisfied by the ordinary path rather than by an exemption from it: a dispatch
already waits for its batch to commit, and for this class D1's waiting clause binds
unconditionally — the observability carve-out does not reach an accounting-bearing row. (Amendment
1's admission-pressure exception is a separate, narrower carve-out than the observability one: it
applies only to the named allowlisted read verbs and only to the two transient admission-pressure
terminal reasons, never to a persistent commit failure.) An earlier draft achieved the same
property by
excluding accounting rows from batching altogether, which would have removed the acquisition
reduction in exactly the high-concurrency deployment that motivates this record, since under
ADR-103 the usage object rides _every_ per-dispatch audit row. Stating the obligation instead of
the exemption keeps the guarantee and keeps the benefit.

What remains specific to this class is failure handling, which is D2's subject, and the store's own
durability posture, which is neither.

ADR-103 already freezes the usage object _before_ the enclosing audit row is written
(`ADR-103:814-822`), so this is compatible with its stated ordering and requires no amendment to
it. This record does not change what accounting means, where it lands, or who reads it.

### D4 — INV-1: accounting, authorization, and security-audit records are never lossy

A record whose correctness affects an accounting computation, an authorization decision, or a
security-audit obligation is written **exactly once**. Four failure modes are forbidden, and they
are listed rather than summarised because each needs its own check:

1. **Dropped** — written through a path that can discard it.
2. **Deferred** — still volatile when its operation returns.
3. **Falsely acknowledged** — the operation reports success when the record did not commit.
4. **Duplicated** — written more than once, so the obligation is counted twice.

Not revisable by configuration, environment, or feature flag.

The fourth matters because D1a's retry could otherwise satisfy the first three while
double-counting. It is the only one of the four that errs against the accounted party rather than
in their favour, and the only one that is not detectable from the accounting records after the fact. An
invariant against loss is not an invariant about correctness.

### D5 — INV-2: unclassified resolves to the stricter handling, enforced by exhaustiveness

An input with no explicit classification is treated as **obligation-bearing**: a failed commit
fails its dispatch. The mechanism is the absence of a wildcard arm in D2's match rather than a
documented convention. A new variant without an explicit arm is a compile error, not a silent
default.

Fail-closed in this direction because the cost of wrongly strict is a caller seeing an error it
could have been spared, and the cost of wrongly lenient is an accounting or authorization record
silently lost while its operation reports success.

### D6 — Read-flag mutation is batched; acknowledgement remains best-effort

A bulk form taking a list of ids becomes the primary surface for sweeping an inbox, collapsing N
dispatches into one. Collapsing N statements into one transaction is a separate, opt-in property
rather than a consequence of batching: `comm.mark_read(atomic=true)` commits every unique mark in
one transaction or none, while the default `atomic=false` path — and `comm.read(ids=...)`, whose
behaviour it matches — applies each patch independently.

**Plain `comm.read` attempts the flag write before returning, but the patch is best-effort.** A
successful patch returns `read: true`. Writer contention, a storage error, or a row that disappears
between fetch and patch does not discard the successful fetch: the response instead returns
`read: false` plus `mark_error`, and the message remains unread. This is the contract established by
ADR-040's 2026-08-01 `comm.read` amendment and implemented by
`crates/khive-pack-comm/src/handlers.rs::read_response`; batching does not strengthen that
acknowledgement into an unconditional durability guarantee.

The unread surface remains load-bearing for the inbox monitor's stale check. Consequently, an inbox
sweep MUST count only per-item responses with `read == true` as acknowledged and SHOULD retry rows
that carry `mark_error`; dispatch success alone does not establish that the flag became
store-visible. A bulk form preserves these per-item outcomes. It reduces dispatches
unconditionally; it reduces write acquisitions only on the `atomic=true` path, since the default
per-item path still takes one write acquisition per row.

### D7 — Serve-ledger writes are batched per call

`brain.record_serve` writes for a single recall are accumulated and written in one transaction
rather than one per returned target, converting a per-result acquisition into a per-call one.

### D8 — Writer instrumentation at the acquisition site

Counters for writer acquisitions, acquisition timeouts, and flush failures are gathered at the
acquisition site rather than at call sites, so they stay correct when a verb is added without
anyone classifying it.

This is a precondition rather than a follow-up: the audit append currently swallows its own
failures by contract, so observed failure counts are a lower bound by construction and present
instrumentation cannot see the population this record shrinks.

## Invariants

- **INV-1 (D4), system-wide.** Accounting-, authorization-, and security-audit-bearing records are
  written exactly once: never dropped, never volatile at return, never falsely acknowledged, never
  duplicated. This is stated as a system-wide invariant rather than a store-local one because the
  same failure mode can arise independently across multiple mechanisms and stores — an audit row
  proposed for a lossy plane, a usage meter dropping accounting events at a bounded channel and
  again at its store write in the serving layer, a commit failure reported to the caller as
  success, and a retry able to write the same accounting payload twice. A property that
  four independent mechanisms can break is not local to any of them.

  **Note on scope.** INV-1 governs how the _write path_ handles a record — whether it can be
  dropped, deferred past its operation's return, or reported as successful without committing. It
  does not by itself establish that the underlying store's durability settings are adequate for
  the records it holds. That is a separate property and the two must not be conflated: a record
  handled correctly by the write path, on a store configured to lose recent commits, is still
  exposed. Both halves have to hold, and satisfying one says nothing about the other.
- **INV-2 (D5).** An unclassified input resolves to the stricter handling — commit failure fails
  the dispatch — enforced by exhaustive matching without a wildcard.
- **INV-3.** Batch accumulation is bounded and never blocks dispatch indefinitely; a full batch
  forces a commit rather than growing without limit, and the time threshold bounds the wait when
  the writer is idle.
- **INV-4.** No write becomes weaker **than the current implementation's behaviour in the same
  situation**. That comparison basis is the whole content of the invariant and was previously left
  implicit. Batching changes _when_ the writer is acquired and _how many rows share one
  acquisition_, never whether a row survives a case the current implementation would have survived.

  Read against D1's scope: for an obligation-bearing row this is strictly stronger than today, since
  a dispatch returns only after its row commits and a persistent failure fails it, where today it
  would silently succeed. For an observability row on persistent commit failure the outcome is
  **identical** to today — the current code swallows a failed append and returns success — so
  nothing became weaker there either. An invariant phrased as "never lossy" rather than "never
  weaker than today" would forbid the current implementation's own behaviour, which is how the
  earlier universal wording of D1 became unsatisfiable.

## Consequences

**Intended.** Writer acquisitions on the read path stop scaling with request rate: from
one-per-dispatch, plus one per result for recall, to one per commit cycle. Contention becomes
measurable. Durability improves relative to today, because best-effort swallowed appends become
transactional appends with surfaced failures.

**Accepted costs.**

- **A scoped reversal of the best-effort audit contract.** Today a store write failure never fails
  the verb it audits. Under D2 that stays true for observability rows and stops being true for
  accounting-, authorization-, and security-audit-bearing rows, which now fail their dispatch
  rather than silently succeeding over a lost record. This is a visible behaviour change and is
  the intended one: the swallow is why the current failure counts are a lower bound.
- **A store outage becomes a service outage rather than unaccounted service, and this follows
  directly from the reversal above.** Under ADR-103 the usage object rides _every_ per-dispatch audit
  row, so a persistently failing writer fails every metered dispatch rather than serving them
  unaccounted. That is the correct trade — serving work whose accounting record cannot be written is
  the loss INV-1 exists to prevent — but it is a real availability consequence and it is named here
  so it is not discovered later.
- **Cross-caller coupling inside a batch.** One malformed row can fail unrelated callers' audit
  writes, which is why D1a rejects at entry rather than at commit.
- Additional memory proportional to batch size.
- **Latency, stated by case rather than as one number.** When the writer is idle a dispatch commits
  immediately and pays approximately nothing; the time threshold is an upper bound under load, not a
  cost the idle path pays. When a commit is already in flight a dispatch waits that commit's tail
  plus its own, so up to two commit durations. Under contention that is still cheaper than today,
  where each dispatch queues for an exclusive turn.

**No longer claimed.** An earlier draft accepted "audit rows become visible to queries slightly
later than the operation that produced them." Under D1's waiting clause that is not a consequence
of this record, and the read-your-own-audit caveat it carried is withdrawn.

**Explicitly not claimed.** This record does **not** claim interactive traffic becomes read-only.
It claims writer _acquisitions_ fall. Whether any verb reaches zero writes is a question the
revised census answers, not something this record asserts.

**Not addressed here.** Remaining genuine writes still serialize on one writer within each lane,
and the two lane writers (ADR-134 D2a) additionally contend on SQLite's write lock between
themselves. Raising that ceiling is the group-commit work, sequenced after this record so it is
designed against the post-change profile.

## Acceptance

1. **A committed census manifest.** The verb inventory is committed as a machine-readable
   manifest naming every verb, its writer path or paths including nested dispatch, its
   conditional assumptions, and the control assertion, pinned to a source revision. Acceptance
   compares manifests at two revisions, not two prose summaries. The existing aggregate counts
   are not sufficient: two reviewers applying the same rule to different verb sets produce
   numbers that look comparable without measuring the same population.
2. The census scanner is fail-closed (`UNKNOWN`, never `NO-WRITER`, on absent evidence) and voids
   its own run if `comm.read` does not classify as a writer.
3. A test that **no dispatch returns before its audit row is committed, on every path where D1's
   scope requires it** (D1, INV-4) — unconditionally for obligation-bearing rows, and on the
   success path for observability rows. Exercised under concurrency so the batch actually forms,
   and shipped with a fixture that returns early on purpose and must make the test fail.
4. A test that a dispatch carrying an accounting payload does not report success when its row's
   commit fails, and that an observability-only row's **persistent** commit failure does **not**
   fail its dispatch (D2, D3, INV-1). The two halves must be asserted separately; a test that only
   checks the first cannot distinguish the classifier from an unconditional rule.

   The observability half asserts the D1 carve-out and nothing wider: on the **success** path that
   dispatch must still not return before its row commits. Assert both, or the criterion licenses an
   implementation that never waits for observability rows at all — which is the batching-without-
   waiting design this record rejects, reached through a test rather than through a decision.

5. **Two independently produced obligation-bearing rows presenting the same identity cannot silently
   acknowledge one as the other** (D1c). Inject the collision. Assert that the conflict is either
   proved to be the same logical record or fails the affected dispatch, and that no dispatch
   observes success over an obligation that was not recorded. Without this, an identity generator
   that collides passes every other criterion while dropping records.
6. A compile-level demonstration that adding a classification variant without an explicit arm
   fails to build (D5, INV-2).
7. **An ambiguous commit acknowledgement, followed by a retry, persists exactly one
   accounting-bearing record** (D1c, INV-1 clause 4). The fixture injects the ambiguity — a commit
   that succeeds in the store while its acknowledgement is lost — rather than simulating a clean
   failure, because a clean failure is the easy case and is not the one that duplicates. Assert on
   the count of persisted records and on what the accounting consumer would compute, not only on the
   absence of an error.
8. **A concurrent batch of mixed validity preserves the unrelated caller** (D1a). One malformed row
   and one valid accounting row from a different caller enter together; the malformed row is
   rejected before enqueue, the valid row commits exactly once, and the valid caller's result
   follows its own record rather than the other caller's failure. The fixture must be able to fail
   at the validation or statement boundary — a fixture that can only fail the whole commit tests
   something else and passes without exercising the guarantee.
9. A test that a recall returning multiple targets performs one serve-ledger acquisition, not one
   per target (D7).
10. Writer-acquisition counters show a reduction under a fixed replayed workload — a measured
    delta under identical input, at a concurrency level high enough for batches to form. A
    single-threaded replay cannot demonstrate this decision, because a batch of one is the
    unchanged path.
11. Crash injection loses no record **for which D1 requires durability at return** — every
    obligation-bearing row, and every observability row whose commit succeeded. Scoped this way
    because D1's carve-out permits a persistently-failed observability row to be absent after its
    dispatch returned successfully, so an unscoped version of this criterion would forbid an outcome
    the record explicitly allows, and an implementer would resolve the conflict by quietly choosing
    a fixture rather than by reading a decision.

    Ship the companion fixture that makes the permitted case explicit: an observability row whose
    commit fails persistently is **intentionally absent**, its dispatch succeeded, and the failure
    reached instrumentation. Asserting the permitted outcome is what stops the exception from being
    read as a defect by the next person to touch this.

    The crash fixture itself must establish **at least one returned row and a distinct in-flight
    batch**, then verify the returned row survives. Without that, a crash during an unacknowledged
    batch with no returned dispatches satisfies the criterion having proved nothing — a pass in the
    shape of a verdict.

## Alternatives considered

**Buffer audit rows in memory and accept bounded loss.** Rejected. It trades durability for a
benefit durable batching already provides, and it collides with ADR-103, under which the audit
row is the accounting record.

**Batch without waiting — let the dispatch return and flush behind it.** Rejected. It is the
reading an earlier draft left open by not stating D1's waiting clause, and it is not a smaller
version of this decision but a different one. A returned operation whose row is still in memory can
lose a row the current implementation would have kept, since today's append is committed
(store-visible) at dispatch whenever it succeeds. That violates INV-4 directly, and for an
accounting-bearing row it violates INV-1. It also buys nothing this record does not already
obtain: the acquisition count falls because rows share a transaction, not because the caller
stopped waiting.

**Exempt accounting rows from batching to keep them durable.** Rejected, having been the previous
draft's D3. Under ADR-103 the usage object rides every per-dispatch audit row, so the exemption
would apply to every row in the deployment this record exists for, and the acquisition reduction
would evaporate exactly where it is needed. The waiting clause obtains the same durability
guarantee without the exemption.

**Split the audit row so accounting lives in its own record.** Rejected for now. It would make
the observability half freely bufferable, but it requires amending ADR-103, a migration, and it
breaks that record's property that the same object appears in the response and the audit payload.
Durable batching obtains the acquisition reduction without any of that. Revisit only if
measurement shows durable batching insufficient.

**Classify by `EventKind`.** Rejected: one `Audit` kind spans observability and accounting, so a
kind-level test cannot express the boundary INV-1 requires. This is why D2 places the classifier
at the production seam.

**Drop audit events entirely.** Rejected: they are the audit trail and the accounting record.

**Sample audit events.** Rejected: sampling changes what the audit trail means, and under ADR-103
it would change what usage gets accounted.

## Amendment 1 (2026-08-26): A Named, Bounded Exception to D4/INV-1 for Admission-Pressure Reads

**Status**: Accepted, implemented alongside PR #2228 (khive#2147/khive#2217/khive#2208).

D2 states: _"A dispatch must not report success when the record that accounts for, authorizes, or
audits it did not commit"_ (`ADR-133:297-298`), and D4/INV-1 states the same as a system-wide
invariant: _"Accounting-, authorization-, and security-audit-bearing records are written exactly
once: never dropped, never volatile at return, never falsely acknowledged, never duplicated"_
(`ADR-133:436-438`), with failure mode 3 named explicitly as _"**Falsely acknowledged** — the
operation reports success when the record did not commit"_ (`ADR-133:375`).

This amendment qualifies both sentences for one narrow, named case: the eleven read verbs on
`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS` (`crates/khive-runtime/src/pack.rs`; the full list and
rationale are in ADR-103 Amendment 3), and only when the row's own commit did not resolve before
the dispatch returned because the audit lane's admission was transiently exhausted or the caller's
bounded wait for it elapsed — `AuditTerminalReason::QueueAdmissionExhausted` or
`AdmissionDeadlineExpired`, never a persistent commit failure. For that verb set and those two
terminal reasons, the dispatch reports its already-computed successful read result without waiting
on its own audit/accounting row. The two reasons are not the same fact, though, and this amendment
does not treat them as one:

- **`QueueAdmissionExhausted`** — the row was refused before it could be enqueued. It never shares a
  generation with anyone and will never commit. This is exactly failure mode 3 (falsely
  acknowledged) for an accounting-bearing row: the dispatch reports success and the record is a
  confirmed, terminal non-commit.
- **`AdmissionDeadlineExpired`** — the row was already enqueued when the caller's bounded wait
  elapsed. It is not dropped: the generation driver still commits or terminally fails it,
  independently of the caller's timeout, so at the moment the dispatch returns its outcome is
  unresolved rather than known-lost. This amendment permits the dispatch to return before that
  resolution is known, which is still a departure from D2/D4's "must not report success before
  commit" language, but it is a weaker claim than failure mode 3 as originally defined — the record
  is not (yet) known false, only not yet confirmed.

Both are counted on separate diagnostics counters precisely so an operator can tell a confirmed
loss from an unresolved one — see ADR-103 Amendment 3.

**Why this is a scoped exception and not a reopening of D4/INV-1 generally:**

- It applies to reads only. A read verb performs no domain write; the value being protected by
  D2's "must not report success" rule is the accounting record of work already done, not
  correctness of a mutation. Losing that accounting record loses a count, not state.
- It applies to two specific terminal reasons, not persistent store failure. A persistently
  failing store still fails these dispatches exactly as D2 requires for any other
  accounting-bearing row — the exception exists only for the audit lane's own transient
  admission pressure, not for durability loss.
- It is opt-in per verb, fail-closed by default (`VerbRegistry::admission_degrade_safe`): an
  unclassified or newly added Assertive handler is NOT eligible until someone deliberately reviews
  it and adds it to the allowlist, preserving D5/INV-2's "unclassified resolves to the stricter
  handling" posture for everything not on the list.
- The resulting loss is counted, not silent — see ADR-103 Amendment 3's diagnostics requirement.

**What does not change:** D4/INV-1 continues to hold without qualification for every write, every
non-allowlisted Assertive handler, gate-denial rows, unknown-verb rows, and `git.digest` receipts.
D2's "must not report success" sentence is unqualified for a persistent commit failure on any row,
including the eleven allowlisted verbs — the exception is admission pressure specifically, not
store failure generally.

This amendment does not revisit "Split the audit row so accounting lives in its own record" from
Alternatives considered above — that remains rejected for the reasons stated there (a migration,
and it breaks the response/audit-payload identity property). The accepted trade here is a bounded,
measured undercount over that redesign.

## Amendment 2 (2026-09-01): Extending Amendment 1's Verb Set to Operational Reads

**Status**: Proposed.

Amendment 1's exception to D2 and D4/INV-1 was scoped to the eleven verbs on
`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`. ADR-103 Amendment 4 extends that list with eight
operational read verbs (`gtd.tasks`, `gtd.next`, `comm.inbox`, `comm.unread`, `comm.thread`,
`comm.delivered`, `comm.probe`, `comm.health`), each individually reviewed against the same
criterion: declared `VerbCategory::Assertive` and no write-shaped operation on the dispatch path.
The per-verb evidence table, the two named exclusions (`comm.heartbeat`, whose primary effect is
a persist; `comm.cursor_get`, whose dispatch path checks out the writer and runs a schema-ensure
script), and the incident measurement motivating the extension live in ADR-103 Amendment 4.

Nothing else in Amendment 1 changes. The exception remains:

- reads only, for the two transient admission terminal reasons only, never persistent store
  failure;
- opt-in per verb and fail-closed by default (`admission_degrade_safe`), so the eight additions
  are deliberate review products, not a loosening of the default posture;
- counted on the same two disjoint diagnostics counters, so the undercount stays measurable.

D4/INV-1 continues to hold without qualification for every write, every non-allowlisted Assertive
handler, gate-denial rows, unknown-verb rows, and `git.digest` receipts — the sentence is
unchanged; only the enumerated verb set it excepts has grown, by review.

The exception's enumerated verb set is authoritative in ADR-103 Amendment 4, which also requires
the extended census test to assert list-to-enumeration equality and per-entry handler resolution —
so a branch widening the constant without a signed amendment fails the census rather than widening
this exception silently.
