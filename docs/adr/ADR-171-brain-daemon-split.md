# ADR-171: Brain daemon — profile state leaves the domain process

- Status: Proposed
- Date: 2026-08-24 (revised 2026-08-25)

## Context

The brain pack owns recall-weighting state: per-profile posterior state
(`BrainState`, section posteriors, entity posteriors, implicit mass) held
in-memory per namespace, persisted to five tables in the main store —
`brain_event_log`, `brain_profile_snapshots`, `brain_implicit_mass`,
`brain_scorer_dedup` (the claim table that makes scorer feedback
exactly-once, written inside the same fold transaction), and
`brain_serve_ledger` (the serve-accounting table that feedback attribution,
scorer deduplication, and grade backfill read) — with folds applied
synchronously inside verb dispatch. This couples the domain process to
profile serving twice over: brain persistence rides the main store's single
writer lane, and the in-memory posterior state is bound to the domain
process lifecycle — a daemon restart discards warm state and every embedded
host rebuilds it from snapshot on first touch.

ADR-170 split the audit lane into a dedicated events daemon and validated
the hand-off primitive this ADR reuses: a subcommand of the same binary
owning its own SQLite file behind a Unix socket with length-prefixed frames,
peer-uid admission, a flock guard, supervision from the domain daemon, and a
kill-switch. This ADR is the follow-up its Forward section names.

Seam facts, verified in-tree, make this decomposition cleaner than the
events one:

- **The brain tables are pack-internal at runtime.** A workspace enumeration
  of raw-SQL consumers (`grep -rl` over all five table names) finds the
  runtime consumers inside the brain pack only:
  `khive-pack-brain/src/persist.rs`, `fold_gate.rs`, and `serve_ledger.rs`.
  The remaining hits are test modules (`khive-pack-brain/src/tests.rs`,
  `khive-pack-brain/tests/adr133_serve_batch.rs`, the `#[cfg(test)]` module
  of `khive-pack-memory/src/handlers/recall.rs`,
  `khive-db/src/migrations_tests.rs` — the last because the tables' DDL
  lives in `khive-db`'s migration set today), one doc comment
  (`khive-brain-core/src/brain_signal.rs`), and one error-mapping string
  (`khive-storage/src/error.rs`). No consumer outside the brain pack touches
  these tables at runtime, so unlike the legacy `events` table (ADR-170's
  amendment) the whole state can move; the migration home is a
  schema-ownership question this ADR decides in point 7. Implementation
  carries a mechanized guard in the shape ADR-170's amendment established:
  a test that reddens if a raw-SQL reference to a brain table ever appears
  outside the brain daemon's own code path, so pack-internality is held by
  a test, not a survey. Enumeration at implementation depth also surfaced
  what a table-level grep cannot see: the feedback dispatch path commits its
  public event-plane row in the same transaction as the fold (scorer-dedup
  claim, implicit-mass gate, brain event append, snapshot upsert). That
  transaction-level coupling, not table location, is the binding constraint,
  so implementation lands in this order: decouple the fold from dispatch
  first (this ADR's point 3 semantics, in-process), relocate brain storage
  second, move the process boundary to the daemon third.
- **Consumers reach brain state through verbs, not memory.** The recall path
  loads a profile's serving view by dispatching `brain.profile` through the
  verb registry (`khive-pack-memory/src/handlers/recall.rs::load_brain_profile`),
  and feedback lands through `brain.feedback`/`brain.auto_feedback`. The
  registry dispatch is already the interface. The response document recall
  and knowledge composition consume is a bounded serving projection (point
  5), never the fold-side state.
- **Recall's serve accounting is already event-shaped.** Every recall
  appends a durable `RecallExecuted` event carrying the served id list,
  `served_by_profile_id`, `serve_attribution`, the query, actor, and
  latency (`emit_recall_executed_event`). The serve-ledger write
  (`brain.record_serve`) is a best-effort background dispatch of the same
  facts. One durable carrier already exists; point 4 makes it the only one.

## Decision

Move brain state into a dedicated brain daemon that owns `brain.db`.

1. **Brain daemon.** A new subcommand of the same binary owns `brain.db`
   beside the main store (the five brain tables plus its in-memory
   posterior state), binds `khive-brain.sock` derived beside that file, and
   reuses the ADR-170 socket pattern: framing, flock guard, versioned
   protocol with typed refusals. It is the only resident writer of
   `brain.db`.

2. **Socket boundary — explicit, fail-closed, on both ends.** The socket
   path is derived beside `brain.db`, which makes the path predictable, so
   the takeover controls the runtime already implements for the domain
   socket (`khive-runtime/src/daemon.rs`) are required here, not inherited
   by reference:
   - _Bind side_: before binding, the brain daemon vets the socket's parent
     directory with the same trust predicate the domain daemon uses — the
     directory is owned by the daemon's euid or root, is not writable by
     group or other, and passes the swap-resistance check; an unreadable
     stat fails closed. The bound socket is asserted mode `0600`. Every
     accepted connection is admitted by kernel peer-uid
     (`getpeereid`/`SO_PEERCRED`): same-uid only.
   - _Connect side_: peer-uid admission authenticates the client to the
     server but not the server to the client — a foreign listener pre-bound
     at the path would pass no server-side check the client can see. The
     thin client therefore applies the same directory trust predicate to
     the socket path's parent before connecting, and refuses paths that
     fail it. Because the directory is owner-only and swap-resistant, a
     pre-bound impostor socket would require the owner's own uid, which is
     the trust boundary this design accepts (single-user store, ADR-170's
     admission rationale). Configurations that place `brain.db` in a
     directory failing the predicate are refused at both ends rather than
     served insecurely.

3. **Event feed, not a push stream.** The brain daemon consumes the event
   plane by tailing it with short-lived read-only opens (WAL
   one-writer-many-readers), folding feedback and serve events into
   posterior state on its own schedule. Which file it tails is fixed by
   ADR-170's routing, not by this ADR: feedback, recall, and serve events
   are plain-append classes and land in the **domain store's** `events`
   table (they must — serve/selection events are reachable as graph-query
   endpoints there, one of the co-residency consumers ADR-170's amendment
   enumerates), while the audit lane lands in `events.db`. The brain daemon
   tails **both files, each under its own cursor** (point 6). No new
   streaming protocol and no producer-side coupling: a row is folded
   whether the brain daemon was up when it landed or not. All folds are
   tail-driven — the feedback verbs append durable events and return; the
   posterior update happens when the tail reaches the row. Within one
   store, insertion order gives the ordering that matters: a recall's serve
   event lands before any feedback that cites it, so ledger materialization
   (point 4) precedes the feedback fold that resolves against it. No
   cross-store fold ordering is claimed; posterior accumulation is
   evidence-summing and tolerates cross-store interleave.

4. **Recall signals: the durable event is the only carrier.** Today the
   fold path observes recalls through a synthetic, non-persisted dispatch
   event that stamps `target_id` from the first served result
   (`khive-runtime/src/pack.rs`), while the durable `RecallExecuted` row
   carries the id list in `payload.selected` but no `target_id` — replayed
   as-is it would interpret every hit as a miss. This ADR closes that gap
   producer-side, before the cutover fence (point 7):
   - `emit_recall_executed_event` additionally stamps `target_id` with the
     first served result id — the same first-result rule the synthetic hook
     applies today — so the existing interpreter
     (`khive-pack-brain/src/event.rs::interpret`) yields the identical
     `RecallHit`/`RecallMiss` signal from the durable row that it yields
     from the synthetic event; `served_by_profile_id` and
     `serve_attribution` already ride the durable payload.
   - The synthetic in-process hook is deleted in the same change, so each
     recall produces exactly one fold input, not two.
   - The serve ledger becomes fold-side output: the daemon materializes
     `brain_serve_ledger` rows from the `RecallExecuted` events it tails
     (the payload carries every column the current `brain.record_serve`
     write derives), and the `brain.record_serve` verb is retired at
     cutover. The ledger's existing UNIQUE key
     (`namespace, target_id, query_class, served_at`) makes
     materialization idempotent under re-fold. Feedback attribution,
     scorer-dedup resolution, and grade backfill — the ledger's only
     runtime readers — already execute inside the brain pack and move with
     it; their read of the ledger stays a local read inside the daemon.

5. **Coefficient queries serve the existing projection.** The brain verbs
   (`brain.profile` and the other profile lifecycle/read verbs) are served
   in the domain process by a thin client that round-trips the daemon
   socket with a bounded timeout. The response is **byte-compatible with
   the current `brain.profile` response document** — including
   `state_snapshot` (which recall parses into its balanced-recall state)
   and `section_posteriors` (which knowledge composition reads for section
   weighting). Existing verb-level consumers (recall, knowledge suggest)
   change in neither call shape nor response shape. What does not cross the
   socket is the fold-side state: the event log, implicit-mass ledger,
   dedup claims, and raw posterior internals stay private to the daemon.
   The served document is the bounded serving projection consumers already
   parse, not a state transfer.

6. **Replay cursor and fold idempotency.** The `events` tables in both
   source stores are rowid tables (`TEXT PRIMARY KEY` — implicit rowid),
   and SQLite assigns rowids monotonically at insert (max+1), so a store's
   rowid is an authoritative insertion-order high-water mark that
   timestamps are not: a delayed transaction lands with a _later_ rowid
   even when its `created_at` is older, so a rowid cursor cannot skip it.
   The protocol:
   - The brain daemon keeps **one cursor per source store** in `brain.db`
     — the last folded rowid, deliberately not a merged cursor, because
     the two writers share no ordering. Purges of already-folded rows are
     harmless to the cursor; rowid reuse would require deleting the
     newest row, which the append-only event plane and tenure-based purge
     policy (oldest-first) do not do.
   - The fold of a batch of rows and the cursor advance commit in **one
     `brain.db` transaction**. A crash before commit re-reads the same
     rows from the (unchanged) source store; a crash after commit resumes
     past them. There is no window in which a row is folded but the
     cursor unrecorded, so no double-fold and no gap — the mid-sequence
     restart test in point 7 asserts exactly this.
   - Scorer-feedback exactly-once continues to be enforced by the
     `brain_scorer_dedup` claim table inside the same transaction, as
     today; ledger materialization is idempotent by its UNIQUE key
     (point 4). These make even an operator-forced cursor rewind safe for
     the exactly-once classes.

7. **Lifecycle, embedded mode, schema ownership, cutover, kill-switch.**
   The domain daemon supervises the brain daemon exactly as it supervises
   the events daemon. One-shot embedded hosts open `brain.db` directly,
   covered by SQLite's per-file cross-process exclusion. Schema ownership
   moves with the state: the brain daemon creates and migrates `brain.db`'s
   DDL on its own boot path; `khive-db`'s migration set stops creating the
   brain tables in new main stores, and existing main-store copies age in
   place under the same tenure language as ADR-170's legacy rows, covered
   by the same operator purge tool.
   - _Cutover fence._ The producer-side recall-event change (point 4)
     lands first. Cutover then stops in-process folding, snapshots state,
     and records each source store's `MAX(rowid)` **while folding is
     stopped** — an exact fence, since legacy folds run synchronously
     inside dispatch. On first boot the brain daemon imports the
     main-store snapshots and initializes each per-store cursor from the
     recorded fence, then replays newer rows. Because the fence postdates
     the producer change, every replayed recall row carries `target_id`;
     pre-change history is covered by the imported snapshots and is never
     replayed.
   - _Cutover test arm._ Fold a known event sequence with a mid-sequence
     snapshot-and-restart and require the resulting posterior to equal the
     uninterrupted single-pass fold of the same sequence; redden on
     double-fold or gap.
   - _Kill-switch, with compatibility DDL._ An environment variable
     restores legacy in-process behavior. The legacy path cannot assume
     main-store brain tables exist (post-cutover stores are born without
     them), so the brain-table DDL is retained as an idempotent
     compatibility bundle (`CREATE TABLE IF NOT EXISTS`, as the DDL
     already is) that the legacy activation path applies to the main store
     on boot. State follows the same fence discipline in reverse:
     activation with a `brain.db` present imports the daemon's snapshots
     and records the fence at which in-process folding resumes; a later
     return to daemon mode re-imports from the main store at a fence
     recorded the same way. Either direction is a snapshot-plus-fence
     hand-off, so the mode switch is symmetric and repeatable, and the
     fallback works on a store initialized at any point in the sequence.

8. **Fail-soft is the contract — with a typed error split.** Liveness and
   validity are different failures and get different behavior:
   - _Liveness_ (daemon unreachable, connect/round-trip timeout, protocol
     version mismatch — the thin client's transport-class errors): recall
     proceeds un-reweighted (hybrid ranking without the profile terms),
     stamps no serving profile, increments a degradation counter, and logs
     the reason once per outage. This holds on **both** profile-resolution
     paths, including an explicitly supplied `profile_id`: a recall must
     never block on, or fail because of, profile-state liveness. The
     current explicit-path behavior — mapping every `brain.profile` error
     to an input error and aborting — is amended to catch only the
     validity class below; the thin client's typed errors are what make
     the two distinguishable.
   - _Validity_ (the daemon answers, and answers that the named profile
     does not exist or is malformed — a typed refusal, not a transport
     failure): an explicitly supplied `profile_id` still fails the recall
     as invalid input, exactly as today — the caller named a profile that
     verifiably is not there, and serving unattributed results against an
     explicit ask would misreport. The auto-resolution path degrades to
     defaults as today.
   - Feedback during an outage is not lost: it lands in the domain store's
     event plane as it does today (point 3), with that store's durability,
     and the daemon folds it from the cursor on return. The attribution
     consequence of fail-soft is stated rather than implied: feedback on
     an un-stamped recall carries no serving-profile id, so the fold gate
     cannot credit a profile with it — such events are **retained in the
     event plane but refused for profile folds**, and the daemon counts
     them, so the posterior-evidence cost of an outage window is
     measurable instead of silently absorbed.

## Consequences

**Positive.**

- Brain writes (event log, snapshot upserts, implicit-mass moves, serve
  ledger) leave the main writer lane entirely — the remaining
  recall-adjacent write traffic on the domain store after ADR-170.
- Posterior state gets a process lifetime independent of domain-daemon
  restarts, and one authoritative copy instead of per-process rebuilds.
- The event plane becomes the single feedback transport; the fold path
  stops running inside verb dispatch, and the serve ledger stops requiring
  a producer-side verb round-trip at all.

**Negative / accepted.**

- Recall's profile terms become eventually consistent with feedback: a fold
  happens when the brain daemon tails the event, not inside the submitting
  dispatch. The posterior model is already an accumulation of evidence over
  time; a seconds-scale fold lag is within its semantics. The same lag
  applies to serve-ledger materialization; scorer flows that resolve
  against the ledger observe rows after the tail reaches the serve event,
  which insertion order guarantees happens before the corresponding
  feedback row is reached.
- A coefficient round-trip is added to profile-stamped recalls; bounded by
  the socket timeout and removed entirely by the fail-soft path when the
  daemon is down.
- One more supervised child process and one more database file.

## Alternatives considered

- **Keep folds in-process, split only persistence.** Rejected: removes the
  writer-lane share but keeps warm state bound to the domain process and
  keeps every embedded host rebuilding it; the coefficient-query seam is
  what makes state ownership movable at all.
- **Push stream from the events daemon to the brain daemon.** Rejected:
  invents a subscription protocol and a delivery contract the tail-with-
  cursor model gets from SQLite WAL for free, including catch-up after brain
  downtime.
- **Keep `brain.record_serve` as a daemon-served write verb.** Rejected:
  the durable recall event already carries every fact the ledger row needs,
  so a verb round-trip would be a second carrier of the same data with its
  own outage semantics; deriving the ledger fold-side removes the write
  path and inherits catch-up from the cursor.
- **Timestamp-based replay cursor.** Rejected: `created_at` is assigned
  before commit, so a delayed transaction can land with an older timestamp
  after the cursor has advanced past it — a skipped row by construction.
  Insertion-order rowid has no such window.
- **Brain daemon owns the events lane too (one auxiliary daemon).** Rejected
  for now: the two lanes have different durability classes and failure
  domains (audit drops are tolerable; posterior state is not), and the
  four-daemon topology this program is converging on separates them
  deliberately. Revisit only with measurements showing the process count
  itself is a cost.

## Forward

The remaining decomposition candidates on the domain store — the indexing
write path (FTS/vector maintenance) as its own concern — follow the same
primitive and the same analysis discipline: enumerate raw-SQL co-residency
consumers of the tables in question before deciding what moves. That
analysis, not this ADR, decides the next slice.

## References

- ADR-170 (events daemon; the hand-off primitive and its amendment's
  co-residency analysis)
- ADR-104 (posterior serving: multiplier clamp, reprojection weight,
  unreadable-profile attribution rule)
- ADR-133 (durable audit batching — the events-lane producer)
- ADR-081 (fold gate: emit-time effective-weight stamping, replay parity)
- `crates/khive-runtime/src/daemon.rs` (socket directory trust predicate,
  swap-resistance check, 0600 assertion, peer-uid admission)
- `crates/khive-runtime/src/pack.rs` (the synthetic recall dispatch event
  this ADR retires)
- `crates/khive-pack-brain/src/persist.rs` (state persistence:
  `brain_event_log`, `brain_profile_snapshots`)
- `crates/khive-pack-brain/src/fold_gate.rs` (implicit-mass fold gate:
  `brain_implicit_mass`, `brain_scorer_dedup`)
- `crates/khive-pack-brain/src/serve_ledger.rs` (`brain_serve_ledger`
  writes, grade backfill, scorer resolution)
- `crates/khive-pack-memory/src/handlers/recall.rs` (`load_brain_profile` —
  the verb-level serving-projection seam; `emit_recall_executed_event` —
  the durable recall carrier)
- `crates/khive-pack-brain/src/event.rs` (`interpret` — the signal
  interpreter both live and replay paths share)
- `crates/khive-db/sql/events-ddl.sql` (the source-store event schema the
  cursor design reads)
