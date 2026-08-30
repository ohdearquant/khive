# ADR-103: Resource Attribution Model

**Status**: Accepted
**Date**: 2026-07-08
**Depends on**: ADR-018 (Authorization Gate), ADR-094 (Sequencing-Assertable Lifecycle
Telemetry Events), ADR-091 (WAL Snapshot Lifetime)

## Context

### The daemon has no way to attribute its own resource use

The daemon runs as a single long-lived process serving many concurrent callers (multiple
agent sessions on a dev machine, or multiple tenants on a hosted deployment) over a shared
warm ANN index and a shared in-process embedder. A cold-start rebuild that triggers a full
ANN warm, followed by sustained embedder-serving load across many callers, can hold the
process at multi-core CPU utilization for hours with no record of which caller, or which
kind of work, was responsible. The daemon's log is silent by default (background phases
emit no start/end markers under the default deployment shape; see ADR-094 Context), so
after the fact the question "what was the daemon doing, and on whose behalf" is
unanswerable from any artifact that should answer it.

This is not solely a single-deployment problem. The same gap exists for any hosted
deployment: a router that meters accounted requests with a two-phase reserve/finalize meter
failing closed on the reservation write still only answers "how many accounted
requests did this tenant make," which
is a different question from "how much compute did this tenant's work cost," and it is
structurally blind to any cost that is not itself an accounted request — background warm,
shared-embedder serving on behalf of a caller, and any other work that runs off the
request path. Observability, scheduling, quota enforcement, and accounting all need to answer
some version of "which actor's work cost how much," but nothing today defines a shared
unit that all four could read.

### The foundation this design builds on already exists

The daemon already writes one audit event to the `events` table on every verb dispatch.
`VerbRegistry::dispatch` (`crates/khive-runtime/src/pack.rs`) constructs and appends an
`EventKind::Audit` row on both `Allow` and `Deny` outcomes whenever an `EventStore` is
configured, and both production server-construction paths wire that store unconditionally
once authorization succeeds (`crates/khive-mcp/src/server.rs`). That row already carries
`actor`, `verb`, `namespace`, `outcome`, `session_id`, and `created_at`
(`crates/khive-db/sql/events-ddl.sql`), and `payload` is untyped JSON. The schema also has
a `duration_us` column, but the persisted audit row currently defaults it to 0: the
measured dispatch duration is applied only to the opt-in dispatch-hook event, not to the
`EventStore` row (`crates/khive-runtime/src/pack.rs`, `crates/khive-storage/src/event.rs`). ADR-094 confirms and
builds on this same fact for a different purpose (ordered lifecycle sequencing for the
email-channel poll loop and the WAL checkpoint task): "every verb execution produces one"
audit row, "already wired into the daemon's default construction."

Three consequences follow that reshape how this design should be read:

1. **There is already one event plane keyed by actor and verb.** A design that reads as
   "add a new resource-event stream" misreads the current state. Per-actor, per-op
   accounting does not need a new event stream; it needs the audit row to populate its
   existing `duration_us` column (today defaulted to 0 on the persisted row) and to gain
   three payload fields it does not yet carry: a closed `work_class` tag, `cpu_us`, and a
   deterministic `cost_unit`. Those are enrichments of a row already written, not new rows.
2. **A new row per dispatch is a write-load hazard already characterized in this repo.**
   ADR-094 §5 works this arithmetic for a different variant and rejects unconditional
   per-tick emission on volume grounds. A literal per-op resource row would roughly double
   the existing audit stream, concentrated in exactly the high-throughput windows a quota
   would need to reason about, worsening the already-open events-table retention question
   (ADR-032, ADR-041, ADR-094 §5 all record this as unresolved and deferred).
3. **ADR-094 is the substrate this design extends, not a parallel system.** It already
   establishes additive variants on the closed `EventKind` enum (no migration required,
   since the `kind` column is untyped `TEXT`), best-effort direct `EventStore::append_event`
   calls in place of a new verb, edge-triggered emission for rare state transitions, and a
   deferred prune decision. This design's phase-span accounting is the same shape as
   ADR-094's `ChannelPollStarted` / `CheckpointOutcomeRecorded` variants and should extend
   that taxonomy rather than invent a sibling one.

### Is a subsystem warranted, or is this three small features plus metering a hosted deployment already owns?

The steelman for "no subsystem": dev-machine contention is an OS problem solved with an
advisory external lock convention for GPU work; hosted-deployment metering is an
accounting-layer concern handled elsewhere. What remains, on this reading, is phase
logging, a health field, a thread-priority call, and a read surface over existing events —
none of which needs a unifying model.

This does not hold, for three reasons:

- **A request counter cannot attribute non-request work.** It counts accounted
  requests at the router chokepoint. Background CPU work — warm, shared-embedder serving
  triggered by other callers' requests, maintenance passes — does not cross that counter at
  all. A request counter cannot become a cost meter by definition; it meters a different
  quantity.
- **The external GPU-contention convention is GPU-only and outside the daemon's control
  surface.** The daemon is not a party to it. That is precisely the shape of failure that
  motivates this design: a co-tenant process holding that lock has no visibility into, and
  no way to signal, the daemon's own CPU/embedder bursts.
- **The one thing piecemeal delivery cannot produce is a shared attribution unit.** If
  `work_class` and `cost_unit` are defined once, the same unit is read by an observability
  surface, classed by a scheduling posture, budgeted by a quota check at the Gate, and
  priced by accounting. Built piecemeal, the result is four things that do not share a key: a
  request counter, a wall-clock duration, a phase log, and an external lock — none of which
  can be joined to answer "which actor's ops cost how much, and was it warm or serving."

The subsystem survives this refutation, but resized: it is not a new component, storage
substrate, or event stream. It is a closed `work_class` enum, a `cost` sub-schema riding
the existing audit-row payload, reuse of the Gate's already-locked `Obligation` composition
model for quota, and phase-span `EventKind` variants extending ADR-094. The remainder of
this ADR specifies that model. A per-op resource stream, a subsystem that duplicates an
external accounting meter, or an OS-level enforcement layer the daemon has no privilege to
run are each considered and rejected below.

## Decision

The decision is a **unifying attribution model** — actor × `work_class` × a deterministic
`cost_unit` — riding the event plane ADR-094 already established, not a new subsystem. Five
parts:

### (a) A closed `work_class` enum

Four values, stamped on every event (default `interactive`). Cost dimensions (embedder
time, SQL time, inference time) are payload sub-fields, not classes, because embedding and
inference usually run _inside_ an interactive op rather than as a class of their own.

| `work_class`  | Covers                                                                                                                                                                                                                              | Scheduling posture                 |
| ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------- |
| `interactive` | Request-driven synchronous verb dispatch. Default for all handlers.                                                                                                                                                                 | Highest; never throttled           |
| `warm`        | Cold-start ANN warm, embedder warm, index warm.                                                                                                                                                                                     | Bounded concurrency + low priority |
| `maintenance` | Checkpoint/TRUNCATE, reindex, backfill, prune, vacuum, versioning/merge sync.                                                                                                                                                       | Bounded concurrency + low priority |
| `inference`   | Local model inference run as a distinct background/batch phase (batch rerank warm, batch embed). Does not cover inline rerank or inline embedding inside an interactive op — those are dimensions of that op, not a separate class. | Bounded concurrency + low priority |

A fifth value requires an ADR amendment, matching how the existing closed taxonomies
(entity kinds, edge relations, note kinds, `EventKind`) are governed.

### (b) A `cost` sub-schema as payload enrichment of the existing audit row — no new row

Every dispatch already produces one `EventKind::Audit` row. (Amendment 3 narrows this for one
bounded case — transient audit-lane admission pressure on a fixed, named set of read verbs — where
the row may be dropped best-effort instead of committing; see that amendment for the exact scope.)
This design adds a `resource` object to that row's existing JSON `payload`, with no new row and no
migration:

```jsonc
// events.payload for the existing per-dispatch EventKind::Audit row, gains:
{
  "resource": {
    "work_class": "interactive", // the closed enum above
    "cpu_us": 1840000, // thread CPU time delta, always-on
    "cost_unit": 12, // deterministic i64, op-class weight
    "dims": { // present only when a sampling flag is set
      "embedder_us": 1700000,
      "sql_us": 90000
    }
  }
}
```

`cpu_us` (thread CPU time via `CLOCK_THREAD_CPUTIME_ID` on Linux, the corresponding macOS
thread-time API) is always-on: one clock read before and after the handler runs, at
negligible marginal cost since the row is already written. `cost_unit` is a deterministic
`i64` computed from an op-class weight table (embedding-bearing verbs weigh more than a
verb like `stats`); this is the number quota and accounting count, because it is replayable
independent of measurement noise. `cpu_us` is the measured, non-deterministic number
diagnostics read. The `dims` split (embedder time vs. SQL time vs. inference time) sits
behind a sampling flag: most ops do not need the split, and it is cheap to sample but
expensive to always compute.

Row identity fields already present and reused: `actor`, `verb`, `namespace`, `outcome`,
`session_id`, `created_at`. The existing `duration_us` column becomes the wall-clock
measure and must be populated at this stage (the persisted audit row currently defaults
it to 0).

### (c) Phase-span `EventKind` variants, extending ADR-094's additive mechanism

Background work that is not itself a verb dispatch (an ANN warm pass, a reindex, a
checkpoint-triggered maintenance pass) gets new `EventKind` variants in the same style as
ADR-094's `ChannelPollStarted` / `ChannelPollFailed` / `CheckpointOutcomeRecorded` family:
`PhaseStarted`, `PhaseCompleted`, `PhaseCancelled`. These are additive to the existing
closed `EventKind` enum (no schema migration, since `kind` is untyped `TEXT`) and are
edge-triggered — one pair of rows per phase occurrence, not a per-tick row:

```jsonc
// EventKind::PhaseStarted | PhaseCompleted | PhaseCancelled
{
  "work_class": "warm",
  "phase": "ann_warm",
  "corpus_size": 553000, // on Started
  "wall_us": 41000000, // on Completed / Cancelled
  "cpu_us": 514000000 // on Completed / Cancelled
}
```

Emission is best-effort, direct `EventStore::append_event`, matching ADR-094's emission
contract exactly: not a new wire-surface verb, logged and swallowed on a write failure, and
a no-op when no `EventStore` is configured.

**Write-load bound.** The WAL pathology this repo has previously hit (issue #580, ADR-091)
was a reader pinning the checkpoint boundary — a growth-by-pin failure, not a
growth-by-row-count failure. This design keeps added row count small and bounded
regardless: payload enrichment adds zero new rows (roughly 80-120 extra bytes on a row
already written, well under the SQLite page size, no material change to frame-per-row
cost). Phase-span rows are rare and edge-triggered — on the order of a few per cold start
or per maintenance occurrence, a bound of under 1,000 rows/day even on a busy multi-seat
box, well under 300 KB/day. The rejected alternative, a literal per-op resource row, was
estimated at roughly double the existing audit stream — at an illustrative sustained 10
dispatches/second that is over 800,000 rows/day of pure duplication, concentrated in the
same high-throughput windows a quota would need to reason about. That alternative is
refuted on this arithmetic and is not what this design does. The events-table
retention/prune question stays open and unresolved by this ADR (see Open Questions); this
design adds a small, known, bounded increment to a growth class that already exists, not a
new one.

### (d) Quota as Gate `Obligation` composition — one mechanism, two policies

Quota is enforced at exactly one seam, the Gate (ADR-018), keyed on actor attribution,
never on namespace, matching the standing architecture (namespace is attribution, not
isolation). The mechanism wraps the base `Gate` by composition, the same pattern ADR-018
already anticipates for a `StrictGate` adapter:

```rust
// Obligation::RateLimit is already locked (ADR-018) and currently unenforced.
// Meter is a proposed addition, the counting variant this design needs.
enum Obligation {
    RateLimit { window_secs: u64, max: u32 },  // ADR-018, shape locked, unenforced today
    Meter     { tag: String, dimensions: Vec<String> },  // proposed
    // ...existing Audit / Custom
}

/// Wraps any base Gate.
struct QuotaGate<G: Gate> {
    inner: G,
    counter: Arc<dyn QuotaCounter>,   // durable, shared across the multi-seat topology
    policy: QuotaPolicy,              // Hard (cloud) | Soft (local)
}

trait QuotaCounter: Send + Sync {
    fn usage(&self, actor: &ActorRef, window: Window) -> Result<i64, QuotaError>;
    fn reserve(&self, actor: &ActorRef, est_cost: i64) -> Result<ReservationId, QuotaError>;
    fn finalize(&self, id: ReservationId, actual_cost: i64) -> Result<(), QuotaError>;
}
```

One mechanism, two policies, over the same `cost_unit`:

- **Hard (cloud):** over-budget denies (`Deny` with a rate-limited reason), reserving the
  estimated cost before dispatch and failing closed if the reservation write itself fails —
  mirroring the delivered cloud router's reserve/finalize design rather than
  fire-and-forget metering, which has previously under-counted credits when it lacked a
  synchronous pre-check.
- **Soft (local):** over-budget allows, with an obligation that lowers the op's
  scheduling posture (a separate `qos_posture` field on the obligation, e.g.
  `defer_behind_interactive`), never a refusal. The op's `work_class` is not mutated:
  `work_class` records what the work _is_ (the attribution join key), while the quota
  obligation records how it is _scheduled_; an interactive request that is over budget
  remains attributed as interactive.
- **Advisory-first staging:** meter, expose, and alert now; wire enforcement behind
  configuration later. This matches ADR-018's own precedent of locking an obligation's
  shape before enforcing it (`RateLimit` today) and how other staged-authority surfaces in
  this system have shipped an authoritative floor with advisory behavior above it.

Two separate mechanisms — one local, one hosted — would mean building and reconciling a
meter twice and risking drift on what a "unit" even is. One mechanism with two policies
keeps a single attribution unit across internal stability and accounting, at the cost of
designing the counter's durability and shared-state model once, correctly, for the
multi-tenant topology.

### (e) Contention signal: pull, not push — the daemon does not join the external lock

Co-tenant contention (a long-running GPU-bound measurement sharing the same box as the
daemon) is resolved by a pull-based health surface plus a voluntary, TTL-bounded deferral
flag, not by the daemon blocking on the fleet's external advisory lock convention.

- The daemon exposes busy/dirty state via a health read surface. Co-tenants poll it.
- Any caller can request quiet with a TTL via a dedicated verb. Background phases
  (`warm`, `maintenance`) check this flag at their existing yield points and voluntarily
  defer or slow down. The TTL bound means a crashed requester cannot wedge the daemon
  indefinitely.
- The daemon takes no code dependency on the external lockfile and does not block on it. A
  holder of that lock can additionally request quiet from the daemon before measuring; the
  two conventions coexist without the daemon becoming a party to the lock itself.

Making the daemon a party to the external lock (blocking acquisition before entering a
heavy phase) was considered and rejected: it couples daemon liveness to a convention that
lives outside this repo, and risks priority inversion — a long external measurement could
starve ANN warm indefinitely, which defeats the purpose of a warm daemon. A warm daemon
must degrade to slower under contention, never to stopped.

## Rejected alternatives

| Alternative                                                                       | Why rejected                                                                                                                                                                                                                                                                                                                                                          |
| --------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A new durable `resource` event row on every verb dispatch                         | Roughly doubles the existing per-dispatch audit stream at exactly the high-throughput windows a quota would need to reason about (illustrative: +800K+ rows/day at a sustained 10 dispatches/sec), worsening the already-open events-table retention question instead of adding a small bounded increment. Accounting instead rides the row already written.          |
| A live in-daemon ring buffer as the only accounting layer, with no durable rows   | Loses post-hoc attribution — the exact failure this design exists to close (the motivating incident could not be reconstructed after the fact from any artifact). A ring buffer remains useful for sub-second live snapshots, but only alongside, not instead of, the durable phase-span layer.                                                                       |
| OS-level scheduling as the sole enforcement locus (cgroups, hard thread priority) | The daemon is unprivileged on both surfaces it must run on — an unprivileged dev-machine process and an unprivileged hosted micro-VM — so real preemption is not available. Cooperative in-daemon work classes are the portable, load-bearing mechanism; OS niceness is a thin, best-effort backstop for the lowest-priority classes only, not the primary guarantee. |
| The daemon as a party to the external GPU-contention lock                         | Couples daemon liveness to a convention outside this repo's control and risks priority inversion (a long external hold starves background warm indefinitely). A warm daemon must degrade to slower, never to stopped; resolved instead by a pull health surface plus voluntary, TTL-bounded deferral.                                                                 |
| One omnibus ADR covering accounting, scheduling, contention, and quota together   | Cannot land as one reviewable unit across scopes of different maturity, and would block the near-term accounting/observability slice on quota semantics that are not yet near-term. Sequenced instead as a capstone model plus incremental sub-ADRs (Staged Landing Plan below), so the near-term slice ships without waiting on the others.                          |

## Staged landing plan

**Stage 0 — this ADR (design only).** The attribution model: the `work_class` enum, the
`cost` payload sub-schema, and the deterministic `cost_unit`. No code changes. Names
ADR-094 as the event-plane substrate it extends, ADR-018 as the enforcement seam, and
ADR-091 as the write-load constraint this design is bound by.

**Stage 1 — accounting and observability (near-term).** Extends ADR-094's `EventKind` set
with the `PhaseStarted` / `PhaseCompleted` / `PhaseCancelled` variants; populates the
existing audit row's `duration_us` (today defaulted to 0 on the persisted row) and adds the
`resource` payload enrichment; adds a daemon resource self-report (cumulative CPU, RSS,
current background-phase name) to the existing health read surface; adds a windowed,
per-actor, per-kind read verb over the event plane that also surfaces the new `work_class`
/ `cost_unit` fields. Payload-only and additive-enum-only — no new migration, no new table.
In terms of the filed issues: this stage covers #723 asks 1 and 2 (phase logging, health
self-report) and #724 Ask A (windowed event counts). #723 ask 3 (QoS for warm-path work)
lands in Stage 2, and #724 Ask B (section co-usage aggregates) is a knowledge-pack read
surface outside this ADR's scope, tracked on that issue independently.

### Stage 1 clarification: audit-row write timing

Populating `duration_us` on the existing audit row (above) requires knowing how long dispatch
took, which is only known after dispatch returns. The implementation therefore defers the
Allow-outcome audit row's durable append until after `pack.dispatch` resolves, so the row can
carry the measured `duration_us` and an outcome derived from the actual dispatch result
(`Success` for `Ok`, `Error` for `Err`) instead of a value fixed before the result is known.

The consequence: if the process crashes, panics, or is killed mid-dispatch — after the gate
allows the call but before the deferred row is appended — that one dispatch's audit row is
lost entirely, not merely incomplete. This widens a pre-existing narrow-case trade-off (the
prior implementation deferred only around a single verb shape) to every Allow-outcome verb.

This is accepted, not an oversight. The alternative — appending a row before dispatch runs, so
a crash cannot drop it — means every Allow-outcome row starts life recording an outcome that
has not actually happened yet (`Success`, before the handler has returned) and a `duration_us`
of 0. That row is not more complete; it is a preserved misattribution that a crash prevents
from ever being corrected. The event log is the attribution and cost-accounting plane this ADR
specifies — it is not the crash-forensics surface; the daemon's own process log is, and it is
unaffected by this trade-off.

**Upgrade path (not built in Stage 1).** If audit completeness across a crash ever becomes a
hard requirement (for example, a compliance obligation), the design moves to a durable
pre-dispatch append plus a narrow finalize/update seam on the event store that sets the final
`duration_us` and outcome once dispatch resolves — two rows' worth of state collapsed onto one
row's identity, rather than one row written twice. That seam does not exist today; this
paragraph records the fork so it is findable if the requirement arrives.

**Stage 2 — scheduling and QoS (sub-ADR).** The per-work-class bounded-concurrency
semaphore and lowered thread priority for the `warm` and `maintenance` classes (#723 ask
3); the voluntary quiet-request verb and TTL-bounded deferral at background yield points;
the external-lock reconciliation described in (e). Telemetry-first: class thresholds are
chosen against Stage 1's measured data, not guessed, consistent with this repo's existing
instrument-before-enforcement doctrine (ADR-091).

**Stage 3 — quota at the Gate (sub-ADR).** The `QuotaGate` composition wrapping the base
`Gate`, wiring the durable shared counter, advisory-first per (d) above.

No stage invalidates a filed shape of a prior near-term ask; each stage is additive on top
of the previous.

## Open questions

1. **Whether per-actor embedder CPU can be attributed at all.** The embedder is a shared
   in-process resource serving many callers' ops concurrently. Thread CPU time measured on
   the dispatching task may not capture CPU the embedder thread spends on that task's
   behalf, and embedder time is the dominant cost component in every embedding-bearing op —
   the actual mechanism of the incident that motivates this design. If attribution fails,
   the per-actor `cpu_us` under-counts exactly the cost that matters most, though
   `cost_unit` (a deterministic op-class weight, not a measurement) is unaffected by this
   risk and remains the accounting-safe fallback. This is the riskiest assumption in this
   design and is not resolved here: a measurement spike to confirm or refute per-actor
   embedder-CPU capture is needed before `cost_unit` weights are finalized, ahead of Stage 1
   shipping any accounting-facing use of the number.

   **Resolved 2026-07-08 — NOT-CAPTURED.** The measurement spike returned a verdict against
   per-actor embedder-CPU attribution, for reasons sharper than the mechanism feared above:

   - The embedder does not escape to another thread. `lattice-embed`'s `encode_batch` runs
     synchronously inline on the OS thread polling the dispatching task (`spawn_blocking`
     is used only for one-time model loading, not per-call inference), so the feared
     dispatch-thread/embedder-thread split does not exist.
   - The codebase has no per-thread CPU measurement at all. The only CPU capture is
     process-wide `getrusage(RUSAGE_SELF)` (`khive-runtime/src/resource.rs`), wired into
     the ANN warm-task phase spans and the `comm.health` resource snapshot; the
     per-dispatch audit row's `duration_us` is wall-clock only.
   - Measured: process-wide `getrusage` over-attributes by a factor matching concurrent
     worker occupancy (a reproducible ~2x on a 2-worker runtime, variance under 0.15%
     across 30 samples). Extended to per-dispatch rows it would charge each actor for
     other actors' concurrent CPU — actively contaminated, not merely incomplete.
   - Measured: under contention, half of async tasks migrated OS threads across a single
     `.await` point, so any `CLOCK_THREAD_CPUTIME_ID` bracket wider than the single
     non-yielding embed call is unsound without a same-thread guard, which does not exist
     in the codebase today.

   Consequence: `cost_unit` weights are finalized as deterministic op-class weights (the
   fallback this section anticipated). Measured `cpu_us` is not a calibration source for
   embedding-bearing ops and, where surfaced, is documented as process-wide rather than
   per-actor.
2. **Events-table retention and prune.** This design adds a small, bounded increment to an
   existing, already-unaddressed growth pattern. It does not resolve the retention question
   recorded as open in prior ADRs (ADR-032, ADR-041, ADR-094) and does not attempt to; it is
   flagged here, not decided.
3. **Whether the internal Gate quota or the delivered cloud router meter is authoritative
   in a hosted deployment.** This is a product and resource decision, not a design
   decision, and is deliberately deferred rather than resolved by this ADR. The
   recommendation carried forward is that both meters count the same `cost_unit`, so
   whichever is authoritative in a given deployment, the two coexist without drifting on
   what a unit means; which one gates a request in the hosted product is left to a
   separate, later decision by whoever owns that product surface.

## Consequences

### Positive

- One attribution unit — actor × `work_class` × `cost_unit` — is defined once and read by
  four consumers (observability, scheduling posture, Gate quota, and accounting) instead of
  four independently-defined, non-joinable measures.
- No new storage substrate. Accounting rides the audit row ADR-094 already established as
  the daemon's default construction; phase spans extend the same closed `EventKind`
  mechanism ADR-094 already specifies. No new migration for Stage 1.
- Quota reuses the Gate's existing composition and obligation-staging pattern (ADR-018)
  rather than inventing a second enforcement seam.
- The write-load cost of Stage 1 is small and bounded (payload bytes plus a low daily count
  of edge-triggered rows), quantified against the specific pathology (checkpoint-pin, not
  row-count) this repo has previously hit.
- Sequencing by maturity (Staged Landing Plan) lets the near-term accounting slice ship
  without waiting on quota or scheduling design.

### Negative

- `cost_unit` weights cannot be finalized with confidence until the embedder-attribution
  open question is resolved; shipping Stage 1 before that spike means the diagnostic
  `cpu_us` field may be known-incomplete for embedding-bearing ops from day one.
  Mitigated: `cost_unit` (deterministic, weight-based) is distinct from `cpu_us` (measured)
  precisely so a measurement gap in one does not compromise the other's use for accounting.
- Two Gate-quota policies (hard, soft) over one mechanism means the shared `QuotaCounter`
  durability model must be correct across a multi-seat topology from the start; getting
  this wrong affects both deployments at once, since they share the mechanism.
  Mitigated: reuses the already-delivered cloud reserve/finalize design rather than
  inventing a second model.
- A closed four-value `work_class` enum will eventually need a fifth value (a new
  background-phase category) requiring an ADR amendment, matching every other closed
  taxonomy in this system.

### Neutral

- Stage 1's write-load addition is negligible against current growth (see Decision (c)) but
  is not zero; it is one more small, known contributor to the still-open events-table
  retention question, unchanged in kind.
- The contention-signal design (e) does not replace the external GPU-contention lock
  convention; it coexists with it. Fleet-wide reconciliation of the two conventions across
  processes other than the khive daemon is out of scope for this ADR.

## Not covered (deliberate scope exclusions)

- System-wide or cross-machine scheduling and orchestration outside this daemon.
- Replacing or taking ownership of the external GPU-contention lock convention.
- An external accounting meter and its reserve/finalize/payment integration — the
  internal Gate quota is its analog and is designed to share its `cost_unit`, not to rebuild
  it.
- Events-table prune/retention policy — an inherited open question (see Open Questions).
- Any WAL journal-mode or writer-serialization redesign — out of scope per ADR-091.
- Memory/RSS hard caps or OOM policy — Stage 1 only self-reports RSS; no enforcement.
- Disk-space quota or a free-space floor — an operator/OS concern, not this subsystem.

## References

- ADR-018: Authorization Gate — the Gate as the sole policy seam; `Obligation` composition
  and staging precedent; the `StrictGate`-style wrapper pattern this design's `QuotaGate`
  follows.
- ADR-091: WAL Snapshot Lifetime — the checkpoint-pin write-load pathology this design is
  bound by; the instrument-before-enforcement doctrine Stage 2 follows.
- ADR-094: Sequencing-Assertable Lifecycle Telemetry Events — the event-plane substrate
  (the existing per-dispatch audit row, the closed additive `EventKind` mechanism, the
  best-effort direct `append_event` emission contract) this design extends rather than
  duplicates.

## Amendment 1 (2026-07-13): Batch-Scaled `cost_unit` and Daemon-Startup Warm-Hook Attribution

**Status**: Accepted and implemented in PR
[#927](https://github.com/ohdearquant/khive/pull/927), merged on 2026-07-13 as
`68e9325a039f0975f9caaa64ee5fb834ba874aa2`.

The shipped boundary is precise: successful dispatch audit rows carry deterministic
`resource.cost_unit`; all dispatch outcomes carry `resource.work_class`; and the kg and knowledge
embedder warm hooks emit phase spans. `brain.event_counts` later gained `total_cost_unit` and
`cost_unit_by_verb` aggregation in PR #958. The formula is implemented, but its current weight
table is still the uncalibrated baseline (`base_weight = 1` for every verb and
`per_item_weight = 1` for embedding-bearing verbs). Stage 2 scheduling and Stage 3 quota
enforcement remain outside this amendment and are not implied by its accepted status.

### Context

Open Question 1 (resolved 2026-07-08, above) settled a narrower question: whether
per-actor embedder CPU time is measurable. It is not, so `cost_unit` was already
committed to being a deterministic op-class weight rather than a measured quantity. This
amendment answers the question that resolution left open: given `cost_unit` is
weight-based, where does the weight attach to an actor, and is the attachment as
specified in Decision (b) and (c) correct. A 2026-07-13 spike traced every embedder call
site to its caller and found two concrete gaps, both closed here without opening new
design surface: the illustrative payload in Decision (b) implies one weight per verb
dispatch, which undercounts batch- and fan-out-shaped verbs by orders of magnitude; and
two daemon-startup warmup call sites run entirely outside any dispatch and are invisible
on the event plane today, contrary to what Decision (c) already specifies for background
phase work. Both gaps stay inside Stage 1 (accounting and observability); neither opens
Stage 2 (scheduling) or Stage 3 (quota) scope.

### Part 1: `cost_unit` scales with batch size and per-item model fan-out, for every dispatch

Decision (b)'s illustrative payload shows `cost_unit` as a single number per verb
dispatch, and its own context already establishes that every dispatch gets a weight from
a deterministic op-class table, with embedding-bearing verbs weighing more than a verb
like `stats`. This amendment keeps that every-dispatch scope (it does not narrow Decision
(b) to a subset of verbs) and corrects two undercounts in how the weight for
embedding-bearing verbs is computed: `knowledge.index` pages and embeds up to the full
selected corpus in one dispatch, not a 1000-item ceiling (see the correction below), and
`create` / `memory.remember` fan out one embed task per registered embedding model, not
one embed call per dispatch (`crates/khive-runtime/src/operations.rs:809-822` for entity
creation, the equivalent note-creation fan-out at `:2722-2735`, reached by
`memory.remember` through `crates/khive-pack-memory/src/handlers/remember.rs:123-136`). A
flat, model-count-blind weight would charge a single-model and an N-model `create`
identically, exactly as it would charge a 1-item and a full-corpus `knowledge.index`
identically.

`cost_unit` is redefined as:

```text
cost_unit = base_weight(verb) + per_item_weight(verb) × item_count × model_count
```

computed with checked `i64` arithmetic (`checked_mul` for each product, `checked_add` for
the sum). On overflow, the row's `cost_unit` clamps to `i64::MAX` rather than the field
being omitted, so determinism and replayability are unaffected and the two-meanings rule
for absence, below, is never given a third case.

- `base_weight` and `per_item_weight` are deterministic, hand-set constants per verb
  class, fixed at implementation time and not measured, consistent with Decision (b)'s
  existing requirement that `cost_unit` stay deterministic and replayable, and with Open
  Question 1's resolution that per-actor CPU is not attributable, so `cost_unit` was
  already the accounting-safe fallback. For every verb that is not embedding-bearing,
  `per_item_weight(verb) = 0`, so `item_count` and `model_count` play no role and
  `cost_unit` reduces to `base_weight(verb)` alone, matching Decision (b)'s original
  `stats`-verb illustration.
- `item_count` is read from the dispatch's own JSON result value, already in scope at the
  emission seam described below, not from a new counter, and is defined per
  embedding-bearing verb family:
  - `create` (kind=entity/note), singleton call, and `memory.remember`: `1`.
  - `create`, bulk call (`items=[...]`): not embedding-bearing. The bulk path routes to
    `create_many`, which intentionally skips embedding for bulk structural ingest and
    backfills vectors later via a separate `reindex` call
    (`crates/khive-runtime/src/operations.rs:4698-4709,4893-4894`). Bulk `create`
    therefore falls under the non-embedding-bearing `base_weight(verb)`-only case above
    (`per_item_weight(verb) = 0`), regardless of its `created`/`attempted` summary
    counts; a distinct structural-ingest cost term, scaled by items actually written
    rather than an invoked-model count, is a separate design question this amendment
    does not open. `link` has no embedding-bearing path either (edges carry no embedded
    body) and is not part of this list; its dispatches fall under the same
    non-embedding-bearing case, regardless of its own bulk summary shape
    (`crates/khive-pack-kg/src/handlers/link.rs:61-72,138-148`).
  - `update`, `memory.recall`, `knowledge.search` / `compose`: `1` (each is a single
    entity/note update, or a single query embedding, never a batch).
  - `knowledge.index`: `result["total"]`, the full selected-item count computed across
    all internally paged reads, not the `batch_size` clamp (see the correction below).
- `model_count` is the number of embedding models actually invoked for this dispatch:
  - `memory.remember`: `1` when the caller passes an explicit `embedding_model`
    parameter naming a single model (`crates/khive-pack-memory/src/handlers/remember.rs
    :117-118,134`); otherwise the length of `registered_embedding_model_names()` read at
    dispatch time. `0` is a valid value when no embedding model is registered at all: no
    embed call is issued, so the whole `per_item_weight(verb) × item_count ×
    model_count` term is `0` regardless of `item_count`, and `cost_unit` is
    `base_weight(verb)` alone for that dispatch. The remember still happened; no
    embedding work backs its cost.
  - `create` (kind=entity/note), singleton call: the length of
    `registered_embedding_model_names()` read at dispatch time, with the same `0` case
    as above. `create`'s parameters carry no `embedding_model` field
    (`crates/khive-pack-kg/src/handlers/params.rs:29-45`), so the explicit-single-model
    override above is `memory.remember`-only and does not apply here.
  - `update`, `memory.recall`, `knowledge.search` / `compose`, `knowledge.index`: `1`.
    None of these paths fan out to more than one embedding model; each invokes exactly
    one (a query-embedding model, or the single configured default embedder).

The emission seam is unchanged from Decision (b): the existing deferred per-dispatch
audit-row construction in `crates/khive-runtime/src/pack.rs`, the block spanning the
measured `dispatch_us` (`:1205-1207`) through the Allow-outcome success arms that build
the row (`:1217-1290`). `verb`, the gate-resolved actor and namespace, and the verb's own
success `result: &Value` are all in scope there for the embedding-bearing verb families
above; no plumbing change reaches into `KhiveRuntime::embed*` itself.

**`knowledge.index` correction.** The `batch_size` parameter
(`crates/khive-pack-knowledge/src/knowledge/index_handler.rs:35`, `clamp(1, 1000)`)
bounds the internal SQL page size and the per-chunk embed grouping only, not the
dispatch's total work: when `ids` is not given, the handler pages through the entire
selected corpus (`:60-88`) and returns the full count as `result["total"]` (`:90,
245`). One `knowledge.index` dispatch can therefore process far more than 1000 items,
and `item_count` above uses that full `total`, never the 1000 ceiling.

Payload shape (extends Decision (b)'s sketch; no new top-level fields):

```jsonc
// events.payload for the existing per-dispatch EventKind::Audit row
// (AuditEvent, crates/khive-gate/src/audit.rs), gains an additive `resource` object:
{
  "resource": {
    "work_class": "interactive",
    "cost_unit": 340 // deterministic i64, checked arithmetic per the formula above
  }
}
```

`resource.cost_unit` remains a single `i64` field. The batch, fan-out, and model-count
signals are inputs to the formula that computes it, never separate persisted fields.
`AuditEvent`'s doc comment already states the compatibility contract this relies on: "the
JSON projection of this struct is the public contract" and "field names are stable.
Adding fields is non-breaking" (`crates/khive-gate/src/audit.rs:10-11`). `resource`, and
`cost_unit` within it, is exactly this kind of additive field: no schema migration, no
change to any existing field's meaning.

**Absence has exactly three meanings** (the third added by Amendment 3). An audit row with no
`resource.cost_unit` field is one of:

1. A pre-amendment event, written before a producer emitted this field.
2. A dispatch that errored. The deferred audit path persists an `Error`-outcome row
   (`crates/khive-runtime/src/pack.rs:1260-1274`) with no successful handler `Value` to
   read `item_count` from, so `resource.cost_unit` is omitted rather than computed for
   any failed or incomplete dispatch.
3. A successful dispatch of one of Amendment 3's named read verbs whose audit row was dropped
   best-effort under transient audit-lane admission pressure. Unlike (1) and (2), the caller
   received a successful result — see Amendment 3 for the exact scope and the counters that make
   this case distinguishable from the other two.

Because this amendment keeps Decision (b)'s every-dispatch scope, there is no third
"verb not yet covered" case: every dispatch that resolves `Ok` gets a `resource.cost_unit`
value, embedding-bearing or not — except the bounded admission-pressure case Amendment 3 adds for
a fixed, named set of read verbs, where the row (and the `resource.cost_unit` value it would have
carried) may be absent because it was dropped best-effort, not because the verb is uncovered. A
missing value outside that named case is never inferred, defaulted to zero, or
backfilled after the fact. This mirrors the absence-is-exclusion convention
`counts_by_work_class` already applies to `work_class`: a row with no `work_class` is
skipped, not counted into a default bucket.

### Part 2: daemon-startup embedder warmups get phase-span events, attributed to the daemon principal

`KgPack::warm()` (`crates/khive-pack-kg/src/dispatch.rs:55-57`) and
`KnowledgePack::warm()` (`crates/khive-pack-knowledge/src/pack.rs:101-109`) each call the
runtime's embedder once at daemon construction (or lazily on first pack install) to warm
it. Both run through `PackRuntime::warm(&self)` (`crates/khive-runtime/src/pack.rs:232`),
which takes no `NamespaceToken` argument, so neither call executes inside `dispatch()` or
under the Gate. Before PR #927 there was no actor in scope, no audit row was written (there
was no dispatch to attach one to), and both embedder-warmup passes were invisible on the event
plane entirely, the one concrete gap in Decision (c) as written, which already commits
background phase work of this shape to `PhaseStarted` plus a terminal event.

Both hooks emit `PhaseStarted`, followed by exactly one terminal event: `PhaseCompleted`
for every outcome except a benign shutdown cancellation, and `PhaseCancelled` only for a
benign shutdown cancellation, matching the cited ANN helper's own terminal-selection rule
exactly (`crates/khive-pack-memory/src/ann.rs:1023-1071`). This is one start event and
exactly one of two possible terminal events, never a "triple" or a "pair" emitted
together, and never both terminal events for one warm pass. `work_class: "warm"` is
already a member of the closed enum in Decision (a) (its "Covers" column already names
"embedder warm" explicitly, so no enum amendment is required); `phase:
"kg_embedder_warm"` for `KgPack::warm()`, `phase: "knowledge_embedder_warm"` for
`KnowledgePack::warm()`. `corpus_size` on the `PhaseStarted` payload is optional
(`crates/khive-storage/src/telemetry.rs:112`) and has no meaningful value for an embedder
warmup call (there is no corpus being counted, only a warm invocation); both hooks emit
`None`.

`KgPack::warm()` calls the embedder unconditionally, so its span brackets the whole hook
body unconditionally, matching the current code shape. `KnowledgePack::warm()` only
spawns its embedder warmup when `runtime.default_embedder_name()` is non-empty
(`crates/khive-pack-knowledge/src/pack.rs:101-108`); its span goes only inside that
existing configured-embedder branch, wrapping the spawned `tokio::spawn` body that
performs the actual embed call, not the unconditional part of `warm()` that also runs
`vamana::warm_known_snapshots`. An unconditional span around all of `warm()` would record
a phase for an embedder warmup that never ran whenever no embedder is configured.

Attribution: since `warm()` receives no token, each hook mints its own the same way an
existing precedent in this codebase already does. `khive-pack-memory`'s ANN
background-rebuild task faces the identical shape of problem: it calls
`ensure_ann_for_model` from a daemon-owned background loop with no caller-supplied
token, and mints one via `rt.authorize(Namespace::local())`
(`crates/khive-pack-memory/src/ann.rs:842`) before calling in.
`KhiveRuntime::authorize` resolves the actor from `RuntimeConfig.actor_id`
(`crates/khive-runtime/src/runtime.rs:441-442`): a configured deployment id when one is
set, otherwise `ActorRef::anonymous()` (actor id `"local"`) as the documented fallback
(`crates/khive-runtime/src/runtime.rs:437-440`). This is what "attributed to the daemon
principal" means concretely: not a caller identity, since none exists at startup, but the
same deployment-level actor every other unattributed daemon-internal event already
resolves to. `KgPack::warm()` and `KnowledgePack::warm()` mint their token via
`self.runtime.authorize(Namespace::local())` and use it to emit the `PhaseStarted` /
terminal pair, mirroring the emission helper at
`crates/khive-pack-memory/src/ann.rs:1079-1089` (`emit_ann_warm_phase_event`):
best-effort, `EventStore` resolution failure or payload serialization failure is logged
and swallowed, never propagated to fail the `warm()` call itself. `KhiveRuntime::embed`
itself takes no token (`crates/khive-runtime/src/retrieval.rs:70`), so a token-mint
failure removes only the phase-span telemetry for that warm pass: it is logged, and the
embed warmup call proceeds unaffected by the authorization outcome. This preserves
`PackRuntime::warm`'s documented contract that overriders "must make it idempotent and
infallible: any errors are logged internally, not propagated to the caller"
(`crates/khive-runtime/src/pack.rs:230-231`).

ADR-028 Amendment A2 narrows the persistence side of this rule for read-only
snapshot runtimes: the embedder warm invocation may still run, but the shared
phase emitter returns before `EventStore` resolution and emits no start or
terminal row. A known-rejected append is not considered read-only telemetry;
it would still enter a writer-bearing path. Writable runtimes retain the exact
pair contract above.

This is Stage 1 work: it extends Decision (c) with two additional emission sites, and
requires no `work_class` enum amendment, no `EventKind` amendment, and no schema
migration. It is a wiring gap in two `warm()` implementations, closed by reusing an
already-proven mechanism, not new design surface.

### Consequences

- Positive: the shipped `cost_unit` formula is usable as the deterministic input to future
  Stage 3 quota accounting, since it already accounts for both the batch-size
  and model-fan-out undercounts a naive per-verb weight would have missed at scale. The
  two daemon-startup warmup passes join the rest of the daemon's background work (ANN
  warm, checkpoint, channel poll) on the event plane instead of remaining the one silent
  gap.
- Negative: the deterministic weight table now needs `base_weight` and `per_item_weight`
  per verb class (with `per_item_weight = 0` for every non-embedding-bearing verb), plus
  a fixed `model_count` rule per embedding-bearing verb family; all of it hand-set and
  documented at implementation time, under the same governance Decision (b) already
  established for the single weight it replaces.
- Neutral: this amendment does not change Decision (a)'s `work_class` enum, Decision
  (d)'s quota mechanism, or Decision (e)'s contention signal, and it reaffirms rather
  than narrows Decision (b)'s every-dispatch scope. It corrects how the weight for
  embedding-bearing verbs is computed and closes two Stage 1 emission gaps identified by
  measurement, without opening new design surface or altering the Staged Landing Plan's
  stage boundaries.

## Amendment 2 (2026-07-21): Itemized Executed-Usage Counters — Response-Envelope `usage` and Audit-Row `resource.units`

**Status**: Accepted; partially implemented in PR
[#1231](https://github.com/ohdearquant/khive/pull/1231), merged on 2026-07-22 as
`bb764bf58f06a8c651b93c3d223dd75dcf6f0a74` (ported in the current history as
`b0d92a67aa2f69d070daca6911384057cc9bdca6`).

The shipped implementation provides the seven-counter `UsageContext`, per-operation MCP envelope
`usage`, and the identical frozen audit snapshot at `resource.units`. Production increments exist
for embedding, FTS, vector probes, graph hops, graph-store round trips, and successfully appended
event rows. Runtime entity/note multi-model create paths explicitly propagate the context into
their joined child tasks.

The remaining gaps are observable in current code and are not papered over by this status:

- `ann_jobs_consumed` has no production increment site, so it is always absent/zero.
- spawned per-model work in `memory.recall` does not re-enter the usage scope, so those child
  tasks can under-report `embed_calls`, `vector_passes`, and related work;
- `db_round_trips` is instrumented at graph-read seams, not as a universal counter for every SQL
  or storage call; and
- Part 5's `[request] max_ops` deployment setting is not parsed or enforced. The shipped parser
  ceiling remains the constant `khive_request::MAX_OPS = 100`.

### Context

Decision (b)'s `cost_unit` is deliberately deterministic and request-shaped: it is
computed from the verb, its params, and its result summary, so it is replayable and
noise-free. Amendment 1 fixed its batch and model-fan-out scaling. What it cannot do,
by construction, is report **executed** work: for a growing verb class the executed
work is not derivable from the request at all:

- Graph verbs (`traverse`, `context`, `neighbors`) cost **edges actually expanded**,
  which depends on the graph at the anchor, not on `max_depth`/`hops` (the BFS in
  `khive-runtime/src/graph_traversal.rs` runs one `batch_neighbors` round-trip per
  frontier level; `max_depth` is only a bound).
- Read verbs perform backlog-dependent ANN index maintenance (the warm-path consumer
  drains `ann_write_log` during `memory.recall` / `knowledge.search`), work whose size
  is a function of the backlog at call time, invisible to the caller.
- Optional parameters change the executed shape: `knowledge.compose` without
  `domain_ids` runs a full internal `suggest` (one extra query embed, FTS pass, and ANN
  pass); `knowledge.search(decompose=true)` splits one FTS pass into per-concept
  passes; `context(entity_ids=..., query omitted)` skips the hybrid-search anchor
  entirely.
- Per-engine fan-out differs per verb: `memory.recall` embeds the query once per
  configured engine, while kg `search` embeds only with the default engine.

A consumer that wants to account for usage by actuals — a quota policy under Decision
(d), an external usage reader, or a diagnosing operator — currently has nothing
per-dispatch except the estimate. This amendment adds the measured, itemized
counterpart without disturbing `cost_unit`'s role.

### Part 1: a closed unit-counter vocabulary

Seven counters, all non-negative integers, all "executed during this dispatch, inline":

| Counter             | Unit meaning                                                                                                                                                                                                                                                                             |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `embed_calls`       | one text run through one embedding engine — a batch of k texts through one engine counts k, per engine                                                                                                                                                                                   |
| `fts_passes`        | one FTS5 query execution                                                                                                                                                                                                                                                                 |
| `vector_passes`     | one vector/ANN probe (sqlite-vec KNN or Vamana), per engine per query                                                                                                                                                                                                                    |
| `graph_hops`        | one adjacency entry returned by storage during BFS/traversal, counted **before** visited-set de-duplication at the adjacency-read choke point; multi-edges and self-loops each count as returned, as does the one non-retained sentinel row used to prove traversal work-budget overflow |
| `db_round_trips`    | one batched storage round-trip issued by the handler path                                                                                                                                                                                                                                |
| `ann_jobs_consumed` | one `ann_write_log` job drained by the inline warm-path consumer                                                                                                                                                                                                                         |
| `event_rows`        | one event-plane row **successfully appended** by this dispatch **before the audit snapshot is taken** — the enclosing per-dispatch audit row itself is explicitly excluded (it cannot count itself; see Part 3)                                                                          |

The set is closed and amendment-governed, matching every other closed taxonomy in this
repository. Within a complete `usage` object, a counter that a given verb path does
not touch is simply zero, and omitting a key is equivalent to zero. This equivalence
holds **only for a complete object** — see the failure rule below.

Governance note on extensibility: the object is designed to grow by amendment without
wire breakage — consumers must treat unknown keys as additional counters, so future
additions (`rows_scanned` for GQL execution, byte-scaled blob I/O) are additive.

**Usage reporting is best-effort and can never fail a verb.** If the accumulator is
unavailable, poisoned, or any counter cannot be computed, the op result ships with
**no `usage` key at all** — never a partial object. All-or-nothing keeps the unit
semantics unambiguous: a present object is complete (absent key = zero); an absent
object means "not measured", never "zero work". The op's `ok`/`error` status is
decided solely by the verb's own execution; a reporting failure is logged, never
surfaced as an op error.

### Part 2: collection — a dispatch-scoped inline accumulator

A **dispatch-accounting context** in `khive-runtime`, armed by the registry around
each verb dispatch and incremented at the existing runtime choke points: the embedder
call seam (`embed_query` / per-model embed), the FTS query functions, the
vector-search functions, `batch_neighbors` (adjacency entries returned), the inline
ANN consumer drain, and the event append path. Handlers do not thread a context
parameter; the choke points are few and already centralized.

Plain task-local storage is **not sufficient** as the propagation mechanism: `memory.recall`
runs its per-model embed and ANN fan-out on spawned tasks that are then joined
(`crates/khive-pack-memory/src/handlers/common.rs`), and
Tokio task-locals do not cross `tokio::spawn`. The context is therefore an explicitly
propagated handle (an `Arc`-shared accumulator or equivalent) with the following
scope contract:

- **In scope**: the dispatch task itself; futures it awaits or `join!`s directly; and
  every **request-owned spawned child** — a task spawned by the dispatch whose
  `JoinHandle` is awaited before the response is produced. Request-owned children
  receive the context at spawn and their counts are merged only after the join.
- **Out of scope**: detached background tasks (ANN warm, maintenance passes) and any
  work that can outlive the response. These receive no context and remain attributed
  via Decision (c)'s phase-span events under their own `work_class`. This closes the
  snapshot race by construction: nothing holding the context can still be running
  when the response's `usage` object is read.

Implementation must carry a regression test for each class: a joined-child path
(recall's per-model fan-out counts its embeds), and a detached path (a background
warm contributes nothing to the dispatch's counters).

### Part 3: surfacing — two read paths, no new rows

1. **Response envelope.** Each per-op result entry gains a sibling `usage` object next
   to `ok`/`tool`/`result`:

   ```jsonc
   { "ok": true, "tool": "memory.recall", "result": { ... },
     "usage": { "embed_calls": 2, "fts_passes": 1, "vector_passes": 2,
                "ann_jobs_consumed": 6, "event_rows": 3 } }
   ```

   Always-on (the increments are integer adds on hot paths already doing the work),
   additive to the wire shape, zero-valued counters omitted. Batch requests report
   per-op usage per entry; the aggregate response adds no roll-up (callers sum).

   Compatibility: the new key is wire-additive **for tolerant JSON consumers** —
   decoders that reject unknown fields would break, and no shipped envelope contract
   yet obligates unknown-key tolerance. The claim is therefore scoped: known consumers
   (the MCP client surface, the CLI, the audit tooling) are tolerant; a strict
   third-party decoder is a documented compatibility risk, not covered by a guarantee.

2. **Audit row.** The same object lands under the existing per-dispatch audit row's
   `resource` payload as `resource.units`, alongside Decision (b)'s `cost_unit` and
   `cpu_us`. Payload enrichment only — no new row, no migration, same write-load
   argument as Decision (b). Snapshot ordering: the `usage` object is frozen once —
   after all request-owned children are joined and all non-audit event appends have
   resolved, and **before** the enclosing audit row is written — and that single
   snapshot is serialized into both read paths, which is why `event_rows` excludes
   the enclosing audit row (Part 1): the same exact object appears in the response
   and in the audit payload.

### Part 4: relation to `cost_unit`

`cost_unit` remains the deterministic, replayable number quota logic keys on; `units`
is the measured record of what actually ran. They are complementary, not competing:
the hand-set weight table Amendment 1 left open can now be **calibrated from observed
`units` distributions** instead of guessed, under the same governance Decision (b)
established. Nothing in quota enforcement (Decision d) changes.

### Part 5: request-cap configurability (not implemented)

The shipped batch cap is the protocol constant `MAX_OPS = 100`, defined in
`khive-request` and re-exported through its crate root. The parser stays pure: ADR-016
makes `khive-request` a zero-runtime-dependency parser shared by every transport, and a
config-dependent parse would let the same stored input parse under one deployment and fail
under another.

This amendment also decides that a future `[request] max_ops` setting (default 100, valid
range 1..=`MAX_OPS`) belongs at the runtime boundary after parse, while the 1 MiB input
length bound remains a parser constant. Current code has no such config field or post-parse
deployment check; only the fixed protocol ceiling is enforced. This paragraph records the
accepted placement for future implementation, not a shipped operator knob.

### Consequences

- Positive: usage accounting by executed actuals is available on the instrumented MCP paths
  without estimating solely from request shape; graph-expansion costs become visible
  per-dispatch, and Amendment 1's open weight table gains a partial empirical calibration
  source. The gaps listed in the lifecycle note above bound that claim.
- Negative: seven increment sites to keep honest as retrieval paths evolve; a
  regression test per counter per representative verb is required at implementation
  time so a refactor cannot silently zero a counter, plus the two propagation-class
  tests Part 2 mandates (joined-child counted, detached excluded).
- Neutral: `work_class`, quota mechanism, phase-span events, and the staged landing
  plan are all unchanged; the response-envelope addition is additive for tolerant
  JSON consumers (Part 3's scoped compatibility statement).

## Amendment 3 (2026-08-26): Admission-Pressure Read-Cost Undercount for an Explicit Allowlist

**Status**: Accepted, implemented alongside PR #2228 and extended across the reviewed
cross-pack Assertive surface for khive#2217 (khive#2147/khive#2208).

Decision (b) established that accounting rides the per-dispatch audit row: `resource.cost_unit`
lives in the same `EventKind::Audit` row the dispatch's own audit obligation writes
(`ADR-103:127-152`), and Consequences already states that row "is the daemon's default
construction" with no second row (`ADR-103:401-406`). This amendment narrows what "the row
commits" guarantees for one bounded case: transient admission pressure on the audit lane
itself, for a fixed, named set of read verbs.

### The narrowed guarantee

Under audit-lane admission pressure — `AuditTerminalReason::QueueAdmissionExhausted` (the row was
refused before it could be enqueued) or `AuditTerminalReason::AdmissionDeadlineExpired` (the
caller's bounded wait for the row's commit elapsed while the row was still pending or in-flight) —
the per-dispatch audit row for a verb on `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS` may be
dropped best-effort instead of failing the dispatch. The caller still receives the read's
successful result. The fixed, reviewed set currently contains 39 verbs, grouped by owning pack:

- agent: `agent.observe`;
- blob: `blob.get`, `blob.stat`;
- brain: `brain.event_counts`, `brain.profiles`, `brain.profile`, `brain.resolve`,
  `brain.bindings`;
- comm: `comm.delivered`, `comm.inbox`, `comm.unread`, `comm.thread`, `comm.health`,
  `comm.probe`;
- gtd: `gtd.next`, `gtd.tasks`;
- kg: `get`, `list`, `stats`, `search`, `neighbors`, `traverse`, `context`, `query`,
  `resolve`, `whoami`, `verbs`;
- knowledge: `knowledge.get`, `knowledge.list`, `knowledge.stats`, `knowledge.fold`,
  `knowledge.topic`;
- moodboard: `moodboard.model`, `moodboard.search`, `moodboard.preference`;
- schedule: `schedule.agenda`;
- session: `session.list`, `session.resume`, `session.export`.

The cross-pack source census classifies every current public Assertive handler exactly once.
`memory.recall` (serve-ledger/accounting writes), `db_diagnostics` (PASSIVE checkpoint I/O), and
`knowledge.search` / `knowledge.suggest` / `knowledge.compose` (persistent ANN
consumer/checkpoint maintenance) remain explicitly fail-closed. A new Assertive handler is not
eligible until its side effects are reviewed and the closed census is updated.

**Consequence, stated precisely:** `brain.event_counts`'s `total_cost_unit` and
`cost_unit_by_verb` aggregation (Amendment 1) undercount those 39 verbs by the `cost_unit` of
every row dropped this way — but the two terminal reasons above are not the same fact, and
conflating them into one counter would report a number no operator could act on:

- **`QueueAdmissionExhausted`** never enqueued the row: it is a confirmed, terminal accounting
  loss. It will never commit.
- **`AdmissionDeadlineExpired`** already enqueued the row before the caller's wait elapsed: the
  generation driver still commits or terminally fails it independently of the caller's timeout, so
  at the moment the dispatch returns, whether that row eventually contributes to
  `brain.event_counts` is unresolved, not known-lost. A count of these rows is an upper bound on
  the eventual undercount, not the undercount itself.

The loss is:

- **Bounded to the allowlisted verb set.** No other Assertive handler, and no write of any kind,
  is affected — see "What does not change" below.
- **Bounded to transient audit-lane admission pressure**, not persistent store failure. A
  persistent commit failure for one of these rows is unaffected by this amendment and is handled
  exactly as any other row of its class.
- **Counted and exported on two disjoint counters, not silent and not conflated.** Every
  `QueueAdmissionExhausted` drop increments a dedicated process-wide counter
  (`AUDIT_ADMISSION_REFUSED_OBLIGATIONS` in `crates/khive-runtime/src/pack.rs`); every
  `AdmissionDeadlineExpired` drop increments a separate one
  (`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS`). Both are threaded through
  `VerbRegistry::audit_batch_metrics()` into
  `khive_db::diagnostics::WriterContentionDiagnostics::audit_admission_refused_obligations` and
  `audit_admission_unresolved_obligations` respectively, visible on the `db_diagnostics` verb — an
  operator can read the confirmed loss and the unresolved upper bound separately, instead of a
  single number that cannot distinguish an eventual undercount of zero from one of N.

### What does not change

Writes and every Assertive handler NOT on the allowlist keep hard-fail semantics: a persistent or
admission-pressure commit failure on their audit row still fails the dispatch. This amendment does
not touch `DispatchFailed`, `DispatchObligation` rows for write verbs, gate-denial rows,
unknown-verb rows, or `git.digest` receipts — all of those remain obligation-bearing with no
degradation path.

### Why this is accepted rather than fixed by splitting the row

The alternative — a dedicated accounting row separate from the read-observability row — was
considered and rejected for this PR as new storage-and-plumbing machinery. See the companion
amendment in `docs/adr/ADR-133-incidental-writes-off-the-request-hot-path.md` ("Amendment 1") for
the corresponding narrowing of that record's D4/INV-1 language, which this same PR's change
requires.
