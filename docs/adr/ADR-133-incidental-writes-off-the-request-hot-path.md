# ADR-133: Reduce writer acquisitions on the request path

- **Status:** Proposed
- **Date:** 2026-07-29
- **Relates to:** ADR-094 (audit row construction), ADR-103 (resource attribution), ADR-131
  (admission control for parallel write batches)

## Context

The measurement and unamended comparisons below describe the pre-batching baseline. References to "today" or the
"current implementation" in those comparisons refer to that baseline, not revision `7d6ff8fd` (corrected by
Amendment 11). The release table and amendment-specific source notes state the revised contract and implementation.

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
whether an `EventStore` is configured. A writable served runtime configures its event store through `with_runtime_event_store`; a read-only runtime retains no event-store handle (`crates/khive-mcp/src/server/pack_setup.rs:30-41`, corrected by Amendment 11).

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
   (`crates/khive-runtime/src/pack/audit.rs:572`, corrected by Amendment 11).
2. **Read-flag mutation.** `comm.read` performs a real `UPDATE` of the message row
   (`crates/khive-pack-comm/src/handlers.rs:268`, corrected by Amendment 11), so a sweep of N messages is N
   acquisitions.
3. **Recall telemetry.** `memory.recall` persists a `RecallExecuted` event per call
   (`crates/khive-pack-memory/src/handlers/recall.rs`).
4. **Serve ledger.** `memory.recall` also dispatches `brain.record_serve` from a background task
   (`crates/khive-pack-memory/src/handlers/recall.rs:1439-1457`, corrected by Amendment 11), and that handler acquires
   `sql.writer()` once for the nonempty target batch and inserts into `brain_serve_ledger` (`crates/khive-pack-brain/src/handlers/brain_handlers.rs:1460-1473`, `crates/khive-pack-brain/src/serve_ledger.rs:121-139`, corrected by Amendment 11).

Item 4 now scales with nonempty recall calls, as D7 requires. A recall can still reach additional writers through its nested dispatch, so the census must trace that path (corrected by Amendment 11).

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
(`docs/adr/ADR-103-resource-attribution-model.md:406-410` (corrected by Amendment 11)), and the usage object _"lands under the existing per-dispatch audit row's
`resource` payload"_ (`docs/adr/ADR-103-resource-attribution-model.md:863-870` (corrected by Amendment 11)), read by four consumers including accounting and Gate
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

**A dispatch does not return before the batch carrying its audit row has committed.** (superseded by Amendment 11, Decision 1) This is the
load-bearing clause of the whole record and an earlier draft left it unstated, which made the
draft readable two incompatible ways.

**Scope of that clause, which an earlier draft got wrong by stating it without qualification.** It
binds unconditionally for **obligation-bearing rows** (accounting, authorization, security audit):
such a dispatch never returns success without its row committed, and a persistent commit failure
fails it. (superseded by Amendment 11, Decision 1) Amendment 1 below carves out one narrow, named exception to this binding: for the
`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS` read set, and only when the row's own commit did not
happen because the audit lane's admission was transiently exhausted or the caller's bounded wait
for it elapsed (never a persistent commit failure), the dispatch reports its already-computed
result without waiting on that row. Amendment 10 carves out a second: the success rows ADR-103
Amendment 7 omits or does not select are never produced, so those dispatches return without one. (superseded by Amendment 11, Decision 1)

For **pure observability rows** it binds on the success path only. The dispatch waits for the batch
in the normal case — so observability rows are batched, share the acquisition reduction, and are
committed (store-visible) at return whenever the commit succeeds — but a **persistent** commit
failure releases the dispatch successfully and routes the failure to instrumentation. (superseded by Amendment 11, Decision 1)

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
  the dispatch returns is still committed before the dispatch returns (Amendment 10 names one
  exception: the success rows ADR-103 Amendment 7 no longer produces). INV-4 holds by construction
  rather than by assertion. "Committed" here is write-path store visibility under the store's
  configured posture; whether a committed row survives an OS crash is the store's durability
  posture, which is ADR-134's subject, not this clause's. (superseded by Amendment 11, Decision 1)
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

**Release table (Amendment 11, Decision 1).** This table supersedes the preceding unqualified waiting clauses.
It governs a produced row on the configured `AuditBatch` path. A return without confirmed commit is permitted
only with the result stated here. `Error` fails a would-be success or preserves an existing denial or handler error;
`Preserve` keeps the domain result, including an existing error. Neither result authorizes replay of a domain effect.

`Obligation` includes `GateDenied`, `DispatchSucceeded`, `DispatchFailed`, `EffectiveTargetCheck`, `UnknownVerb`
and `GitDigestReceipt`. `Eligible read` is only a successful `DispatchSucceeded` admitted by
`admission_degrade_safe`. `Pure` includes `ConfigLocked`, `RecallExecuted` and `GateUnavailable`. The exhaustive
classification is at `crates/khive-runtime/src/audit_batch.rs:74-86`; eligibility is checked at
`crates/khive-runtime/src/pack/audit.rs:584-586`.

| Result or terminal reason                | Obligation                                          | Eligible read                        | Pure                              |
| ---------------------------------------- | --------------------------------------------------- | ------------------------------------ | --------------------------------- |
| `Committed` or `AlreadyPresentIdentical` | Preserve, commit confirmed                          | Preserve, commit confirmed           | Preserve, commit confirmed        |
| `PreflightRejected`                      | Error                                               | Error                                | Preserve                          |
| `AdmissionClosed`                        | Error                                               | Error                                | Preserve                          |
| `QueueAdmissionExhausted`                | Error                                               | Success, row never enqueued          | Preserve                          |
| `AdmissionDeadlineExpired`               | Error on bounded paths; resolved paths keep waiting | Success, commit unresolved           | Preserve, commit unresolved       |
| `ResolutionDeadlineExpired`              | Error on resolved paths                             | Not reached by bounded submission    | Not reached by bounded submission |
| `IdentityConflict`                       | Error                                               | Error                                | Preserve                          |
| `StoreFailure`                           | Error                                               | Error                                | Preserve                          |
| `RetryExhausted`                         | Error                                               | Error; implementation differs below  | Preserve                          |
| `IdempotencyUnsupported`                 | Error                                               | Error                                | Preserve                          |
| `DriverPanicked`                         | Error                                               | Error                                | Preserve                          |
| `DriverCancelled`                        | Error                                               | Error                                | Preserve                          |
| `DriverJoinLost`                         | Error                                               | Error                                | Preserve                          |
| `DriverExitedInconsistent`               | Error                                               | Error                                | Preserve                          |
| `DriverAppendAbandoned`                  | Error if still awaiting notification                | Error if still awaiting notification | Preserve                          |
| `StoreWedged`                            | Error                                               | Error                                | Preserve                          |

Every terminal-reason row permits release without a confirmed commit; the success outcomes require a confirmed
insert or identical prior record. Refusal and expiry differ: `QueueAdmissionExhausted` never enqueues the row;
`AdmissionDeadlineExpired` and `ResolutionDeadlineExpired` leave it with the driver. An identity conflict does not
prove that the submitted logical record exists. A failed or interrupted append can leave the commit outcome unknown.
The terminal variants are defined at `crates/khive-runtime/src/audit_batch.rs:93-219`; idempotent equality is checked
at `crates/khive-db/src/stores/event.rs:521-533`.

Successful non-allowlisted dispatches and `GitDigestReceipt` use resolved waiting. Other producers use bounded
submission. The default admission wait is 5 s. Resolved waiting adds up to 30 s after that threshold, for a total
of 35 s, while the driver has its own 90 s bound. A caller can therefore finish before an abandoned generation's
notification, which does not change that caller's earlier result. These are audit-wait bounds, not end-to-end
request latency guarantees (`crates/khive-runtime/src/pack/audit.rs:483-491`,
`crates/khive-runtime/src/pack/audit.rs:604-609`, `crates/khive-runtime/src/audit_batch.rs:305-315`,
`crates/khive-runtime/src/audit_batch.rs:881-920`, `crates/khive-runtime/src/audit_batch.rs:671-672`).

A pure-observability submission waits at most the admission bound, including when the append has not failed.
Its audit outcome does not fail an otherwise successful dispatch. A `GateUnavailable` dispatch remains failed
closed. Background `RecallExecuted` submission does not delay the already-computed recall response
(`crates/khive-runtime/src/pack/audit.rs:681-688`, `crates/khive-runtime/src/pack/dispatch.rs:603-633`,
`crates/khive-pack-memory/src/handlers/recall.rs:1435-1483`).

**Implementation discrepancy.** At `7d6ff8fd`, an eligible read also succeeds on `RetryExhausted`
(`crates/khive-runtime/src/pack/audit.rs:661-671`). Amendment 1 names only `QueueAdmissionExhausted` and
`AdmissionDeadlineExpired`. This table retains that two-reason boundary; it does not authorize the extra release.

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
failure outcome rather than a routing choice. (superseded by Amendment 11, Decision 1)

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
the exemption keeps the guarantee and keeps the benefit. (superseded by Amendment 11, Decision 1)

What remains specific to this class is failure handling, which is D2's subject, and the store's own
durability posture, which is neither.

ADR-103 already freezes the usage object _before_ the enclosing audit row is written
(`docs/adr/ADR-103-resource-attribution-model.md:863-870` (corrected by Amendment 11)), so this is compatible with its stated ordering and requires no amendment to
it. This record does not change what accounting means, where it lands, or who reads it.

### D4 — INV-1: accounting, authorization, and security-audit records are never lossy

A record whose correctness affects an accounting computation, an authorization decision, or a
security-audit obligation is written **exactly once**. Four failure modes are forbidden, and they
are listed rather than summarised because each needs its own check:

1. **Dropped** — written through a path that can discard it.
2. **Deferred** — still volatile when its operation returns.
3. **Falsely acknowledged** — the operation reports success when the record did not commit.
4. **Duplicated** — written more than once, so the obligation is counted twice. (superseded by Amendment 11, Decision 1)

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
`crates/khive-pack-comm/src/handlers/read_marking.rs:520` (corrected by Amendment 11); batching does not strengthen that
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
  four independent mechanisms can break is not local to any of them. (superseded by Amendment 11, Decision 1)

  **Note on scope.** INV-1 governs how the _write path_ handles a record — whether it can be
  dropped, deferred past its operation's return, or reported as successful without committing. It
  does not by itself establish that the underlying store's durability settings are adequate for
  the records it holds. That is a separate property and the two must not be conflated: a record
  handled correctly by the write path, on a store configured to lose recent commits, is still
  exposed. Both halves have to hold, and satisfying one says nothing about the other.

  **Named exceptions.** Three bounded exceptions to INV-1 are recorded in this document, and there
  are no others (superseded by Amendment 11, Decision 1). Amendment 1 scopes an admission-pressure undercount to allowlisted read verbs.
  Amendment 10 omits or samples the success rows of six named verbs (ADR-103 Amendment 7).
  Amendment 6 adds a wedged-store path that applies to every producer class: when a generation's
  append does not return within `driver_append_deadline` (3x `resolution_deadline`), the driver
  abandons the generation. If the detached append later commits, its rows are durable but no
  waiter observes them; if it fails permanently, they are lost. Either way the loss path is never
  silent: the generation snapshot carries `terminal_reason = DriverAppendAbandoned` with
  `committed_rows = 0`, `flush_failures` increments, and pure producers record degradation. An
  operator reads that snapshot as a store that stopped answering for longer than the bound, not as
  a write-path defect, and treats the snapshot's row count as the upper bound on what was lost. The
  same amendment's cap on outstanding abandoned appends (`max_abandoned_appends`) adds a second,
  same-family sub-path rather than a third exception: once that many appends are already
  outstanding, a further generation is shed without an append ever being attempted, so its rows are
  lost with certainty rather than merely possibly, upon the same non-silent surfacing —
  `terminal_reason = StoreWedged`, `flush_failures` increments, and pure producers record
  degradation exactly as they do for `DriverAppendAbandoned`. (superseded by Amendment 11, Decision 1)
- **INV-2 (D5).** An unclassified input resolves to the stricter handling — commit failure fails
  the dispatch — enforced by exhaustive matching without a wildcard.
- **INV-3.** Batch accumulation is bounded and never blocks dispatch indefinitely; a full batch
  forces a commit rather than growing without limit, and the time threshold bounds the wait when
  the writer is idle.
- **INV-4.** No write becomes weaker **than the current implementation's behaviour in the same
  situation**. That comparison basis is the whole content of the invariant and was previously left
  implicit. Batching changes _when_ the writer is acquired and _how many rows share one
  acquisition_, never whether a row survives a case the current implementation would have survived.
  Amendment 10 records one deliberate exception (superseded by Amendment 11, Decision 1): the success rows ADR-103 Amendment 7 omits or does
  not select are no longer produced, so for those dispatches a row the current implementation
  commits is not written at all. (superseded by Amendment 11, Decision 1)

  Read against D1's scope: for an obligation-bearing row this is strictly stronger than today, since
  a dispatch returns only after its row commits and a persistent failure fails it, where today it
  would silently succeed. For an observability row on persistent commit failure the outcome is
  **identical** to today — the current code swallows a failed append and returns success — so
  nothing became weaker there either. An invariant phrased as "never lossy" rather than "never
  weaker than today" would forbid the current implementation's own behaviour, which is how the
  earlier universal wording of D1 became unsatisfiable. (superseded by Amendment 11, Decision 1)

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
of this record, and the read-your-own-audit caveat it carried is withdrawn. (superseded by Amendment 11, Decision 1)

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
   and shipped with a fixture that returns early on purpose and must make the test fail. (superseded by Amendment 11, Decision 1)
4. A test that a dispatch carrying an accounting payload does not report success when its row's
   commit fails, and that an observability-only row's **persistent** commit failure does **not**
   fail its dispatch (D2, D3, INV-1). The two halves must be asserted separately; a test that only
   checks the first cannot distinguish the classifier from an unconditional rule.

   The observability half asserts the D1 carve-out and nothing wider: on the **success** path that
   dispatch must still not return before its row commits. Assert both, or the criterion licenses an
   implementation that never waits for observability rows at all — which is the batching-without-
   waiting design this record rejects, reached through a test rather than through a decision. (superseded by Amendment 11, Decision 1)

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
    a fixture rather than by reading a decision. (superseded by Amendment 11, Decision 1)

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
it would change what usage gets accounted. (Amendment 10 accepts sampling for one scoped case: the
success rows of two channel-maintenance subhandlers, each kept row marked with its rate.)

## Amendment 1 (2026-08-26): A Named, Bounded Exception to D4/INV-1 for Admission-Pressure Reads

**Status**: Accepted, implemented alongside PR #2228 and extended across the reviewed
cross-pack Assertive surface for khive#2217 (khive#2147/khive#2208).

D2 states: _"A dispatch must not report success when the record that accounts for, authorizes, or
audits it did not commit"_ (`docs/adr/ADR-133-incidental-writes-off-the-request-hot-path.md:356-357` (corrected by Amendment 11)), and D4/INV-1 states the same as a system-wide
invariant: _"Accounting-, authorization-, and security-audit-bearing records are written exactly
once: never dropped, never volatile at return, never falsely acknowledged, never duplicated"_
(`docs/adr/ADR-133-incidental-writes-off-the-request-hot-path.md:497-499` (corrected by Amendment 11)), with failure mode 3 named explicitly as _"**Falsely acknowledged** — the
operation reports success when the record did not commit"_ (`docs/adr/ADR-133-incidental-writes-off-the-request-hot-path.md:436` (corrected by Amendment 11)).

This amendment qualifies both sentences for one narrow, named case: the 70 read-verb pairs at `7d6ff8fd` (corrected by Amendment 11) on
`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS` (`crates/khive-runtime/src/pack/registry_access.rs:456-553`; the full list and
rationale are in ADR-103 Amendment 3), and only when the row's own commit did not resolve before
the dispatch returned because the audit lane's admission was transiently exhausted or the caller's
bounded wait for it elapsed — `AuditTerminalReason::QueueAdmissionExhausted` or
`AdmissionDeadlineExpired`, never a persistent commit failure. Membership in that verb set is
itself bound to the exact pack that registered the handler, not the verb name alone (ADR-103
Amendment 3) — a handler under a different pack never qualifies for this exception, no matter what
it is named. For that verb set and those two
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

**What does not change:** D4/INV-1 continues to hold without qualification (superseded by Amendment 11, Decision 1) for every write, every
non-allowlisted Assertive handler, gate-denial rows, unknown-verb rows, and `git.digest` receipts.
(Amendment 10 later qualifies both clauses for the success rows of ADR-103 Amendment 7's six verbs:
"every write" for the two channel-maintenance subhandlers, and "every non-allowlisted Assertive
handler" for `comm.cursor_get`, an Assertive handler off the allowlist whose success row it omits.)
D2's "must not report success" sentence is unqualified for a persistent commit failure on any row,
including the 70 allowlisted pairs at `7d6ff8fd` (corrected by Amendment 11), the exception is admission pressure specifically, not
store failure generally.

This amendment does not revisit "Split the audit row so accounting lives in its own record" from
Alternatives considered above — that remains rejected for the reasons stated there (a migration,
and it breaks the response/audit-payload identity property). The accepted trade here is a bounded,
measured undercount over that redesign.

## Amendment 2 (2026-09-01): Extending Amendment 1's Verb Set to Operational Reads

**Status**: Accepted (2026-09-28).

Amendment 1's exception to D2 and D4/INV-1 is scoped to the enumerated pairs (corrected by Amendment 11) on
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

D4/INV-1 continues to hold without qualification (superseded by Amendment 11, Decision 1) for every write, every non-allowlisted Assertive
handler, gate-denial rows, unknown-verb rows, and `git.digest` receipts — the sentence is
unchanged; only the enumerated verb set it excepts has grown, by review. (Amendment 10 later
qualifies both clauses for the success rows of ADR-103 Amendment 7's six verbs: "every write" for
the two channel-maintenance subhandlers, and "every non-allowlisted Assertive handler" for
`comm.cursor_get`, an Assertive handler off the allowlist whose success row it omits.)

The exception's enumerated verb set is authoritative in ADR-103 Amendment 4, which also requires
the extended census test to assert list-to-enumeration equality and per-entry handler resolution —
so a branch widening the constant without a signed amendment fails the census rather than widening
this exception silently.

As built (2026-09-28): the eight verbs' review was re-derived at `fe272269` with two instruments,
recorded in ADR-103 Amendment 4, and all eight remain eligible. The list-to-enumeration equality
this paragraph relies on is superseded: the allowlist is the census-tested set of
`(owning pack, verb)` pairs, 61 entries at `fe272269`, guarded by
`admission_degrade_safe_assertive_census_matches_live_pack_sources`
(`crates/khive-runtime/src/pack_tests.rs:382`, corrected by Amendment 11). The exception's rules above are unchanged.

## Amendment 3 (2026-09-08): The obligation error carries the domain disposition, and a post-dispatch obligation error is never retry permission

**Status**: Accepted (corrected by Amendment 11).

### The gap

D2 fails a would-be-success dispatch when its obligation row does not commit. The fold that applies
D2's pre-Amendment-3 fold discarded the domain result in that arm. The implemented fold preserves it (`crates/khive-runtime/src/pack/audit.rs:718-729`, corrected by Amendment 11). The earlier shape was:

```rust
(Ok(value), Ok(())) => Ok(value),
(Ok(_), Err(audit_err)) => Err(audit_err),
(Err(err), _) => Err(err),
```

The domain verb is dispatched before the audit append, so in the middle arm the domain effect has
usually already committed. The pre-Amendment-3 error said nothing about that effect (corrected by Amendment 11). Two
observations of this one class are on file: an `AdmissionDeadlineExpired` after a comm write had
committed (#2399) and a `StoreFailure` after a domain write had committed (#2256). The class is
defined by where the error is raised, after the dispatch, not by the reason it names.

The consequence lands on the caller. A client that retries after such an error creates a duplicate
it cannot see; a client that refuses to retry has to re-read an outcome it holds no key for. D4's
fourth forbidden mode, duplicated accounting, is pushed one layer out onto the domain records, where
nothing checks it. The policy is right: a dispatch must not report success over an uncommitted
obligation row. What was never stated is what the error must say, and what a caller may do with it.
This amendment states both. Tracked as #2424.

### A3.1 The error carries a machine-readable domain disposition

Every per-op error object on the `request` envelope, and the error field of a daemon-frame response,
carries `domain_disposition` with exactly one of three values:

- `committed`: the domain dispatch returned a success value and the error was raised afterwards by
  the obligation path (D2), or by the transport after the dispatch returned. The error object also
  carries `domain_result`, the value the fold was holding, so the caller learns the same identifiers
  a success would have returned. When that value cannot be carried (the response depth guard or the
  batch byte budget refused it), `domain_result` is omitted and `code` names the limit; the caller
  resolves that case as it resolves `unknown`. The op still reports `ok: false`; D2, D3 and D4 are
  unchanged.
- `not_committed`: the error was raised by the envelope layer before the op was handed to a pack:
  an unknown verb, a request that failed parameter validation or `$prev` resolution, a conflict
  refusal, the permission refusal of an internal subhandler, and every aborted chain entry (an op
  never dispatched because an earlier op in its chain failed). No domain effect exists. The value
  is assigned by raise site, never by error variant: `InvalidInput` is raised by pack handlers as
  well as by parameter validation, and the secret gate is the standing example: it is invoked inside
  handlers (`crates/khive-runtime/src/operations.rs`, the code and git ingest
  handlers) at points where a handler may already have written, so a `SecretDetected` raised there
  is `unknown`. One pack-raised class joins `not_committed`: a refusal the handler asserts wrote
  nothing and that names the existing record, such as a keyed create-if-absent that found the
  holder (`key_conflict`, the first member); the handler asserts it and arm 5 covers it. A handler
  whose gate provably runs before every write on a path may claim `not_committed` for that path by
  name, in a later amendment.
- `unknown`: everything else. A domain handler's own error, an error the transport could not
  deliver, a daemon `version_mismatch`. The caller cannot learn the disposition from the error and
  must resolve it another way.

Three values, not two, because a caller reads an absent or two-valued field as `not_committed`
exactly when the runtime could not tell, which reinstates the defect with more confidence attached.
The field is present on every error object; absence is a defect, not a fourth state. In addition,
every failed per-op entry carries the same required `domain_disposition` beside `ok: false`
(#2951). Both fields come from the canonical disposition; promoting arbitrary input metadata into
commit evidence is forbidden. This additive envelope field preserves `ok`, audit-obligation
failure semantics and summary counts. An aborted
chain entry carries no error object today (`ok: false`, `aborted: true`, `message`); it carries
`domain_disposition: "not_committed"` at the entry level.

`runtime_error_value` renders obligation errors as a structured object with `kind: "obligation"`,
`code` set to the terminal reason (`admission_deadline_expired`, `store_failure`, and the other
`AuditTerminalReason` values in snake case), `message`, `domain_disposition: "committed"` and
`domain_result`. An obligation path that fails before it has a terminal reason (the git digest
receipt path's setup branches: no event store, no deferred audit, a malformed receipt payload)
renders the same object with `code` set to that path's own failure code in snake case
(`git_digest_receipt_failure`) and the branch named in `message`; it is `committed` because the
digest report exists, and `domain_result` carries it. Any handler-owned receipt path that bypasses
the fold is in scope of this amendment and renders through the same serializer. Errors serialized as a bare string before Amendment 3 keep their message and gain the field
(corrected by Amendment 11) by becoming an object of the shape `{"kind": "...", "message": "...", "domain_disposition": "..."}`.

Wire rule. Before Amendment 3, `error` was a string at some sites (corrected by Amendment 11) (dispatch failures, the conflict and `$prev`
refusals, the internal-subhandler permission refusal, the batch byte budget) and an object at others
(the storage and unavailable contexts, the depth guard). String to object is a shape change for
every consumer that reads `error` as text; the message stays at `error.message`. Presentation is
applied to the `result` field only and never touches an error entry, so the two fields survive every
presentation mode and per-op override on the canonical machine result. A client that reshapes
entries on its own side is outside this guarantee; the canonical request/outcome carrier for the
Python client is separate work and cites this record.

On the daemon frame the top-level `error` stays text, because unchanged peers deserialize it as a
string; the frame gains an additive optional `error_detail` object of the same shape as the per-op
error object (`kind`, `code`, `message`, `domain_disposition`, and `domain_result` when present),
which the MCP side maps into its error data and the Python client exposes. No protocol version
change for this amendment; a peer that ignores `error_detail` sees what it saw before.

### A3.2 A post-dispatch obligation error is UNKNOWN to the caller and never retry permission

The consumer rule, stated once here so every client inherits it rather than rediscovering it: an
error whose `domain_disposition` is not `not_committed` is not permission to resend the request.
`committed` is resolved from the error itself, using `domain_result`. `unknown` is resolved only by
an outcome re-read keyed on an identity the caller chose before the call; ranked recall and inbox
listings are not absence oracles. No reason string, code or message text promotes an error into a
retry decision. The Python client exposes the field on the per-op error object and raises nothing
new for it.

This amendment does not supply the caller-chosen identity; that is a separate record. Response loss
before the fold and every `unknown` disposition stay genuinely unknown until it lands.

### Acceptance

1. **Committed arm.** A dispatch that writes a domain row and whose obligation row is forced to fail
   returns `ok: false`, `domain_disposition: "committed"`, and a `domain_result` carrying the id of
   the row; the row exists in the store. On the pre-amendment code the same test fails because the
   field is absent (run the mutation, quote both results).
2. **Not-committed arm.** An unknown verb, an invalid-parameter request and the permission refusal
   of an internal subhandler each return `domain_disposition: "not_committed"` and leave `stats()`
   unchanged.
3. **Unknown arm.** A handler that returns its own error yields `domain_disposition: "unknown"`
   and no `domain_result`; a secret-gate refusal raised inside a handler yields the same.
4. **Degrade path unchanged.** An admission-degrade-safe read under the two transient reasons of
   Amendment 1 still returns `ok: true` with its result; no disposition field appears on a success.
5. **Every error carries the field.** A test walks every error constructor reachable from the
   envelope, the exhaustive match in `runtime_error_value` and the envelope-layer sites that build an
   error entry without it (the depth guard, the batch byte budget, the conflict, `$prev` and
   permission refusals, the aborted-entry constructor), and asserts the field is present; adding an
   error variant or an envelope-layer site without a disposition fails to compile or fails this test.
6. **Three surfaces, one shape.** The MCP `request` tool, the daemon frame and the Python client
   return the same error object for the same forced failure; the Python client preserves the two
   new keys without validation errors.
7. **The caller that retried anyway.** After a committed-arm response, replaying the identical
   request produces a second domain record, and the test asserts that second record exists. This
   arm documents today's behaviour honestly; it flips to a refusal when the caller-chosen identity
   record lands, and stays in the suite until then.
8. **Aborted entries.** A chain whose first op fails returns every later entry with `aborted: true`
   and `domain_disposition: "not_committed"`; the summary counts them as aborted, not failed.
9. **Consumers of the error field.** Before the object form replaces a string at any site, the
   implementation enumerates the consumers of `error` outside this repository (the lionagi v1
   client, autopipe, khive-cloud, the inbox monitor probe) and shows each parses the object form,
   quoting the read site. A consumer that reads the text keeps the string at `error` and the object
   lands at `error_detail` instead; the implementation PR states which of the two it shipped.

### Implementation notes

- `fold_audit_obligation` keeps the value in the obligation arm and returns an error that owns it:
  a `RuntimeError` variant wrapping the audit reason and the domain value (name is implementation
  freedom; the wire fields are not).
- `runtime_error_value` in `crates/khive-mcp/src/server/dispatch_helpers.rs:392-397` delegates to the shared projection in `crates/khive-runtime/src/error_projection.rs:17` (corrected by Amendment 11); `not_committed` is assigned at the envelope-layer raise
  sites and in the aborted-entry constructor, never by matching a variant (`SecretDetected` maps to
  `unknown`); the daemon response path uses the same serializer.
- `python/khive/envelope.py`: `_validate_op_errors` admits the two keys and exposes them; no
  behaviour change for callers that ignore them.

<!-- Amendment numbers are allocated in merge order, not by date: Amendment 3 was
accepted on main while Amendments 4 to 6 were in flight, and it keeps its number so
existing citations stay valid. Read them in numeric order. -->

## Amendment 4 (2026-08-30): Resolve Enqueued Audit Outcomes for Committed Successes

**Status**: Accepted, implemented for khive#2256.

Amendment 1 deliberately lets an admission-degrade-safe read return when its already-enqueued
audit row crosses `admission_deadline`. That same return rule is not sound for a successful
non-degrade-safe operation: its domain effect may already be committed, so converting the audit
wait timeout into a generic dispatch failure reports the opposite of what happened and invites an
unsafe retry of a non-idempotent verb.

Successful `DispatchSucceeded` operations outside the read-degrade allowlist, plus the dedicated
`GitDigestReceipt` producer, therefore use the batch seam's resolved-wait mode. They enqueue and
share generations exactly as before. If `admission_deadline` elapses after enqueue, the runtime
records a warning and keeps awaiting that same receiver until the generation reports its real
commit or terminal failure; it never re-enqueues the row and never reruns the handler.

The distinction at the two admission boundaries remains exact:

- `QueueAdmissionExhausted` happens before enqueue and is still returned immediately. No audit row
  exists to await.
- `AdmissionDeadlineExpired` remains a caller-visible result of ordinary bounded `submit()` and
  continues to drive Amendment 1's read-degradation accounting. It is not returned by the
  resolved-wait mode once a committed success row has been enqueued.
- `IdentityConflict`, `StoreFailure`, unsupported idempotency, and driver terminal failures still
  fail the successful dispatch once they are known. The change removes only a false failure caused
  by an unresolved wait threshold; it does not weaken durable audit obligations.

Gate-denial, unknown-verb, failed-dispatch, and pure-observability rows retain bounded submission:
their caller-visible operation outcome is already fixed, or their contract is explicitly
best-effort. The 70 admission-degrade-safe pairs at `7d6ff8fd` remain governed by Amendment 1 (corrected by Amendment 11).

## Amendment 5 (2026-09-03): Bound the Resolved Wait Itself

**Status**: Accepted, implemented for khive#2331.

Amendment 4's resolved-wait mode kept the enqueued audit receiver alive with no upper bound once
`admission_deadline` elapsed: a stalled `EventStore::append_events_idempotent()` call (a wedged
writer task, an unreachable events daemon that never times out at that layer, and so on) retained
the completed write's caller, its request slot, and its audit-lane waiter indefinitely. The stalled
generation also stops the driver from draining any row queued behind it, so both request and audit
capacity exhaust together rather than the caller ever observing a terminal outcome.

`AuditBatchConfig` gains a second, larger bound, `resolution_deadline`, which caps how much longer
`AuditBatch::submit_until_resolved` may wait once `admission_deadline` has already elapsed on the
row. It defaults to 6x `admission_deadline` and is validated (debug-only, at `AuditBatch`
construction) to be no shorter than `admission_deadline`.

If `resolution_deadline` also elapses, the caller receives a new terminal reason,
`AuditTerminalReason::ResolutionDeadlineExpired`, distinct from `AdmissionDeadlineExpired`: the
domain effect has already committed, so it is not retried and the row is not re-enqueued, but the
audit outcome itself is now unknown to the caller rather than merely slow. The row is left exactly
where the driver holds it — in `state.pending`, or already mid-generation — for the driver to
resolve independently, the same non-removal contract Amendment 1 established for
`AdmissionDeadlineExpired`. Amendment 6 gives such a row a second possible ending: once the
driver's own bound on the generation holding it elapses, that generation is abandoned and the row
resolves as `DriverAppendAbandoned` rather than to a commit the driver observes. Every other
boundary in Amendment 4 is unchanged: `QueueAdmissionExhausted`
still returns immediately with no audit row to await, and a genuine `IdentityConflict`,
`StoreFailure`, unsupported-idempotency, or driver terminal failure that resolves before
`resolution_deadline` still fails the successful dispatch exactly as before.

## Amendment 6 (2026-09-04): Bound the Driver's Own Append, Not Just the Caller's Wait

**Status**: Accepted, implemented for khive#2331.

Amendment 5 left exactly the gap its own text names: bounding how long a _caller_ waits does not
bound how long the _driver_ holds a stalled generation. `supervisor_loop` drains rows into one
generation, spawns a child task that calls `EventStore::append_events_idempotent()`, and then
awaits that child with no timeout of its own. When the call never returns, the loop never returns
to the top to drain `state.pending` again — every submission arriving after the stall piles up in
`pending` regardless of whether its own caller is using bounded `submit()` or resolved-wait
`submit_until_resolved()`, and regardless of whether that caller has already given up. Once
`pending` reaches `max_pending_rows`, every later submission gets `QueueAdmissionExhausted`, not
just the row that started the stall — a caller giving up on its own deadline frees nothing, because
the row it gave up on is never removed from wherever the driver holds it.

`supervisor_loop` now wraps its `child.await` in `tokio::time::timeout`, bounded by a
`driver_append_deadline` derived from `resolution_deadline` (3x it, not a new config field — see
the rationale on `driver_append_deadline` in `audit_batch.rs`, which exists to guarantee the driver
can never race ahead of a `submit_until_resolved` caller's own, more specific
`ResolutionDeadlineExpired` reason for the same row). If that bound elapses, the generation is
recorded with the new `AuditTerminalReason::DriverAppendAbandoned`, every waiter on it is resolved
with that reason, and the loop continues immediately to drain whatever has queued in `pending`
since — restoring admission capacity for later callers regardless of how the stalled generation
eventually resolves. The underlying `append_events_idempotent()` call is not cancelled — it may be
a blocking storage call that cannot be safely aborted mid-write — so it is handed to a detached
task that drives it to completion and discards whatever it eventually returns; no waiter is still
listening for that result. As with `ResolutionDeadlineExpired`, the caller's already-committed
domain effect is never retried and the row is never re-enqueued. Under a store that never returns,
this mints one detached task per `driver_append_deadline` (90 s at the defaults: a 5 s
`admission_deadline`, 6x for `resolution_deadline`, 3x again for the driver) — but only up to
`AuditBatchConfig::max_abandoned_appends` (default 4) may be outstanding at once. Before spawning a
generation's child, `supervisor_loop` reads the current count of still-running
`DriverAppendAbandoned` appends; at or above the cap, the store is treated as wedged and this
generation is shed instead of attempted: no child task is spawned and no store call is made, every
waiter resolves immediately with the new `AuditTerminalReason::StoreWedged`, `flush_failures`
increments, and the loop continues straight to the next `pending` drain. A shed generation costs no
store work at all. Recovery needs no timer of its own: as soon as one outstanding append returns —
commit or failure, each recorded on a dedicated counter so an operator can see that a store recorded
as wedged later drained — the count drops back below the cap and the next generation attempts a
real append again. This bounds the retained-buffer growth a wedged store can cause to
`max_abandoned_appends * max_rows_per_generation` rows, in place of the fully unbounded growth this
amendment originally left.

## Amendment 7 (2026-09-22): operation attribution is an orthogonal pair, not a new obligation class

**Status**: Accepted.

Originating issue(s): #2049

[ADR-016](ADR-016-request-dsl.md) Amendment 1 adds `op_index`/`ref_resolution` to every audit
event produced within a request operation's scope. That pair identifies which operation in a
parsed request produced a row; it carries no argument values and supports no cross-request
argument-equality check. It changes nothing decided here: the obligation-bearing/pure-observability
classification at D2 above, the domain-disposition vocabulary this ADR's amendments already define,
and the caller-retry identity rule (a retry is a new execution and a new audit obligation, even when
it reuses `request_id`) are unaffected. `op_index`/`ref_resolution` is an attribution pair riding an
otherwise-unchanged row; it is not itself an obligation class, a domain disposition, or a basis for
collapsing two attempts' obligations into one.

## Amendment 8 (2026-10-06): Count-free event-page transport

**Status**: Accepted (2026-10-06). Acceptance is required before dependent implementation merges.

The compound read in [ADR-005](ADR-005-storage-capability-traits.md) uses a new
`QueryEventPage { protocol_version, namespace, query }` event IPC request and
`EventPageWindow { window }` response. The current protocol version and old operations
remain unchanged. The new operation participates in existing version validation,
namespace checks, reader admission, cancellation, frame limits and error transport.
An old peer that cannot decode the new operation must refuse explicitly. There is no
fallback to `QueryEvents`, which cannot preserve the compound seek contract.

`AttributedEventStore` forwards to its scoped inner store. `SplitEventStore` queries
both legacy and event-lane stores with the same predicates, seek and bounded row
request, merges in ascending physical-key order, and returns the requested prefix.
A `budget_stop` from either plane bounds the merged prefix: only rows ordered before
the earlier stop key are returned, and the merged window carries that key.
Normal producers continue to route one identity to one plane; this introduces no
cross-plane repair/deduplication. A read-only deployment without a lane must not
create a sidecar to answer the query. Equal physical time/ID keys across the merged
prefix cause an explicit refusal before truncation so a continuation cannot skip one.
Bounds and explicit refusal semantics are
defined in ADR-005; the response must also retain the transport's existing frame cap.
The blocking row's order key crosses the socket as `budget_stop` inside the
`EventPageWindow` response, never through the string-only error frame.

The read handler performs no domain mutation. `brain.event_page` joins the
admission-degrade-safe read set exactly as `brain.event_counts` does (ADR-103
Amendment 3): under audit-lane admission pressure its own dispatch audit row may be
dropped while the caller still receives the page. Nothing else about read audit
changes. Fixed window acceptance must keep subsequent reader audits outside its
population, or select disjoint event kinds. Current GTD audit payloads remain
unchanged.

Acceptance requires direct/socket/mixed-plane parity, a controlled old-peer decode
refusal proving no fallback, protocol-version rejection, namespace/row/frame guards,
and read-only missing-lane behavior. Existing audit obligation, event count/debug
and split-store tests remain applicable.

## Amendment 9 (2026-10-19): What an operator reads for an abandoned or shed generation

**Status**: Accepted (2026-10-19).

The INV-1 "Named exceptions" paragraph and Amendment 6 tell an operator to read an abandoned or
shed generation from its generation snapshot: `terminal_reason = DriverAppendAbandoned` or
`StoreWedged`, `committed_rows = 0`, and the snapshot's row count as the upper bound on what was
lost. That snapshot is retained only in builds compiled with `test` or the `test-internals`
feature. In an ordinary build the per-generation record is discarded, so no operator surface
carries a per-generation terminal reason or row count. This amendment replaces that operator
instruction. The abandonment, shedding and non-retry behavior those sections define is unchanged.

An operator reads, instead:

1. **The submitting call's result.** Every waiter on an abandoned generation receives
   `Err(AuditTerminalReason::DriverAppendAbandoned)`, and every waiter on a shed generation
   receives `Err(AuditTerminalReason::StoreWedged)`. How a producer surfaces that reason follows its
   class under D2 and Amendments 3 to 5.
2. **`db_diagnostics` writer-contention fields.** `audit_batch_flush_failures` increments once per
   abandoned or shed generation. It also counts the other terminal non-commit outcomes, so it counts
   failed generations of every kind, not this path alone. `audit_degraded_rows` counts the
   pure-observability rows released on these paths. On the abandon and shed paths `audit_degraded`
   is set only through such a row: a generation holding only obligation-bearing rows increments
   `audit_batch_flush_failures` and leaves `audit_degraded` as it was.
3. **The process log.** The driver writes one warning per abandoned generation ("audit generation
   append exceeded the driver's own bound; abandoning this generation ...") and one per shed
   generation ("audit store treated as wedged ..."). Neither line carries a row count.
4. **In-process health metrics.** An embedder holding the batch (`audit_batch_handle()`) reads
   `AuditBatch::health_metrics()`: `flush_failures`, `degraded_rows` and `degraded` as above, plus
   `late_append_commits` and `late_append_failures`, which count detached abandoned appends that
   later committed or failed. These two counters are not carried into `db_diagnostics`.

**Loss bound.** A generation drains at most `max_rows_per_generation` rows (default 256), so each
abandoned or shed generation loses at most that many. The number of such generations is the count
of the two warning lines above. An abandoned generation whose detached append later commits (one
`late_append_commits` increment) lost nothing. A shed generation is lost with certainty. The
generation snapshot is not the instrument for this bound in an ordinary build.

## Amendment 10 (2026-10-20): Success Rows of Six Named Verbs

**Status**: Proposed

ADR-103 Amendment 7 proposes omitting the audit rows of most successful dispatches of six named verbs (corrected by Amendment 11). Under that proposal,
`comm.inbox`, `comm.probe`, `comm.cursor_get` and `whoami` would write none. `comm.heartbeat` and
`comm.cleanup_expired_quarantine` would write the rows of a hash-selected one in ten, plus the row of
every call whose result reports a state change or a failed poll. Each of those rows carries
`cost_unit`, so under D3 and D4/INV-1 it is an accounting-bearing record, and for the two
subhandlers it is a write's dispatch audit. This amendment proposes a named
exception to INV-1, and INV-4: those success rows would not be produced (corrected by Amendment 11). Its status remains Proposed. The implementation at `7d6ff8fd` already contains omission and sampling in `crates/khive-runtime/src/pack/audit_success_rows.rs:217-241` and skips row construction at `crates/khive-runtime/src/pack/dispatch.rs:1044-1052`; implementation presence does not establish specification acceptance. Amendment 11, Decision 1 gives the complete exception scope.

**The exception.** It covers a dispatch whose outcome is `Success`, of a verb on ADR-103
Amendment 7's omitted or sampled constant, served by a pack registered through the trusted path,
in `reduced` mode, whose row that amendment omits or does not select. No row is produced for it,
and the dispatch returns its result without one. Each such dispatch increments one of that
amendment's per-verb counters, so the omission is counted, not silent.

**What does not change.**

- The exception is not a degrade path. An omitted or skipped row is never submitted, so D1, D2,
  Amendment 1 and Amendments 3 to 6 never see it.
- Every row Amendment 7 still writes, for these six verbs and for every other verb, keeps D1 to D5
  and INV-1 as written. The selected and forced rows of the two subhandlers are obligation-bearing:
  committed before the dispatch returns, and a persistent commit failure fails the dispatch. A
  refused admission, a failed append and an elapsed resolution deadline end such a dispatch as they
  end it today, under Amendments 3 to 6, and the row keeps the sequence number and `sampling` object
  it was given at the cut, including when the driver commits it after the caller stopped waiting. (superseded by Amendment 11, Decision 1)
- Error, denial and unknown-verb rows of the six verbs would retain their existing treatment (corrected by Amendment 11). For the four omitted
  verbs the omission covers `Success` dispatches only: every non-`Success` dispatch of `comm.inbox`,
  `comm.probe`, `comm.cursor_get` and `whoami` writes its row, unsampled.

**D4's last clause.** "Not revisable by configuration, environment, or feature flag" still holds.
The exception's verb set changes only by amendment, and `KHIVE_AUDIT_SUCCESS_ROWS` can only restore
rows: `all`, and any unrecognized value, writes every success row. No setting removes a row outside
the set.

**Sampling.** "Sample audit events" was rejected because sampling changes what the audit trail
means. That reason still holds for every other verb. For the two subhandlers, each kept row states
what it is (`resource.sampling` with `every`, `seq` and `selected_by`), readers keep counting stored
rows, and ADR-103 Amendment 7 item 5 says which totals become sampled.

**Accounting.** D3 is unchanged for every row that is written. Each omitted or skipped dispatch
would have carried a `cost_unit` of 1, since none of the six verbs is embedding-bearing
(`crates/khive-runtime/src/cost_unit.rs:29` and `crates/khive-runtime/src/cost_unit.rs:40`, corrected by Amendment 11); stored-row sums leave those units out. A
deployment that bills from `cost_unit` runs `all`, and ADR-168's aggregate rule for classes serving
quota or admission accounting is unchanged.

**Acceptance.** ADR-103 Amendment 7's arms 6, 7 and 13 are this exception's arms: error, denial
and unknown-verb rows written as today; a selected or forced row's refused admission or failed
append failing its dispatch, with its sequence number kept; the operational-read admission tests
passing in `all` mode; and the response's `usage` unchanged.

## Amendment 11 (2026-10-23) -- Audit release by producer class and terminal reason

**Status**: Proposed 2026-10-23. This draft requires specification approval before dependent implementation lands. It
does not change the status of the base ADR or of earlier amendments. The in-place status correction for Amendment 3
records its existing acceptance; Amendment 10 remains Proposed.

**Addresses**: [#3730](https://github.com/ohdearquant/khive/issues/3730).

**Trigger.** D1's unconditional waiting clause conflicts with bounded observability submission, while INV-1 and INV-4
name different exceptions. Base acceptance 4 requires observability to wait for a successful commit even after its
admission bound (`docs/adr/ADR-133-incidental-writes-off-the-request-hot-path.md:109-126`,
`docs/adr/ADR-133-incidental-writes-off-the-request-hot-path.md:513-541`,
`docs/adr/ADR-133-incidental-writes-off-the-request-hot-path.md:607-615`). The bounded submit path returns on deadline
without removing the row (`crates/khive-runtime/src/audit_batch.rs:911-920`). The release rule must distinguish
an unconfirmed audit write from a failed domain operation, and explain the retained read wait in #3730.

Source line references in this amendment and the in-place citation corrections refer to repository revision
`7d6ff8fde3a801afdc16d3cd13b6410731cb03ae`. Section and decision references identify the amended text independently
of line movement.

### Decision 1 -- One release table for D1, INV-1 and INV-4

D1's release table above is the rule for each producer class and every `AuditTerminalReason`. It supersedes the
unqualified wait statements in D1, D2, D3, D4, INV-1, INV-4 and base acceptances 3, 4 and 11. A successful obligation
waits for confirmed commit unless a named exception applies. An error may return without commit. An obligation error
after handler success carries the domain result; it does not authorize retry of that effect
(`crates/khive-runtime/src/pack/audit.rs:718-729`, `crates/khive-runtime/src/error.rs:644-651`).

Pure-observability audit submission waits at most `admission_deadline`, 5 s by default, and releases an otherwise
successful operation whether or not its row has committed or the append has failed. Earlier commit or terminal
failure resolves the wait earlier. There is no obligation carried by this class, so the writer lock must not retain
a successful operation beyond that bound. This preserves bounded submission instead of extending the wait until
commit, and preserves an actual wait instead of making all submissions return immediately
(`crates/khive-runtime/src/audit_batch.rs:305-315`, `crates/khive-runtime/src/audit_batch.rs:911-920`,
`crates/khive-runtime/src/pack/audit.rs:681-688`).

**INV-1 exception list.** For obligation-bearing rows, the exceptions and terminal departures are:

- `QueueAdmissionExhausted`: Amendment 1 permits an eligible successful read to return with no enqueued row.
  Other obligation producers return an error.
- `AdmissionDeadlineExpired`: Amendment 1 permits an eligible successful read to return while its row is unresolved.
  Other bounded obligation paths return an error; resolved paths continue waiting.
- `ResolutionDeadlineExpired`: Amendment 5 ends resolved waiting with an error and leaves the row with the driver.
- `DriverAppendAbandoned`: Amendment 6 detaches the append; its result may later be commit or failure. No domain
  effect is retried and no row is re-enqueued.
- `StoreWedged`: Amendment 6 sheds the generation without a store call. Amendment 9 identifies this as certain loss
  and specifies the production diagnostics. This is distinct from an abandoned append's unknown outcome.
- `PreflightRejected`, `AdmissionClosed`, `IdentityConflict`, `StoreFailure`, `RetryExhausted`,
  `IdempotencyUnsupported`, `DriverPanicked`, `DriverCancelled`, `DriverJoinLost` and
  `DriverExitedInconsistent`: an obligation error permits return without confirmed commit, never a new success.
- Amendment 10 proposes omitting or sampling success rows of six named verbs. Omitted rows never enter the batch and
  have no `AuditTerminalReason`. That proposal remains separate from terminal-reason degradation.

**INV-4 exception list.** The obligation-bearing departures are exactly the same list: `QueueAdmissionExhausted`,
`AdmissionDeadlineExpired`, `ResolutionDeadlineExpired`, `DriverAppendAbandoned`, `StoreWedged`, `PreflightRejected`,
`AdmissionClosed`, `IdentityConflict`, `StoreFailure`, `RetryExhausted`, `IdempotencyUnsupported`, `DriverPanicked`,
`DriverCancelled`, `DriverJoinLost`, `DriverExitedInconsistent`, and Amendment 10's proposed omission or sampling.
Pure observability is outside INV-1. For INV-4 it additionally permits a successful return before commit at the
admission bound, or on an earlier terminal failure, as the same table states. These limits replace the claim that
batching can never weaken visibility at return or retention in the same situation. Identity uniqueness and exact
payload comparison still prevent duplicate accounting; none of these exceptions permits a second copy of a row.

The reason for enumerating errors as well as successful degradation is that "never volatile at return" is stronger
than "never falsely reports success". The table states the latter precisely without hiding error exits from the
former. The implementation supplies the enum, refusal, timeout and result-fold evidence at
`crates/khive-runtime/src/audit_batch.rs:93-219`, `crates/khive-runtime/src/audit_batch.rs:842-920` and
`crates/khive-runtime/src/pack/audit.rs:718-729`. The abandoned and shed paths are at
`crates/khive-runtime/src/audit_batch.rs:954-985` and `crates/khive-runtime/src/audit_batch.rs:1026-1077`.
Amendment 9's production diagnostics replace the earlier snapshot instruction: generation retention is conditional
on `test` or `test-internals` (`crates/khive-runtime/src/audit_batch.rs:403-408`).

Base acceptance 3 now requires commit before success only on the table's commit-required paths, with concurrent
batch formation and an early-return negative fixture. Base acceptance 4 separately requires obligation failure on
persistent commit failure, preservation of successful pure-observability results on failure, and an observability
wait of at most the admission bound even when the append remains pending. Base acceptance 11 checks crash survival
for confirmed commits under the store's stated posture; the table's unconfirmed and omitted rows are not claimed
as committed. Existing identity and ambiguity acceptance remains in force.

The extra `RetryExhausted` success arm at `crates/khive-runtime/src/pack/audit.rs:661-671` disagrees with the two-reason
read exception retained here. The source test `read_survives_lease_retry_exhaustion_but_write_stays_strict` explicitly
expects that extra success (`crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:179-209`). This amendment does
not expand the exception to reconcile that discrepancy.

### Decision 2 -- Retain the bounded read audit wait requested in #3730

The 0-5 s audit wait for an eligible read stays. A successful read's produced dispatch row carries `cost_unit >= 1`:
`base_weight` is 1 and successful dispatches build the accounting payload before submission
(`crates/khive-runtime/src/cost_unit.rs:40-42`, `crates/khive-runtime/src/cost_unit.rs:143-155`,
`crates/khive-runtime/src/pack/dispatch.rs:1143-1167`). Thus a read is not pure observability merely because it performs
no domain mutation. Returning immediately would permit a metered read without its accounting row even while the
normal commit path remains available.

The wait ends on commit or terminal failure, or at the admission bound under Amendment 1. `kg.get` is an allowlisted
pair (`crates/khive-runtime/src/pack/registry_access.rs:505`); its produced success row uses bounded submission
(`crates/khive-runtime/src/pack/audit.rs:584-609`). Queue refusal and deadline expiry remain the two named exceptions;
they do not remove the normal wait. Amendment 10's proposed omissions remain separately scoped and do not include
`kg.get` (`crates/khive-runtime/src/pack/audit_success_rows.rs:55-81`). Failed dispatches are not assigned a successful
read's cost: their resource payload omits `cost_unit` (`crates/khive-runtime/src/cost_unit.rs:137-142`).

Removing the wait would need audit rows to commit off the shared writer, for example in a separate audit store. No
accepted record specifies one. ADR-134, which is not accepted, describes a distinct connection whose writer still
shares SQLite's write lock, which would not remove the contention (`docs/adr/ADR-134-store-durability-posture.md:118-132`).
The base text's references to ADR-134's D2a lane and durability posture point to that record; no rule in this
amendment depends on them.
This amendment selects neither a new store topology nor new durability settings. It answers #3730 by retaining the
bounded wait. The issue closes when this amendment merges.

### Decision 3 -- GateUnavailable is pure observability

`AuditProducer::GateUnavailable` is explicitly `PureObservability`. The dispatch has already failed closed; the row
records a gate outage, not a completed effect or authorization grant. A failed audit append must not turn that
refusal into success, or replace the gate error with a new domain outcome. `GateDenied` remains
`DispatchObligation`. The two producers are distinct in the exhaustive classifier
(`crates/khive-runtime/src/audit_batch.rs:49-54`, `crates/khive-runtime/src/audit_batch.rs:74-86`).

The outage path constructs `RuntimeError::GateUnavailable`, submits the observability row, then returns that error
(`crates/khive-runtime/src/pack/dispatch.rs:603-633`). This selects best-effort outage recording over an additional
obligation on an operation already refused, without weakening the gate's fail-closed result.

### Decision 4 -- Correct factual references and keep acceptance evidence explicit

The admission-degrade allowlist contains 70 `(owning pack, verb)` pairs at `7d6ff8fd`
(`crates/khive-runtime/src/pack/registry_access.rs:456-553`). Present-tense counts of 39 or eleven are corrected in
place; Amendment 2's explicitly dated 61-entry observation at `fe272269` remains historical. The census assertion is
`admission_degrade_safe_assertive_census_matches_live_pack_sources` at `crates/khive-runtime/src/pack_tests.rs:382`,
with equality and ownership assertions at `crates/khive-runtime/src/pack_tests.rs:526-565`.

Amendment 3's status is corrected to Accepted, consistent with the existing acceptance note
(`docs/adr/ADR-133-incidental-writes-off-the-request-hot-path.md:955-957`) and the implemented result-preserving fold
(`crates/khive-runtime/src/pack/audit.rs:725-729`). Amendment 10 remains Proposed. Its proposed policy is described as
such even though omission and sampling code is present. Source locations for event-store setup, the audit fold,
recall dispatch, serve-ledger batching, error projection, the census and ADR-103's audit payload are corrected in place.
These corrections distinguish source facts and specification status without accepting a proposal by implication.

**Base acceptance 1: unexecuted.** No two-revision census comparison is established by this amendment. The committed
manifest pins one source revision and a 12-pack inventory with 91 entries
(`scripts/data/writer-census-v1.json:3-133`). The runtime fixture has 14 packs, two entries and `final_revision: null`
(`crates/khive-runtime/tests/data/adr133-writer-census.json:3-37`). Those files do not constitute matched manifests at
two revisions. The historical aggregate of 100 verbs is not a substitute for that comparison. Acceptance 1 remains
required and is not waived.

### Acceptance

The statements below are verification requirements. Source inspection does not establish a passing execution result.
Test names identify existing coverage or explicitly identify coverage to add.

| ID     | Statement                                                                                                                                                             | Evidence                                                                                                                                                                                                                                                                                                 |
| ------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A11-1  | Every producer and terminal enum variant has the D1 table outcome; no default classification arm exists.                                                              | `crates/khive-runtime/src/audit_batch.rs:74-219`; compare all variants with D1's table.                                                                                                                                                                                                                  |
| A11-2  | A pending pure-observability append waits until commit, earlier failure or the admission bound; deadline release preserves success without requiring a failed commit. | Add a blocked-append dispatch case to `crates/khive-runtime/tests/adr133_audit_batch.rs`; exercise `crates/khive-runtime/src/pack/audit.rs:681-688` through a producer call site. `crates/khive-runtime/tests/adr133_audit_batch.rs:888-901` covers a panic, not this case.                              |
| A11-3  | A successful strict dispatch stays pending past the admission threshold, succeeds after commit and executes its handler once.                                         | `write_verb_waits_past_audit_deadline_until_row_commits`, `crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:592-648`.                                                                                                                                                                        |
| A11-4  | Resolution expiry returns an obligation error and leaves the row owned by the driver without rerunning the handler.                                                   | `write_verb_gives_up_after_resolution_deadline_when_store_never_returns`, `crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:664-725`.                                                                                                                                                        |
| A11-5  | Queue refusal and admission expiry preserve an eligible read result and increment distinct counters.                                                                  | `read_verb_dispatch_survives_audit_lane_admission_exhaustion_disposition` at `crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:1223`; `read_verb_dispatch_survives_audit_lane_admission_deadline_expiry_disposition` at `crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:1721`. |
| A11-6  | No additional successful obligation release is introduced for `RetryExhausted`.                                                                                       | Unmet by source: `crates/khive-runtime/src/pack/audit.rs:661-671` and `read_survives_lease_retry_exhaustion_but_write_stays_strict` at `crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:162`.                                                                                               |
| A11-7  | Driver abandonment frees admission capacity; shedding at the cap makes no store call and increments flush failures.                                                   | `driver_deadline_recovers_admission_capacity_from_a_stalled_generation` at `crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:820`; `driver_sheds_generations_once_the_abandoned_append_cap_is_reached` at `crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:1008`.               |
| A11-8  | Gate unavailability remains a typed failed dispatch while its audit producer is pure observability.                                                                   | `git_digest_gate_unavailable_precedes_the_receipt_contract`, `crates/khive-runtime/src/pack_tests.rs:4612-4646`; classifier at `crates/khive-runtime/src/audit_batch.rs:82-84`. Add outage-row append-failure and deadline cases.                                                                        |
| A11-9  | A shared identity is accepted only for an identical record, and an ambiguous acknowledgement retry accounts for exactly one row.                                      | `crates/khive-db/src/stores/event.rs:526-533`; `d1c_ambiguous_ack_retry_persists_exactly_one_accounting_row`, `crates/khive-runtime/tests/adr133_audit_batch.rs:1003-1042`.                                                                                                                              |
| A11-10 | The 70-pair allowlist resolves to the named owners and covers the classified Assertive surface.                                                                       | `admission_degrade_safe_assertive_census_matches_live_pack_sources`, `crates/khive-runtime/src/pack_tests.rs:382-567`; count entries at `crates/khive-runtime/src/pack/registry_access.rs:456-553`.                                                                                                      |
| A11-11 | Base acceptance 1 compares source-pinned inventories for the same pack population at two revisions.                                                                   | Unexecuted. `scripts/data/writer-census-v1.json:3-133` and `crates/khive-runtime/tests/data/adr133-writer-census.json:3-37` expose the unmatched populations and missing final revision.                                                                                                                 |
| A11-12 | The amended text retains the 5 s read wait and Proposed status of Amendment 10, and both invariants name the same obligation departures.                              | D1's release table and Amendment 11, Decisions 1, 2 and 4 in this file; `crates/khive-runtime/src/audit_batch.rs:307`.                                                                                                                                                                                   |
