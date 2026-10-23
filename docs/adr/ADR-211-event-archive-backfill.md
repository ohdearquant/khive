# ADR-211: Event Archive Backfill

- **Status:** Proposed 2026-10-23
- **Date:** 2026-10-23
- **Grounding commit:** `05dd44348e1d05667757430be922f3ca21b6926a`
- **Depends on:** [ADR-170](ADR-170-events-daemon-split.md), ADR-168
- **Relates to:** [ADR-168](ADR-168-event-retention-classes.md)
- **Addresses:** [#5589](https://github.com/ohdearquant/khive/issues/5589).

This draft requires specification approval before dependent implementation lands. It does not change the status of
ADR-168 or any other record.

## Context

ADR-168 names one live event store: the `events` and `event_observations` tables (ADR-168 §8). Since
[ADR-170](ADR-170-events-daemon-split.md), the event plane has two halves. The idempotent
audit-batch lane (dispatch audits and the config-lock records that share its flusher) writes to a
sidecar database beside the main file, and every other append class stays in the main file's
`events` table. The events daemon is the sidecar's resident writer; ADR-170 also keeps a
daemonless embedded mode in which the process appends to the sidecar directly. Rows written before
the split stayed where they were. Each half has its own `events` and `event_observations` tables,
and an observation row always sits in the same half as the event it names.

ADR-168 §9 makes every row written before a class's assignment ineligible until a versioned backfill
procedure exists, and lists the properties that procedure must settle. At the grounding commit, no retention-class field
is present in the storage Event
(`crates/khive-storage/src/event.rs:19`), so that rule covers the historical population. Most of those events are
generic
dispatch audits.

This record does two things. It extends archival to both halves of the split. It defines
backfill version 1, which reaches historical rows of exactly one class, Table A row 1 (generic
dispatch audit, `age_archivable`, no referent), and settles ADR-168 §9's properties for that class. ADR-168 §9 requires
a dedicated follow-up ADR. This record supplies that separately numbered contract;
it does not waive the placement rule.
Every other class keeps ADR-168 §9's default: historical rows are retained in full.

## Decision

### 1. Archival covers both halves of the split

- **Population.** ADR-168 §8's population is the `events` and `event_observations` tables of both halves.
  An event's observation rows are the rows in the same half's `event_observations` that name its
  id; they are copied, sealed, verified and pruned with their event, as ADR-168 §3 already requires. The
  copy carries the persisted observation rows, never a projection rebuilt from the event.
- **A segment holds rows of one half.** Each segment is copied from exactly one half, and its
  manifest records which half. ADR-168 §5's `ArchiveSegmentSealed` and `ArchiveRowsPruned` fields then
  describe one source as written, and ADR-168 §3's verification compares one half's rows with one
  segment.
- **Each half is archived by its own writer.** Main-half segments are copied and pruned through
  the main database's writer. Sidecar-half segments are copied and pruned by the events daemon,
  through copy, reserve and prune requests this record adds to its protocol; the event store
  trait and the daemon protocol carry no observation read at the grounding commit
  (`crates/khive-storage/src/event.rs:323`, `crates/khive-runtime/src/events_split/protocol.rs:6`). This adds no second
  resident or
  archival writer to the sidecar. A prune of either half writes reservations into the other half
  first (below), so this version archives neither half in daemonless embedded mode.
- **Pruning follows the segment.** Rows are deleted from the half the manifest names, and the
  deletion set is derived from the verified segment (ADR-168 §3), never by re-evaluating the eligibility
  predicate. A prune may refuse a segment (below); it never deletes a row the segment does not
  hold.
- **Every archived id is recorded in both halves.** Each half has its own archived-stub table (ADR-168 §7:
  event id and segment id). A pruned row's stub is written in the transaction that prunes it, in
  its own half, so the prune and the stub commit or fail together. The other half holds a
  reservation for the same id, written before the prune (below). Once a prune completes, both stub
  tables name every id it removed.
- **By-id reads.** `get` checks the main half's live rows, then the sidecar's live rows, then the
  stub tables of both halves, and answers a stubbed id with ADR-168 §7's typed archived answer. That
  holds for the trait's `get_event` and for the caller-facing `get` path that reads the sidecar
  directly; neither may present a stub as an ordinary event or feed it to observation decoding.

#### Archived ids are reserved

A stub records that an id was archived; with two identifiers it cannot show whether a later write
of the same id carries the same content. An id with a stub is therefore never inserted again, in
either half:

- **The check is part of the insert.** Every statement that inserts into a half's `events` table
  carries, in the same statement, the condition that no row of that half's stub table names the
  id. The condition travels with the statement, so it binds every writer of that file: the main
  writer, the events daemon, an embedded process appending to the sidecar directly, a caller-owned
  writer transaction, and the lineage-warning insert. The implementation carries a test that
  enumerates the production statements inserting into `events` and fails unless each carries the
  condition.
- **Reservation before prune.** Before a segment's rows leave their half, the other half's writer
  writes each selected id into its own stub table as a reservation naming the segment, in one
  transaction that first checks that the id has no live row in that half and that the segment is
  not retired in that half (Releasing a segment, below). A live row there means the id is live in
  both halves; the transaction refuses the whole segment and writes nothing, and neither copy is
  pruned. A retired segment is refused the same way. Only after the reservations commit does the
  prune transaction write each
  row's stub in its own half and delete the row.
- **Why that closes both directions.** An append into half B of an id being pruned from half A
  either commits before B's reservation, in which case the reservation finds the live row and
  refuses the segment, or after it, in which case the insert's own condition refuses it. An append
  into half A of an id still live in A is already decided by A's primary key. No ordering leaves the id
  inserted after its stub exists.
- **Outcomes.** An idempotent insert stopped by the condition returns a new per-item outcome,
  `Archived`: the id names an archived event, nothing is inserted, and no equality is claimed. The
  routed idempotent append, the leaf idempotent batch and the wire response carry it; the flusher
  treats it as settled, neither inserted nor retried; a peer that does not know the outcome is
  refused at version negotiation, before any write. An ordinary append stopped by the condition
  fails with a typed error naming the archive; in a batch this is a failure of the batch under its
  existing error rules, never a silently skipped row. A forwarded fire-and-forget append has
  already been acknowledged to its caller when persistence refuses it, so the receiver logs the
  refusal as a dropped row naming the id; that caller sees no error, as for any other drop on that
  path today.
- **Routing.** The split store's probe of the main half before a sidecar insert reads main's live
  row and main's stub in one statement and routes a stubbed id to the `Archived` outcome.
  Correctness rests on the insert condition, not on this probe.
- **A reservation whose prune did not run.** If a run stops between reserving and pruning, the id
  stays live in its half and reserved in the other. By-id `get` returns the live event, because
  live rows are read first. What else holds depends on the half and the entry point. An insert
  that reaches the reserving half is stopped by the reservation, and an idempotent one answers
  `Archived`; an idempotent retry that reaches the live half compares against the live row as it
  does today, with its existing equality or conflict outcome, and an ordinary append that reaches
  it fails on the event-id primary key with its existing duplicate-insert error (a batch fails
  under its existing rules). New edges are refused once
  main's stub table names the id: at once for a sidecar segment, whose reservation is in main, and
  only from the prune for a main segment, whose reservation is in the sidecar; an edge written in
  between is seen by the prune's re-check (Edges, below), which then refuses the segment. The
  run's recovery (§2, Audit evidence) completes the prune, or rollback releases the segment
  (below).
- **Releasing a segment.** A reservation is withdrawn only after its segment is released in the
  segment's own half, the half the prune would delete from. The release is a write transaction in
  that half that records the segment as released, and it commits only if no row of that half has
  been pruned against the segment; if one has, the release refuses and the reservations stay
  permanently. Every prune transaction checks, inside its own write transaction and before it
  writes a stub or deletes a row, that its segment is not released, and refuses the segment if it
  is. Both are write transactions on the same file, so that file's writer orders them: either the
  prune committed first and the release refuses, or the release committed first and the prune
  refuses. A prune request that outlives its caller is ordered the same way, so withdrawal does not
  depend on any lock or on the caller still running. Only after the release commits are the
  reservations removed from the other half, through that half's writer, in one transaction that
  also records the segment as retired in that half. Because every reservation transaction checks
  that record inside its own write transaction, a reservation request that reaches that writer
  after the removal, such as one sent by an invocation that has since died, refuses instead of
  recreating a reservation for the withdrawn segment. A reservation is never removed while its
  event is absent from the live store. The release and retirement records are permanent; their
  identity and lifetime are stated under Retention below.

Event ids are minted when an event is constructed, so these outcomes arise only when an old id
is written again.

#### Edges

- **Selection.** Backfill version 1 selects no event that is the source or target of any row in
  `graph_edges`, live or soft-deleted.
- **Admission is checked in the statement.** Edges are stored in the main file, and main's stub
  table holds both the stubs of pruned main rows and the reservations for sidecar rows. Every
  statement that writes a `graph_edges` row, whether an insert, an upsert, a resurrection or an
  update that changes an endpoint, carries in the same statement the condition that main's stub
  table names neither endpoint. That holds for guarded and unguarded writes alike, including the
  coordinator's same-backend link, which validates its endpoints and then writes without the
  endpoint guard. A write stopped by the condition fails with a typed error naming the archived
  endpoint. A census test enumerates the production statements that write `graph_edges` rows and
  fails unless each carries the condition.
- **Re-check where the selection is made final.** Seal and prune are separate actions (§2,
  Rollback), and an edge can be created to a still-live event between them. The transaction that
  writes main's stub table for a segment re-checks that no selected id is an edge endpoint, live
  or soft-deleted: for a main-half segment that is the prune transaction, and for a sidecar-half
  segment it is the main-half reservation transaction. If one is, the transaction refuses the
  whole segment. Main's single writer orders every edge write against that transaction, so an
  edge written before it is seen by the re-check and an edge written after it is refused by the
  condition above. Sidecar events remain valid `annotates` targets (ADR-170, Graph
  addressability); no part of this rests on which half an endpoint check reads.
- **Edges stored in another backend.** A cross-backend edge is stored in its source's backend. An
  edge held in another backend that names this backend's event is not visible to these checks, and
  after the prune its target resolves to ADR-168 §7's archived answer. The converse holds too: the source
  backend's write cannot see the target's stub. The coordinator can validate a live target, the
  target can then be pruned, and the edge can commit afterwards in the source backend. Version 1
  permits that result: the edge names an event that resolves to ADR-168 §7's archived answer. It does not
  coordinate archival across backends.
- **Raw-SQL readers of the main `events` table.** ADR-170 names three: the schedule
  creator-provenance fence, the kg projection worker's guarded insert, and the graph compiler's
  cross-substrate union. Backfill version 1 selects none of the rows the first two read (schedule
  creator provenance is Table A row 2, retained), and graph-query reachability of event rows is
  not an ADR-002 promise (ADR-170, Graph addressability).

### 2. Backfill version 1

Backfill version 1 is a classifier and a run procedure. It reaches only historical rows that are
generic dispatch audits.

#### Classifier identity

The classifier is named `backfill-v1`. A row is selected only when all of these hold:

1. Its `kind` is `audit`.
2. Its payload carries the gate's audit record: a JSON object whose `gate_impl` is a string,
   whose `decision` is one of `allow`, `deny` or `gate_unavailable`, and whose `verb` equals the
   row's `verb`. The generic dispatch audit writes this record, including its link-specific
   variants, which flatten it into their payload. So does one other emitter, the `git.digest`
   receipt, which condition 3 excludes by verb.
3. Its `verb` is not in this list, fixed for this version:
   - the verbs of Table A rows 2 to 7, both emitters of the collapsed rows 4 and 5 included:
     `schedule.creator_provenance`, `schedule.remind.fire`, `git.commit`, `git.branch`,
     `git.update_ref`, `git.push`, `git.pr_open`, `git.pr_review`, `git.pr_merge`, `git.digest`,
     `moodboard.serve_record` and `moodboard.model_record`. ADR-168 Amendment 3 corrects row 4's verb list to include
     the three pull-request verbs; the
     supplementary write audit covers them too;
   - every verb that another audit emitter writes beside the generic dispatch audit: `create`,
     `update`, `code.ingest` and `code.findings_ingest` (the content manifest exemption audits of
     [ADR-115](ADR-115-secret-gate-content-manifest-exemption.md)), `comm.suppression_remove` and
     `comm.withheld_body` ([ADR-200](ADR-200-external-email-policy.md)), `delete` (the
     hard-delete lineage warning), and `mount.repin` (the mount repin audit);
   - the six polling keys reserved to ADR-212: `comm.inbox`, `comm.cleanup_expired_quarantine`,
     `whoami`, `comm.probe`, `comm.cursor_get` and `comm.heartbeat`. This exclusion applies to
     every Audit row of those exact verbs, irrespective of stamp, historical assignment, policy
     activation or reservation state. These keys are never selected by `backfill-v1`.
4. Its id is not the source or target of any `graph_edges` row, live or soft-deleted.
5. Its `created_at` is older than the configured horizon (ADR-168 §4). Unset means the backfill does not
   run.

**Shared keys are excluded whole.** ADR-168 §1 keys retention on `(kind, verb)`, gives a key with several
emitters the safest of their classes, and allows a per-emitter split only through a
producer-stable discriminator recorded in the payload, never one inferred from payload shape.
Backfill version 1 splits no key. Condition 2 only narrows the selection; every key the generic
dispatch audit shares with another emitter is excluded in full by condition 3, so generic
dispatch audits of those verbs stay live too. That costs coverage (`create`, `update` and
`delete` are frequent dispatch verbs) and keeps every other emitter's rows exactly where ADR-168 §1's
rules leave them. Splitting those keys needs the discriminator ADR-168 §1 describes and a later version.

**The emitter census is a test.** The implementation carries a test that enumerates every
production construction of an `audit` event and fails unless each one is the generic dispatch
audit or writes only verbs that condition 3 excludes. A new audit emitter fails that test until
it is classified, and a new shared verb joins the list in a new classifier version.

Every segment written by a backfill run records `backfill-v1`, the excluded-verb list and its
digest, condition 2's predicate, and the `run_id` and `attempt_id` of the seal invocation that
wrote it (Audit evidence, below). A later change to Table A does not change what
`backfill-v1` selected; a new list or predicate is a new version.

`excluded_verbs_digest` is BLAKE3 over the canonical JSON bytes of the excluded-verb list. The list is a JSON array of
verb strings, deduplicated and sorted by byte order before serialization. Serialize it with the same recursively
key-sorted, whitespace-free `canonical_json_bytes` rule as ADR-168 Amendment 3, Decision 4 and ADR-142's catalog
digest (`crates/khive-types/src/canonical_json.rs:8`, `docs/adr/ADR-142-agentic-process-runtime.md:271`). Hash the array
bytes and encode the result as the lowercase hex string used by `ContentRef`
(`crates/khive-storage/src/blob.rs:103`). This makes the excluded set's identity independent of duplicate entries or
input ordering while retaining the segment digest's serialization and representation rules.

#### Unresolved references

Row 1 declares no referent, so `backfill-v1` resolves none. It fails closed on classification:
a row that does not meet every condition above is retained, including a row whose payload is not
an object, whose `gate_impl` or `decision` is missing or of another type, or whose `verb` is
missing or empty. A generic dispatch audit carries a `target_id` when its arguments named a
record; that column is evidence about the call, not a referent, and does not change its class.

#### Correlation units

Row 1 declares no correlation key, so each selected row is its own unit, and the selection is
deterministic: the same rows and the same horizon yield the same set. `backfill-v1` selects no
row of a correlation-keyed class.

#### Stub creation

A backfilled row's id is reserved in the other half before its prune, and its stub is written in
the transaction that prunes it, in the row's own half, as §1 states. By-id `get` of a pruned
historical id returns ADR-168 §7's archived answer.

#### Audit evidence

A backfill is an operator action with its own canonical verb, `archive.backfill`, which the Gate
must allow explicitly by verb id, as ADR-168 §6 requires of the segment actions; no wildcard grant covers
it. It runs in two invocations: a seal invocation that selects, copies, seals and verifies, and a
prune invocation that names a sealed run. Each invocation is recorded by one new additive event
kind, `ArchiveBackfillRun`, in the shape of ADR-168 §5's and ADR-168 §6's attempt records:

- **One invocation at a time.** A backfill invocation holds an exclusive lock on
  `<canonical main database path>.khive-event-archive.lock`, taken without waiting and held until
  its completion row is appended. The lock is distinct from every other coordination lock. An
  invocation that finds the lock held writes its attempt row and a completion with outcome
  `refused` and failure `busy`, and does nothing else; an error opening or locking the file is a
  `failed` completion, not contention.
- **Rows.** Each invocation has its own `attempt_id`, written on its attempt row and on its
  completion row and on no other row. The attempt row is appended before the invocation has any
  effect, and the completion row when it ends, whatever the outcome. A denied invocation writes
  both, with outcome `denied`. If the attempt row cannot be appended, the invocation does nothing
  and reports the failure to its caller and to the host's error log, as ADR-168 §6 requires. A completion
  is an outcome record written after the effect; it cannot undo an effect already performed.
- **A missing completion is resolved before any other effect.** An allowed attempt with no completion
  under its `attempt_id` reads as admitted with its outcome unrecorded, never as success (ADR-168 §6).
  After appending its own attempt row and before any other step, a seal or prune invocation of a
  run resolves every such attempt of that run from durable state and appends a completion under the
  unresolved `attempt_id` with `recovered` true:
  - A seal attempt resolves to `interrupted`. The segments and `ArchiveSegmentSealed` records an
    interrupted seal left behind show which segments it finished, not whether the seal as a whole
    finished: a seal that completed and one that failed before its next segment had a manifest
    leave the same records. So a seal is `sealed` only by its own completion row, `interrupted`
    never authorizes a prune, and its segments are released and deleted by rollback or left in
    place. Sealing again is a new run.
  - A prune attempt resolves per segment, and the states are disjoint. Pruned: the segment's
    `ArchiveRowsPruned` record exists and every one of its rows is absent from its half with a stub
    there. Not pruned: every one of its rows is still live, whether or not the record exists. The
    record is a precondition appended before the delete executes, so a record beside live rows is
    the expected state of a crash before the prune transaction committed, not a contradiction. The
    completion records the segments found pruned, and the invocation resumes the rest after
    re-verifying them as a fresh prune would.
  - Evidence that cannot be read, or that fits neither state (some rows of a segment absent and
    some live, a row absent without its stub, or rows absent with no `ArchiveRowsPruned` record),
    resolves to `failed` with failure `unresolved_evidence`. The invocation then stops without
    changing any live event or observation row, any reservation, or any release or retirement
    record. Its only writes are audit rows: its own attempt row, appended before recovery began; the
    recovered completion under the old `attempt_id`; and its own completion under its own
    `attempt_id`, with outcome `refused` and failure `unresolved_evidence`. The old attempt's outcome
    is reported under the old id and this invocation's under its own.
  - A run is held while it has a completion with outcome `failed` and failure `unresolved_evidence`
    that no resolution names. The hold is read from the run's audit history, never from its latest
    row: every later seal or prune invocation of the run looks for such a failure and, finding one
    unresolved, refuses (outcome `refused`, failure `unresolved_evidence`). A later refusal, denial,
    busy result, lock failure, recovery completion, success of either phase, or deletion of a
    segment does not resolve it. Only a resolve invocation does: an operator action, phase
    `resolve`, under the same lock and with its own attempt and completion rows, whose completion
    has outcome `resolved` and names in `resolves` the `attempt_id` of the failed completion it
    clears. It refuses when the named attempt is not an unresolved `unresolved_evidence` failure of
    that run, and it changes no live row, reservation, segment, release or retirement record; the
    operator repairs or accepts the evidence before running it. Admission of seal and prune and the
    retention rule below use this same predicate. Rollback stays available to the operator, because
    its release refuses any segment with a pruned row.

  If a recovered completion cannot be appended, the invocation stops and reports the failure to its
  caller. A completion stands for its own attempt only.
- **A completion that cannot be appended.** When an invocation's own completion row cannot be
  appended, it reports the failure to its caller and to the host's error log, releases the lock,
  and leaves its attempt unresolved; the next invocation of the run resolves it first, as above.
- **Typed payload, schema version 1:**

  | Field                   | Type               | Nullable | Semantics                                                                                        |
  | ----------------------- | ------------------ | -------- | ------------------------------------------------------------------------------------------------ |
  | `run_id`                | `Uuid`             | no       | Identity of the backfill run; the seal and prune invocations share it.                           |
  | `attempt_id`            | `Uuid`             | no       | Unique per invocation; identical on its attempt and its completion row.                          |
  | `phase`                 | `String`           | no       | `seal`, `prune` or `resolve`.                                                                    |
  | `stage`                 | `String`           | no       | `attempt` or `completion`.                                                                       |
  | `outcome`               | `String`           | yes      | Completion only: `sealed`, `pruned`, `interrupted`, `resolved`, `refused`, `denied` or `failed`. |
  | `failure`               | `String`           | yes      | Completion only, for `refused` and `failed`: the step that ended it.                             |
  | `recovered`             | `bool`             | yes      | Completion only: true when appended by a later invocation's recovery.                            |
  | `resolves`              | `Uuid`             | yes      | Resolve completion only: the `attempt_id` of the `unresolved_evidence` failure it clears.        |
  | `classifier`            | `String`           | no       | `backfill-v1`.                                                                                   |
  | `excluded_verbs_digest` | `String`           | no       | Digest of condition 3's list as the run applied it.                                              |
  | `horizon`               | `DateTime<Utc>`    | no       | The horizon the run applied, as a `created_at` bound.                                            |
  | `segment_ids`           | `Vec<Uuid>`        | yes      | Completion only: segments this invocation sealed or pruned.                                      |
  | `selected_by_half`      | `Map<String, i64>` | yes      | Seal completion only: selected rows keyed `main` and `sidecar`.                                  |
  | `selected_by_verb`      | `Map<String, i64>` | yes      | Seal completion only: selected rows per `verb`.                                                  |

- **Dimensions.** Attribution: the dispatched operator principal. Additivity: additive under
  ADR-162 §3. Sink: `caller_event_store`, in the main half. Write posture: attempt rows are
  preconditions, appended before any effect; completion rows are outcome records, appended after
  it. A prune invocation runs only when its run's seal attempt has its own durable completion
  with outcome `sealed` and `recovered` false, and its own attempt row precedes any reservation or
  delete. A recovered seal completion is never `sealed`: it is `interrupted`, or `failed` with
  `unresolved_evidence`, and neither authorizes a prune.
- **Retention.** Class `pinned_while_referenced`. The referent of every row of a run, attempt or
  completion, is the run's segment set: the segments whose manifest names the `run_id`. The
  manifests are the authority; a completion's `segment_ids` is a copy for readers. The terminal
  condition is met when that set is known, is not empty, and every segment in it has been
  destroyed (ADR-168 §6). A set that cannot be read, a run with an unresolved attempt, or a held run (an
  `unresolved_evidence` failure that no resolution names, as above) has no terminal condition, and
  neither does a run that sealed nothing, a denied run included; those rows are retained
  indefinitely. Such runs are rare operator actions, so the cost is small.
- **Release and retirement records.** These records belong to this record's storage, beside the
  stubs. Each half holds at most one release record and one retirement record per segment, keyed by
  the segment id; writing either again for the same segment changes nothing. They are permanent:
  rollback, destruction of the segment, and the age eligibility of the run's audit rows do not
  remove them, and no retention rule of this record or of ADR-168 §6 applies to them, because a delayed
  prune or reservation request for the segment can reach its writer at any later time and only
  these records make it refuse. A future reclamation would first have to prove that no request for
  the segment can still execute; a timeout, a lost reply, a closed connection, the invocation lock,
  or the segment's destruction is not that proof. Each record holds a segment id and a timestamp,
  so the cost is constant per released segment and its count grows with seal-and-rollback cycles,
  not with archived rows; no total size bound is promised for it.
- **A prune verifies before it deletes.** It refuses unless the run's seal completion is `sealed`,
  every segment in the run's set still has its `ArchiveSegmentSealed` record and has been neither
  deleted nor replaced, and each segment re-verifies under ADR-168 §3. Each segment's prune then emits
  ADR-168 §5's `ArchiveRowsPruned` as on the live path. A segment from a seal that ended `failed` is never
  pruned by that run; it is removed through ADR-168 §6 or left in place.

**A backfill run is not authorized until ADR-168 §5's three kinds and `ArchiveBackfillRun` exist in the
event plane** with the full additive contract ADR-168 §5 states.

#### Verification

Backfilled segments follow ADR-168 §3 without exception: copy, seal, re-read and compare, and prune only
rows proven present in the verified segment.

#### Rollback

ADR-168 §9 requires a way to undo a run that classified rows incorrectly before those rows are pruned.
Backfill version 1 makes that window explicit:

- **Seal and prune are separate invocations.** The seal invocation stops after its completion
  row. The prune invocation is a second action, taken after the operator has read the run's counts
  per half and per verb. The counts are a read for the operator, not the guard: the classifier
  conditions are the guard.
- **Rollback is releasing, then deleting, the run's segments before prune.** For each segment,
  rollback first releases it in its own half (§1, Releasing a segment). A segment whose release
  refuses has rows pruned against it; rollback stops for that segment, reports it, and leaves the
  segment and its reservations in place. After a release commits, rollback removes the segment's
  reservations from the other half, then removes the segment through ADR-168 §6's
  `archive.delete_segment`, the only action that ends a sealed segment, with its
  `ArchiveSegmentAccessAttempted` pair. `archive.delete_segment` refuses a backfill segment that is
  neither released nor fully pruned, so no prune can verify a segment and then find its media gone.
  Rollback holds the backfill lock (Audit evidence) for its whole run, which keeps it from
  overlapping a backfill invocation in the ordinary case; correctness rests on the release, not on
  the lock. No selected event or observation has been pruned in a released segment, so nothing
  else is needed; rollback is complete when every intended deletion has succeeded.
- **After pruning there is no rollback into the live store.** ADR-168 §6's restore copies rows to an
  operator-served location and defines no path back into the live tables. Backfill version 1 does
  not add one, which is why the prune step waits for the operator's read.

### Consequences

- Historical generic dispatch audits become reachable by archival once a horizon is configured
  and the event kinds above exist, except those of the verbs condition 3 lists. Every other
  historical row stays retained until a later backfill version names its class and settles ADR-168 §9 for
  it.
- An event an edge in its own backend names is never pruned by this version, including one whose
  edge was created after its segment was sealed. An edge stored in another backend is not seen.
- Every statement that inserts into either half's `events` table, and every statement that writes
  a `graph_edges` row, gains a stub condition. Idempotent appends gain the `Archived` outcome, and
  the events daemon gains copy, reserve and prune requests for its half.
- A new edge written in the same backend as an archived event fails with a typed error naming the
  archive. An edge written from another backend is not checked against the target's archive and
  can commit after the target is pruned; its target then resolves to ADR-168 §7's archived answer.
- A run interrupted between reserving and pruning leaves live events whose behaviour depends on
  the half (§1, A reservation whose prune did not run) until its recovery or rollback. A seal
  whose completion was lost is not recovered as `sealed`; it is sealed again by a new run.
- Withdrawing a reservation requires releasing its segment first, so a rollback can never race a
  prune that is still in flight.

### Out of scope

- Backfill of any class other than Table A row 1, and any per-emitter split of a shared key.
- Default horizon values. ADR-168 §4 still leaves them to operators.
- Changing which append classes the sidecar receives.
- Archival in daemonless embedded mode.
- Coordinating archival with edges stored in other backends.

## Alternatives considered

**Retain all historical rows indefinitely.** This remains the inactive state. It preserves every row but does not
reduce historical live-store growth. A versioned, explicitly invoked classifier reaches the eligible subset while
keeping other classes grandfathered under ADR-168 §9.

**Delete by age without a sealed copy or identity reservations.** Rejected. It violates ADR-168's verified-copy
requirement and permits retries to recreate pruned IDs. The live-only retry branch currently inserts on an ID miss
(`crates/khive-db/src/stores/event.rs:521`), which is why reservations are part of the write contract.

**Split shared Audit keys by inferred payload shape.** Rejected. ADR-168's key is `(kind, verb)` and requires a
producer-stable discriminator before an emitter split. Condition 2 narrows candidates; condition 3 excludes shared
keys whole.

## Acceptance

The named tests below are required implementation evidence, not assertions that those tests already exist or pass.

| ID  | Statement                                                                                                                           | Evidence                                                                                                                                                                         |
| --- | ----------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| B1  | Every selected row satisfies all five classifier conditions; the polling keys in condition 3 never qualify, stamped or unstamped.   | Required `backfill_v1_classifier` test; compare condition 3 with ADR-212 §1.                                                                                                     |
| B2  | Every production Audit constructor is generic dispatch or uses only excluded verbs.                                                 | Required `backfill_v1_emitter_census`; `crates/khive-runtime/src/pack/audit.rs` and the Table A sites in ADR-168.                                                                |
| B3  | Every event insert checks its local identity reservation in the insert statement.                                                   | Required `archive_event_insert_census`; include `crates/khive-db/src/stores/event.rs:362`, `crates/khive-db/src/stores/event.rs:563`, `crates/khive-db/src/stores/event.rs:672`. |
| B4  | Every same-backend edge write rejects archived endpoints; selection and final main-side reservation recheck existing edges.         | Required `archive_edge_write_census` and `archive_reference_races`; §1, Edges.                                                                                                   |
| B5  | Prune and its source stub commit together, after the other half's reservation; conflicts refuse the whole segment.                  | Required `archive_two_half_reservations`; `crates/khive-runtime/src/events_split/stores.rs`.                                                                                     |
| B6  | Segment bytes include persisted observations and reproduce ADR-168 Amendment 3's count and BLAKE3 digest.                           | Required `archive_segment_readback`; `crates/khive-db/src/stores/event.rs:465` and ADR-168 Amendment 3, Decision 4.                                                              |
| B7  | Live rows resolve first; a pruned historical ID returns Archived on both trait and public get paths.                                | Required `archive_split_get`; `crates/khive-runtime/src/events_split/stores.rs:275`, `crates/khive-pack-kg/src/handlers/get.rs:394`.                                             |
| B8  | A missing seal completion never authorizes prune; unresolved evidence holds the run until an explicit resolution names the failure. | Required `backfill_recovery_hold`; §2, Audit evidence.                                                                                                                           |
| B9  | Release fences delayed prune; retirement fences delayed reservation; neither removes a permanently pruned ID.                       | Required `backfill_release_retirement_races`; §1, Releasing a segment.                                                                                                           |
| B10 | Failed attempt append has no effect; completion loss is reported and resolved from durable state.                                   | Required `backfill_audit_failures`; §2, Audit evidence.                                                                                                                          |
| B11 | Archived idempotent retries settle without equality claims; unknown protocol peers refuse before writes.                            | Required `archive_wire_disposition`; `crates/khive-runtime/src/events_split/protocol.rs`, `crates/khive-storage/src/event.rs:303`.                                               |
| B12 | No backfill runs with an unset horizon, before the four additive event contracts exist, or in daemonless embedded mode.             | Required `backfill_activation_gates`; §1 and §2.                                                                                                                                 |
