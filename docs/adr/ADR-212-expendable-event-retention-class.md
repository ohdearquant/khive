# ADR-212: Expendable Event Retention Class

- **Status:** Accepted 2026-10-23
- **Date:** 2026-10-23
- **Grounding commit:** `05dd44348e1d05667757430be922f3ca21b6926a`
- **Depends on:** ADR-168, [ADR-211](ADR-211-event-archive-backfill.md),
  [ADR-170](ADR-170-events-daemon-split.md), [ADR-204](ADR-204-automatic-freelist-reclamation.md)
- **Relates to:** [ADR-168](ADR-168-event-retention-classes.md)
- **Addresses:** [#5589](https://github.com/ohdearquant/khive/issues/5589).

It does not change the status of ADR-168 or any other record.

## Context and scope

The idempotent audit-batch lane introduced by ADR-170 accumulates high-volume dispatch receipts for
recurrent mailbox, channel-maintenance and identity checks. A seven-day recent receipt window is
useful for debugging and accounting; retaining every such payload permanently is not required for
reconstruction of mailbox, channel or training state.

This record adds an `expendable` retention class for the six exact `(kind="audit", verb)` keys
below. It permits removal of eligible payloads without sealed copies. It does not permit sampling,
change the audit obligation at dispatch time, or remove any feedback, phase, channel-lifecycle,
schedule-provenance or moodboard record. It applies to both halves of the split event plane,
including historical generic dispatch audits in the main file.

ADR-103 Amendment 7 (Accepted) is the write-time half of the same problem and is a separate
decision. In its `reduced` mode four of the six verbs write no row for a successful dispatch, and
the other two write the row of a hash-selected one in ten successful calls plus every call whose
result reports a change or a failed poll. Every non-success outcome of all six keeps its row. That
amendment reduces growth and deletes nothing. This record governs the rows that remain under the
six keys: rows written before that amendment's reduced mode, rows written in its `all` mode, the
sampled and forced rows it keeps, and every non-success row. The two are independent: either may
land first, and neither changes the other's predicate.

The capacity metric is `size(events database file) + size(events WAL)`. It excludes the domain
database, shared-memory file, archives and temporary maintenance files, but maintenance must still
have sufficient physical disk space for all of them. The bound on that metric is operator
configuration (§6). Seven-day pruning alone does not hold the metric under any fixed value: other
classes keep growing, and so does the compact identity state this record adds. The bound is a
disk-pressure signal with a defined response, not a correctness line, and reaching it does not stop
the deployment.

**Relationship to ADR-168.** ADR-168 §2 requires a separate record with its own historical-row contract for a move
to a weaker class. This record supplies that contract independently of ADR-168 and ADR-211; §4 states the historical
cohort and its migration. For the six keys only, it moves them from
`age_archivable` (Table A row 1) to `expendable`, which is weaker because it keeps no sealed copy.
For the same six keys it also makes the following departures, and no others:

- it defines `expendable` as this record's extension; ADR-168 retains its closed three-value vocabulary;
- it is this record's separate exception to ADR-168 §3's sealed-copy rule and to the base's rejection of delete-only
  retention;
- it fixes the horizon at seven days for this policy instead of taking it from ADR-168 §4's configuration;
- it fixes the expendable pass's cadence (at least hourly), departing from the non-goal "no
  automatic scheduling policy" for this pass only;
- it makes one availability decision, the response to the configured capacity bound (§6), departing
  from the non-goal "no durability or availability decision" for that response only;
- it is the narrow exception to ADR-168 §9 for the historical cohort named in §4 below.

ADR-168 §3, §4, §9 and its non-goals stand unchanged for every other class, for sealed archival, and for the
remainder of Table A row 1.

## Decision

### 1. Closed class assignment

| Exact verb, with kind `audit`     | Reported rows on 2026-10-19 UTC | Reason for this assignment                                                                                                                                  |
| --------------------------------- | ------------------------------: | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `comm.inbox`                      |                          35,460 | Repeated mailbox listing/wait checks; message state is stored independently of the generic dispatch receipt.                                                |
| `comm.cleanup_expired_quarantine` |                          25,310 | Recurrent bounded channel cleanup. The receipt is expendable; the cleanup's domain mutations and any independent events are not.                            |
| `whoami`                          |                          25,029 | Repeated principal/build/namespace checks. This also covers interactive calls to the same verb; classification is by exact key, not inferred caller intent. |
| `comm.probe`                      |                          24,656 | Explicit read-only mailbox polling interface.                                                                                                               |
| `comm.cursor_get`                 |                          12,655 | Channel poll checkpoint reads. The persisted checkpoint is separate state.                                                                                  |
| `comm.heartbeat`                  |                          12,655 | Receipt of the per-poll health update; the health record remains domain state.                                                                              |

The total is 135,765 of 164,793 reported Audit rows, 82.3852%, measured before ADR-103 Amendment 7.
The reported counts are also recorded at `docs/adr/ADR-103-resource-attribution-model.md:1459`.
These operational measurements motivate the assignment but are not classifier inputs or guaranteed
byte savings.

No other key is included. In particular `knowledge.get`, `gtd.tasks`, `channel.poll_lifecycle`,
every phase kind and `feedback_explicit` are excluded. Case folding, alias expansion after storage,
prefixes, regular expressions, caller-supplied lists and configurable verb additions are prohibited.
The predicate compares the persisted canonical verb exactly and always also requires `kind="audit"`.

This record carves ADR-168 Table A row 1 into this closed subset and its unchanged `age_archivable` remainder.
The `expendable` extension belongs to this record, not ADR-168's base vocabulary. The most protective class still wins
for a shared
`(kind, verb)` key: pinned, aggregate-required, age-archivable, then expendable. This record does
not relax protection for any shared or provisional key. A new emitter of an allowlisted key must
fail the emitter-census acceptance test until independently reviewed; it may not inherit
expendability merely because its verb matches.

At construction, the generic dispatch producer records an additive payload object:

`retention = {"class":"expendable", "policy":"poll-dispatch-v1", "horizon_seconds":604800}`.

This object is immutable with the event and does not redefine existing payload fields or
schema-version meanings. The runtime computes it; user parameters cannot supply or override it.
Assignment happens before batch admission and the same stamped event is reused on every retry. It is
not synthesized at query time. A stamped row still must match the exact kind/key and
generic-producer contract before pruning; a stamp alone never authorizes deletion. Unrecognized
policy versions, malformed stamps, aggregate-bearing records, and conflicting classification retain
the row and report the reason.

This policy covers success, denied and error outcomes of the selected generic dispatch keys,
including the sampled and forced rows that ADR-103 Amendment 7 keeps. That choice expires old
failure diagnostics too; retaining a subset of outcomes would require another version and a revised
capacity model. A row's `resource.sampling` object is neither a classifier input nor a reason to
retain the row, and pruning a sampled row removes its sampling evidence with it.

### 2. Horizon, execution and preservation

The horizon is **fixed at 604,800 seconds (seven 24-hour days)** for this policy. It is not
inherited from ADR-168 §4's unset/off archival configuration. The operator explicitly activates the policy
at rollout; activation and its timestamp are durable. Once active, maintenance evaluates eligibility
at least hourly, with bounded batches. The fixed cutoff for a run is
`run_started_at_utc - 604800 seconds`; only `created_at < cutoff` qualifies. Equality is retained.
No shorter horizon or broader list is a supported tuning knob. Failure to execute on schedule raises
an alarm; it does not authorize early deletion.

Remove only the selected event and that event's persisted observation rows in its own half,
transactionally. A row with a live or soft-deleted graph edge naming it in the same backend is
retained. An unresolved classification or reference check retains the row. This is a protection
exception to age eligibility, so protected backlog is reported separately. Neither a future
timestamp nor missing metadata is interpreted as permission to delete.

The generic Audit decoder currently creates no observations, but that does not authorize an
assumption that historical persisted observations are empty. The selection record includes the
actual persisted observations. Unknown observation content or a classification inconsistency refuses
that row pending review. Every other event and observation remains byte-for-byte unchanged.

There is **no sealed copy of expended payloads**, no restore path for them, and no claim that the
whole plane remains information-preserving for this class. The reason is a product decision to limit
detailed polling receipts to the recent window. The readers of these rows are `brain.event_counts`,
grouped event counts, `brain.event_page`, event list/get, benchmark request-to-duration joins, audit
retry comparison, and event-ID references. `brain.events` was inspected and is not one of them: its
fixed verb filter names none of the six verbs. Each reader's post-expiration behavior is specified
below; absence of readers is not the justification.

### 3. Event identity, references and disclosed expiration

The payload is expendable; the fact that an event ID was used is not silently erased. Pruning
retains compact permanent **expended-ID records** with event ID, policy/classifier version,
namespace/verb needed for scoped disclosure, removal time and run identity. They are not ordinary
events, are not fed to observation decoding or counted as dispatches, and never claim that an
archive exists. Their physical cost and continuing growth are included in capacity accounting.

Use ADR-211's two-half reservation ordering for these IDs, adapted to an expended
disposition rather than an archive segment. Before deletion in half A, half B durably reserves each
selected ID and refuses if it has a live copy. The source-half transaction records the expended
disposition and removes exactly its verified selected rows together. Every event insertion path in
each half checks its local reservation record in the same statement/transaction; a separate
preflight lookup is insufficient. This includes embedded writes and caller-owned SQL transactions,
not just the events socket.

The main writer orders reference validation and reservations against every graph-edge
insert/upsert/resurrection/endpoint change, as in ADR-211. Selection and final
main-side reservation recheck both reject selected IDs used by a live or soft-deleted edge. A later
same-backend edge write cannot bypass the reservation. Edges held by another backend are not
discoverable by these checks; as in ADR-211, their eventual target may resolve to a
typed non-live result. Here that result is Expended and has no recoverable payload. This limitation
must be documented for reference holders.

Live rows resolve before reservation/expended records. A completed deletion returns a typed
**Expended** result to trait and public by-ID resolution, carrying the event ID, policy and removal
evidence; it is distinct from unknown ID and Archived. Query/count/list/page surfaces report
retention coverage when the requested kind/time scope can include expended polling receipts. A broad
query must not call its retained-row total a full historical dispatch count. Counts over retained
rows stay integer/exact; no extrapolation or zero-fill is added. Queries scoped exclusively to
non-expendable kinds gain no retention fields from this policy, preserving their existing serialized
response shape.

The idempotent batch gains an **Expended** per-item disposition: the ID is reserved and no row is
inserted, but column equality is not asserted. The split probe, both leaf stores, daemon wire and
audit flusher carry it. It is terminal, not retried and not reported as AlreadyPresentIdentical.
Ordinary appends to a reserved ID refuse with a typed error. Old peers that do not understand the
disposition are refused by protocol negotiation before writing. Delayed retries must never resurrect
pruned rows in either half.

A run interrupted after reservation but before deletion leaves the live row readable and the
reservation protective. Recovery may complete the guarded delete or release the reservation only
after source-half durable evidence proves no deletion committed and fences delayed prune requests.
Use ADR-211 release/retirement ordering, keyed by immutable prune-batch identity instead of
segment identity. A timeout, process death or lost reply is not proof of non-execution. No
permanently expended ID is released for reuse.

### 4. Historical population and one-time migration

The historical classifier is **`poll-dispatch-legacy-v1`**, distinct from `backfill-v1`. It reaches
only this record's six keys. A candidate must have kind Audit; exact allowlisted verb; generic
dispatch envelope with `gate_impl` string, `decision` in `allow`, `deny`, `gate_unavailable`, and
payload `verb` equal to row `verb`; no aggregate identity; and no contrary dedicated-emitter
evidence. A row carrying ADR-103 Amendment 7's `resource.sampling` object is a generic dispatch
envelope and is classified by the same predicate. The shared-emitter exclusions of ADR-211 §2 also apply. Its separate
polling-key exclusion governs
`backfill-v1` only, and does not exclude the historical cohort defined here. Ambiguous or malformed rows remain
grandfathered.

At an explicit, writer-quiesced rollout boundary, capture the pre-policy population in each half and
classify it once. Record immutable per-ID historical assignments outside the Event payload; do not
rewrite old events as if they carried a write-time stamp. Assignments include the classifier version
and rollout/run identity. Old eligible rows can be pruned immediately; correctly classified rows
younger than seven days acquire age eligibility later under the same assignment. Rows excluded from
that captured population are never retrospectively swept by a loose timestamp predicate. Late
unstamped writes from an old producer fail rollout admission rather than silently entering a new
historical cohort.

An operator first reviews counts by half, verb, age bucket and exclusion reason under a fixed
candidate-set digest. Mutation is a separate explicitly authorized phase using that same versioned
cohort, with final predicate/reference rechecks. Before deletion, aborting leaves event and
observation bytes intact and can discard unused assignment state through a recorded rollback. After
deletion there is no payload rollback; expended-ID and run records are permanent evidence. The lack
of a sealed copy is authorized here, not by reinterpreting `backfill-v1`.

The candidate-set digest is BLAKE3 over the canonical JSON bytes of a JSON array of event ids. Each id is a lowercase
hyphenated UUID string; deduplicate the strings and sort them by byte order before serialization. Use the same
recursively key-sorted, whitespace-free `canonical_json_bytes` rule as ADR-168 Amendment 3, Decision 4 and ADR-142's
catalog digest (`crates/khive-types/src/canonical_json.rs:8`, `docs/adr/ADR-142-agentic-process-runtime.md:271`). Hash
the array bytes and encode the digest as the lowercase hex string used by `ContentRef`
(`crates/khive-storage/src/blob.rs:103`). This binds the counts and mutation phase to a reproducible set identity,
independent of enumeration order or repeated ids, using the segment digest's serialization and representation rules.

This is the narrow exception to ADR-168 §9 for the named historical cohort. Every other pre-existing row
stays under ADR-168 §9. In particular, this does not make phase events eligible merely because Table A calls
them age-archivable.

### 5. Run evidence and composition with sealed archival

Each run has an immutable ID, caller or maintenance principal, policy/classifier revision and
literal allowlist digest, activation/cohort identity, fixed cutoff, selected counts by half/verb,
excluded counts/reasons, and candidate-set digest. A run's authorization and attempt record are
durable in the main event plane before any reservation or delete. If that append fails, the run has
no destructive effect.

Use a distinct typed Audit payload under exact maintenance verb `events.prune_poll_audits`, schema
version 1, outside the expendable list. Its dimensions:

- **Attribution.** The caller or maintenance principal that started the run.
- **Additivity.** Additive under ADR-162 §3: fields are added, never removed or redefined.
- **Sink.** `caller_event_store`, in the main half.
- **Write posture.** Attempt rows are preconditions, appended before any reservation or delete;
  completion rows are outcome records, appended after it.
- **Retention.** Class `pinned_while_referenced`. The referent of every row of a run, attempt or
  completion, is the run's expended set: the expended-ID records and source-half batch receipts that
  name the run's ID. That set is permanent, because no expended ID is released (§3), so the class
  has no terminal condition and these rows are retained indefinitely by the referent's own
  permanence. A run that expended nothing, a refused run and a run with unresolved evidence are
  retained the same way. The class covers every row written under the key
  `(Audit, events.prune_poll_audits)`, including a generic dispatch receipt for the same verb if one
  is produced, by the base's most-protective rule for a shared key. A run writes an attempt row and
  a completion row, so the required hourly cadence adds about 48 rows a day; they count toward the
  capacity metric like any other rows.

A completion records per-half committed batch IDs/counts, actual observed bytes/page/freelist/mode
before and after, verification result, failures and excluded backlog. It must not equate
payload-byte estimates with file bytes reclaimed.

Each source-half delete transaction also stores a durable batch receipt with selected-ID digest,
count, policy and cutoff, atomically with expended records and removal. This resolves a crash after
commit but before main completion. Missing completion means outcome unrecorded, never success.
Recovery reads those receipts and reservations, appends its recovery outcome, and refuses ambiguous
evidence. It does not re-run an unbounded age delete. A completion append failure is reported to the
caller and host error log; committed destruction is not concealed.

Sealed archival remains the route for the remainder of Table A row 1 and all other classes governed
by the base ADR. This record does not activate or require copy/seal/segment lifecycle for
expendable payloads. Both halves remain in population, and each is mutated by its existing writer;
no second resident SQLite writer is added.

ADR-211 §2, Classifier identity, states the unconditional exclusion that separates `backfill-v1` from this policy. A
shared reservation check prevents concurrent archival and
expenditure of the same ID; encountering the other operation's reservation refuses the run. An
already archived event stays Archived and is not converted to Expended. Pending reservations are
resolved before switching mechanisms. The two classifiers keep their distinct meanings and
histories.

### 6. Physical reclaim and the capacity bound

The release must measure the actual sidecar mode; it must not infer INCREMENTAL from an ADR or a
newly created test database. Existing sidecars may be NONE. The current implementation provides no
incremental drain.

For an existing NONE sidecar, conversion is an explicit exclusive maintenance operation, after
paired main/sidecar backup and verified writer quiescence, using SQLite's `auto_vacuum=INCREMENTAL`
plus the required full VACUUM. It is not a startup migration. The initial implementation must
support a stopped conversion that first checkpoints through SQLite, verifies no competing
connections, uses rollback-journal mode for the rebuild where required to avoid duplicating live
data into the counted WAL, restores WAL, and verifies mode 2 and database integrity before serving.
This is a proposed maintenance path, and its exact peak/disk behavior must pass the scratch
acceptance arm before rollout. Temporary-file and rollback-journal disk capacity is checked
separately.

After conversion, the events owner runs bounded incremental-vacuum slices, using ADR-204's specified
mechanics: a typed positive page bound of at most 1,024 pages per slice, step to SQLITE_DONE, zero
busy timeout for a slice with restoration afterward, at most one slice per 500 ms tick, and only
after a fully backfilled WAL observation. Checkpoint between slices. Arm the drain after a committed
prune. Run it off the async worker. Provision this owner explicitly in the events daemon; main-only
checkpoint/FTS wiring does not satisfy it. New sidecars set INCREMENTAL before their first WAL
transition. Existing main files processed by the migration receive their own measured-mode reclaim
procedure through their owner.

Use ADR-204's 5%/4,096-page start and 1%/1,024-page floor as routine defaults. Capacity recovery may
request bounded drain slices below the routine start threshold, but never bypass WAL/backfill/space
guards or the positive bound. Completion reports measured combined bytes, not just freelist
improvement. Pinned readers, a busy writer or a mode mismatch cause a visible failure or skip, not
an unbounded vacuum or a falsely reported shrink.

**The capacity bound.** The bound `B` is operator configuration, in bytes, over the capacity metric.
Unset means no bound is applied; measurement and reporting still run. `B` covers **all** sidecar
writes, including non-allowlisted audits, config-lock records, identity metadata, migration and
maintenance traffic, embedded writers and concurrent admitted batches.

Reaching `B` does not refuse ordinary writes and does not stop the deployment. Refusing writes at
the bound would stop every caller whose dispatch carries an audit obligation, which costs more than
the growth it prevents. Instead, the events owner:

1. raises a durable capacity alarm, recorded on the next run's completion and in the host error log,
   with the measured metric, `B`, the protected backlog and the eligible backlog;
2. starts capacity recovery: prune runs continue on schedule, and the drain may run bounded slices
   below its routine start threshold;
3. refuses to start any maintenance operation whose demonstrated worst-case peak does not fit under
   `B` together with the free disk that operation needs.

What follows is the operator's decision, made on the reported numbers. Nothing in this record
shortens the horizon, widens the list, deletes another event class, reroutes audits into the main
file, truncates a database file with filesystem tools or silently drops an obligation-bearing row to
get back under `B`. The original dispatch/audit failure contract still applies.

**Maintenance admission.** Every maintenance operation that can grow the counted files (conversion,
drain, historical migration and each prune run) reserves a conservative worst-case growth budget
before it starts, including index/page overhead, journal or WAL growth and checkpoint growth. The
budget must be a demonstrated upper bound from the operation's storage and write limits, not a
per-row byte estimate times a row count. An operation whose bound cannot be established is refused.
Reservations serialize across all processes that can write the file. A stat-after-write check,
`max_page_count` alone, or a WAL-only ceiling does not establish the bound. A WAL-mode conversion is
refused unless its worst-case combined-file peak fits.

The release reports measured daily combined-file growth and growth of protected backlog and
expended-ID state. The planning proxy, which assumes file bytes track audit rows, is 118–121 MiB/day
before either amendment. About 21 MiB/day of that comes from rows outside the six keys and is
unaffected by both. With ADR-103 Amendment 7 in `reduced` mode, the six keys write about 3,846
rows/day on the 2026-10-19 counts plus forced rows, about 2.8 MiB/day by the same proxy; this
amendment then holds them to a seven-day live window. Expended-ID records grow by one per expended
row and are permanent; the release measures their size. No plateau of the whole sidecar is promised.
A later residual-class policy may extend service time.

### 7. Before implementation

Until this record is implemented and its acceptance passes, nothing is deleted from either half
under it. No interim hard delete is authorized, no ad hoc SQL purge, no deletion or truncation of a
store file, and no stop of the events-producing deployment because of file size. Write-time growth reduction is
specified by ADR-103 Amendment 7 (Accepted) and is implemented
at `crates/khive-runtime/src/pack/audit_success_rows.rs:217`. A deployment that reaches its configured bound
first is an operator decision made on measured numbers, and this record authorizes no deletion
for that case.

The historical migration in §4 runs only after acceptance of this text and verification of the
maintenance procedure above, with the reviewed cohort, reference and identity reservations,
mandatory run evidence, training-count checks and physical reclaim procedure. Urgency does not waive
any of them.

### 8. Acceptance arms

1. **Exact training invariance.** On a frozen, quiescent paired scratch store, capture the complete
   serialized `brain.event_counts` result with explicit RFC3339 `since` and `until`, identical
   namespace/actor scope, separately for `feedback_explicit`, `phase_started`, `phase_completed` and
   `phase_cancelled`. Include old and recent rows, mixed actors/namespaces and same-timestamp ties.
   Before anything runs, check the baseline against an independent oracle: the expected count for
   each training kind is computed from the seed fixture itself, is nonzero, and must equal the
   captured response, so that an empty or zero baseline cannot make the identity check pass
   vacuously. Run migration, prune and reclaim. Repeat the identical calls: response bytes must be
   identical, without removing fields from the comparison. Exercise exhaustive and bounded modes
   with nontruncated populations; separately verify that existing limit refusals remain refusals.
   Also compare the persisted training rows/observations byte-for-byte. An unfiltered whole-plane
   result is not the invariant, because its Audit total must change.
2. **Independent closed-list oracle.** Seed every allowed key, `knowledge.get`, `gtd.tasks`, another
   generic non-allowlisted Audit verb, schedule/moodboard dedicated Audit keys, all training kinds
   carrying even allowlisted verb strings, and malformed/stamped impostors. Only fully eligible
   allowed generic Audit rows disappear. The expected six literals are owned by the test oracle, not
   imported from the production classifier. A must-fail mutation that widens the production
   allowlist and corresponding stamp assignment makes the non-allowlisted-survival arm red. Removing
   the kind guard makes the training arm red.
3. **Exact matching, time boundary and schedule.** `comm.inbox.extra`, `comm.inbox_preview`, a
   differently cased spelling, and other same-prefix verbs survive. Test one microsecond before
   cutoff, equality, one microsecond after, missing/future timestamps and bad metadata. Only the
   strictly older valid rows qualify. Default/unset archival config does not disable an explicitly
   active policy; an inactive rollout changes no rows. With a controlled clock, an active policy
   starts a run within every hour; a run withheld past its hour raises the alarm at the next
   evaluation and deletes nothing that is not yet eligible; each run's cutoff is computed from that
   run's own start time. A mutation that drops the scheduler makes the alarm arm red.
4. **Reader honesty.** Recent retained counts and costs equal an independently enumerated row
   oracle. Mixed/old polling queries disclose retention incompleteness across count/list/page/brain
   reads. By-ID results distinguish live, unknown, Archived and Expended. A per-request latency join
   returns the retained duration before expiration and unavailable afterward; it never invents a
   duration.
5. **Both halves, identity races and settlement.** Seed identical-ID conflict cases, retries of
   main-resident historical rows, sidecar rows, and observations. Race late appends, reservations,
   same-backend edge writes, pruning and release; inject crashes before/after every durable boundary
   and lose replies. No pruned ID is reinserted in either half; no falsely identical acknowledgement
   is returned. Existing referenced candidates survive. Unknown peers refuse before writes. Exercise
   embedded insertion paths too. Through the production paths (audit flusher, daemon wire, split
   probe and both leaf stores), a retried batch item whose ID is reserved settles Expended exactly
   once, inserts nothing, is not retried, and is not reported as AlreadyPresentIdentical. No
   moodboard implementation change is necessary for its rows to survive.
6. **Historical migration safety.** Review a dry-run cohort/digest, then mutate a candidate
   reference or payload before prune. The final guard retains/refuses it; it does not delete an
   unchecked substitution. Young classified legacy rows remain until seven days;
   malformed/unclassified rows stay. A row carrying a `resource.sampling` object is classified like
   any other generic dispatch row. An old producer cannot add new unstamped rows after activation.
   Rollback before delete preserves row/observation bytes; recovery after delete uses committed
   receipts and cannot recreate payloads.
7. **Physical, peak and drain evidence.** On representative scratch copies in both NONE and
   INCREMENTAL modes, report file bytes, WAL bytes, their sum, mode, page size/count and freelist
   before delete, after delete, during conversion/drain and after final checkpoint. Delete-alone is
   a negative control and must not be credited as shrink. Verify actual shrink when reclaimable
   pages exist, including WAL allocation, identity-index bytes and conversion temporary-space
   demand. Inspect database integrity and surviving row hashes after reopen. ADR-204's acceptance
   arms 2 to 4 apply unchanged to the events sidecar's drain. In addition: each slice releases at
   most 1,024 pages and exactly its page count, measured as the change in freelist count; the slice
   statement is stepped to SQLITE_DONE; at most one slice runs per 500 ms tick; the slice runs with
   zero busy timeout and the connection's previous busy timeout is restored after it; no slice runs
   while the WAL is above its backfill point; the drain runs off the async worker. Must-fail
   mutations: an unbounded `incremental_vacuum` call and a helper that steps the statement once each
   fail this arm. Do not run these destructive arms against the live store.
8. **Capacity bound and failure controls.** Drive a scratch store past a configured `B` with large
   valid audit payloads, concurrent batches, an embedded writer, non-allowlisted traffic, pinned
   readers and maintenance. Ordinary writes keep succeeding and keep their audit obligations; the
   capacity alarm is durable and carries the measured metric, `B` and both backlogs; recovery prunes
   only eligible rows and never shortens the horizon, widens the list or deletes another class. A
   maintenance operation whose demonstrated peak does not fit is refused before it starts, and the
   measured peak of every admitted maintenance operation stays within its reserved budget. Unknown
   bounds and insufficient conversion headroom refuse the operation. A deliberately removed
   maintenance-admission guard fails this arm, and so does a mutation that refuses ordinary writes
   at `B`.
9. **Evidence/recovery.** Fail the attempt append and prove zero destructive effects. Crash after a
   source-half delete commit but before completion and prove its receipt resolves exactly the
   committed batch. Corrupt/miss evidence and prove no further deletion. Enumerate every production
   Audit constructor, every events insertion path and every graph-edge write path covered by the
   guards; an added unclassified emitter or unguarded path fails the census. The census asserts that
   every emitter of `(Audit, events.prune_poll_audits)`, a generic dispatch receipt for that verb
   included, is classified `pinned_while_referenced`; a mutation that classifies the key as
   `expendable` or leaves it unclassified fails the census.

### 9. Implementation fences

**MAY:** implement only this closed polling class, its versioned historical migration, compact
identity/coverage evidence, typed protocol/read changes, source-owned bounded prune/reclaim, the
capacity alarm and maintenance admission. Reuse ADR-211 reservation ordering and ADR-204
drain mechanics with explicit tests of the actually shipped paths. Begin with a stopped maintenance
release for the conversion and the historical migration.

**MAY NOT:** infer expendability from `Audit` alone, `comm.*`, frequency, absence of a known
consumer, or metadata alone; add `knowledge.get` or channel kinds; rewrite training rows; prune
unstamped rows outside the captured migration; suppress initial audit obligations; change ephemeral
dispatch-hook delivery; represent deletion as archival; recreate expired IDs; substitute manual file
deletion/truncation for SQLite reclaim; refuse ordinary writes or stop the deployment at the
capacity bound; or advertise a permanent size plateau from a seven-day age rule.

**Verify by:** all acceptance arms above, paired-store before/after artifacts, peak byte traces and
a measured daily-growth report. No destructive rollout or dependent merge proceeds on this draft
alone.

### Alternatives considered

**Write-time sampling of the polling receipts.** Taken separately, as ADR-103 Amendment 7. It
removes about 80% of new rows at the source and keeps the plane append-only. It does not reclaim
rows already written, does not bound the file, and leaves the sampled and non-success rows growing
without an end, so it complements this record and does not replace it. Counts over sampled rows
are estimates, and per-request durations of omitted rows cannot be recovered; that amendment states
both effects for its readers.

**Aggregate rows per verb, actor, namespace and window.** Rejected for this decision. Per-cell
counts are not the original records: ID lookup and per-request latency joins do not survive
aggregation, and an arbitrary sub-window cannot be answered exactly from a coarse window. Distinct
actors, namespaces and windows are unbounded, so one permanent cell per window is still unbounded
storage, and deleting the original rows would still need either this class or sealed archival. Kept
as separate accounting work if exact long-term polling totals become a product requirement.

**Sealed archival of the polling keys.** Rejected under `backfill-v1`, whose exclusion is fixed by ADR-211 §2.
A future separately specified classifier could provide a sealed-archive alternative. It preserves more forensic value:
by-ID resolution returns Archived and
the payload stays recoverable from sealed media. It needs the same identity reservations, reference
races, historical classification and physical reclaim as this record, plus segment copy, seal,
verify and destruction, and it keeps polling payloads that nothing reads on sealed media
indefinitely. If long-term diagnostics of these receipts turn out to be needed, or the expended-ID
state proves too costly, that alternative requires its own classifier decision.

**Sampling now and archival later.** Rejected as a shortcut. It pays for two transitions, keeps
today's rows until the second ships, and cannot reconstruct events that were never written. It mixes
exact historical counts with sampled estimates, so every reader has to disclose which it is
returning. The adopted combination is write-time reduction plus this class, with a seven-day window
of the remaining rows.

**Refusing writes at the capacity bound.** Rejected. A write-refusing cap stops every caller whose
dispatch carries an audit obligation, which costs more than the file growth it prevents. The bound
instead drives an alarm, capacity recovery and maintenance admission (§6).

**Plain age deletion without identity records.** Rejected. A retried append of a deleted ID would be
inserted again, by-ID reads could not tell an expired event from one that never existed, and
references to the deleted ID would resolve as unknown.

## Acceptance evidence

The nine acceptance arms in §8 remain normative. The tests below are required implementation evidence, not claims of
existing coverage or passing results. ADR-211 §2 owns the backfill exclusion; this record does not repeat its rule.

| ID  | Statement                                                                                                                            | Evidence                                                                                                               |
| --- | ------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------- |
| E1  | Training results and stored training rows are byte-identical across migration, prune and reclaim, with a nonzero independent oracle. | Required `poll_retention_training_invariance`; §8 arm 1; `crates/khive-pack-brain/src/handlers/brain_handlers.rs:351`. |
| E2  | The independent literal oracle accepts exactly the policy's keys and rejects impostors, prefix matches and other kinds.              | Required `poll_retention_closed_keys`; §8 arms 2-3.                                                                    |
| E3  | Only rows strictly older than 604800 seconds qualify; activation, hourly execution and missed-run alarms obey §2.                    | Required `poll_retention_clock`; §8 arm 3.                                                                             |
| E4  | Readers distinguish live, unknown, Archived and Expended; incomplete history is disclosed.                                           | Required `poll_retention_reader_honesty`; §8 arm 4; `crates/khive-pack-kg/src/handlers/get.rs:394`.                    |
| E5  | Retries cannot recreate expended IDs; references, releases and protocol settlement obey the two-half ordering.                       | Required `poll_retention_identity_races`; §8 arm 5; `crates/khive-db/src/stores/event.rs:514`.                         |
| E6  | The frozen historical cohort and final rechecks prevent loose retrospective deletion or unchecked substitutions.                     | Required `poll_retention_legacy_cohort`; §8 arm 6.                                                                     |
| E7  | Reclaim evidence measures combined file/WAL bytes and enforces bounded slices, peak space and writer ownership.                      | Required `poll_retention_physical_reclaim`; §8 arm 7; ADR-204 acceptance arms 2-4.                                     |
| E8  | Capacity pressure preserves ordinary writes and audit obligations while refusing maintenance that lacks a proven peak bound.         | Required `poll_retention_capacity`; §8 arm 8.                                                                          |
| E9  | Audit failures and crash recovery preserve evidence; every insertion/edge/emitter path participates in the census.                   | Required `poll_retention_recovery_census`; §8 arm 9.                                                                   |
| E10 | The backfill and expenditure classifiers cannot claim the same polling ID.                                                           | Required `backfill_poll_retention_disjoint`; ADR-211 §2 and this record §5.                                            |

## Amendment 1 (2026-10-28) -- An interim historical prune before implementation

**Status**: Proposed 2026-10-28. It needs specification approval before it binds. It does not change the status of the
base record.

**Trigger.** §7 says that until this record is implemented and its acceptance passes, nothing is deleted from either
half under it, and that no interim hard delete is authorized (lines 349-350). On 2026-10-28 an operator-authorized
interim run of the §4 historical classifier removed the eligible polling-dispatch audit rows from one deployment's
events store, ahead of implementation. This amendment records the tool, where the run departed from the base text,
the evidence it left, and the bound on any further interim run.

Evidence line numbers refer to commit `b49d4d7dba4b4b47e9b6c32ebd5f412c38ab1411`.

### Decision 1 -- The interim tool and what it does not implement

`kkernel db events-prune` (`crates/khive-db/src/events_prune.rs`, #6192, with the resume fix in #6252) runs
`poll-dispatch-legacy-v1` in three phases: a read-only plan that reports counts by verb, age bucket and retention
reason with the §4 candidate-set digest; a backup through SQLite's online backup API with an integrity check and
per-kind row counts; and an apply pinned to the plan's cutoff and digest, which deletes in bounded transactions with a
per-row recheck and stops on a control mismatch. It implements §4's predicate and digest. It does not implement:

- §3's two-half reservations and the Expended disposition. A by-ID read of a removed event returns the same answer as
  an ID that never existed; §3's disclosed expiration is not in place, and §8 arm 4 does not pass.
- In-store expended-ID records. The apply writes an append-only expended-ID file outside the store (one intent line
  and one commit line per batch, each carrying the digest), which stands in for them (`events_prune.rs:6-8`). The file
  is kept with the backup and is permanent evidence under §4.
- Per-ID historical assignments. The cohort is fixed by the plan's cutoff and digest instead; rows of the six keys
  younger than seven days at the cutoff are classified but not assigned.
- §6's maintenance admission, capacity alarm and physical reclaim. The tool never VACUUMs and never changes
  `auto_vacuum` (`events_prune.rs:10-12`).

### Decision 2 -- The edge-writer precondition and its compensating read

The tool's edge-reference check reads the edge stores on their own connections, so it cannot share a transaction with
the delete; §3 closes that window with main-writer reservations, which the tool does not implement, so the tool
requires that no process write edges to the named stores during the apply (`events_prune.rs:17-24`). The 2026-10-28
run did not meet this precondition: the deployment's daemon served requests throughout. §4's writer-quiesced capture
boundary (line 199) was not met either; rows written during the run were all younger than the cutoff and outside the
candidate set.

The compensating read: after the apply, the plan runs read-only on the backup copy, which holds the pre-apply rows,
against the live edge stores at the same cutoff. The apply is complete only when the candidate count equals the
backup plan's, the digest equals the authorized digest, and no candidate is retained as edge-referenced. This shows
that no edge existing at the time of the read names a removed row. It does not cover an edge written and deleted again
inside the window. For the 2026-10-28 run the read found 4,229,195 candidates, the authorized digest, and no
edge-referenced retention.

### Decision 3 -- Run evidence

For the 2026-10-28 run:

- **Classifier.** `poll-dispatch-legacy-v1`.
- **Capture instant and cutoff.** Plan at 2026-10-28T09:26:15.687092Z; cutoff exactly 604,800 s earlier, `cutoff_us` 1792574775687092.
- **Candidate-set digest.** `798db37643234a224c3371a36aa377df41f6bb6af7115c7dbfe6df3b4fb6a4de`.
- **Rows.** 6,484,537 before; 4,229,195 removed in 2,115 committed batches, none skipped; 2,255,814 after.
- **Removed per key.** `comm.probe` 1,362,010; `comm.inbox` 1,246,609; `comm.cursor_get` 620,827; `comm.heartbeat` 620,385; `comm.cleanup_expired_quarantine` 218,450; `whoami` 160,914.
- **Retained of the six keys.** 398,395 rows younger than the cutoff; none for any other reason.
- **Backup.** Taken before the first batch; integrity check ok; its per-kind row counts sum to the row total of the plan read from the copy.
- **Controls.** The tool's same-run control in each of nine invocations, no mismatch; rows before the cutoff for all 206 retained (kind, verb) pairs equal before and after; `recall_executed` and `config_locked` event counts up to the cutoff equal before and after, read for one namespace (an all-kinds count timed out).
- **Edge read.** Decision 2, clean.
- **Write-ahead log.** One PASSIVE checkpoint after each committed batch, no other checkpoint, no VACUUM.
- **File.** 5,181,235,200 bytes before, 5,181,272,064 after; 764,105 free pages of 1,264,959; `auto_vacuum` NONE.

The main half was not pruned.

### Decision 4 -- Physical reclaim stays under §6

The run returned no bytes to the file system. Later writes reuse the freed pages, so the file does not grow until they
are spent. No conversion, VACUUM or drain runs on this store under this amendment; §6 governs reclaim, and a
deployment that reaches its configured bound first decides on measured numbers, as §7 already states.

### Decision 5 -- Further interim runs

Until §8's acceptance passes, a further interim run of this tool is allowed only when all of these hold:

1. An operator authorizes that run by its cutoff and digest, after reviewing its plan.
2. Its cutoff is no later than the first interim run's capture instant (2026-10-28T09:26:15.687092Z), so it reaches
   only rows that existed when the first population was captured, the rows §4 says acquire age eligibility later. Rows
   written after that instant wait for the implemented record and its write-time stamp.
3. It takes a fresh backup, runs the same controls, and either runs with the edge writers stopped or carries
   Decision 2's read as a named step before it reports completion.
4. It leaves the same evidence as Decision 3 and appends to the same expended-ID file.

Every other deletion under this record waits for §7 as written.
