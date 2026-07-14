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

This is not solely an internal-operations problem. The same gap exists on the commercial
side: the khive-cloud deployment (a separate product codebase, not part of this source
tree) meters billable requests at its router with a two-phase reserve/finalize meter that
fails closed on the reservation write. That external constraint is taken as given here,
not verifiable from this repository. A request counter answers "how many billable
requests did this tenant make," which
is a different question from "how much compute did this tenant's work cost," and it is
structurally blind to any cost that is not itself a billable request — background warm,
shared-embedder serving on behalf of a caller, and any other work that runs off the
request path. Observability, scheduling, quota enforcement, and billing all need to answer
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

### Is a subsystem warranted, or is this three small features plus billing khive-cloud

### already owns?

The steelman for "no subsystem": dev-machine contention is an OS problem the fleet already
solves with an advisory external lock convention for GPU work; cloud metering is a
billing-layer concern that is already delivered. What remains, on this reading, is phase
logging, a health field, a thread-priority call, and a read surface over existing events —
none of which needs a unifying model.

This does not hold, for three reasons:

- **The delivered request counter cannot attribute non-request work.** It counts billable
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
  priced by billing. Built piecemeal, the result is four things that do not share a key: a
  request counter, a wall-clock duration, a phase log, and an external lock — none of which
  can be joined to answer "which actor's ops cost how much, and was it warm or serving."

The subsystem survives this refutation, but resized: it is not a new component, storage
substrate, or event stream. It is a closed `work_class` enum, a `cost` sub-schema riding
the existing audit-row payload, reuse of the Gate's already-locked `Obligation` composition
model for quota, and phase-span `EventKind` variants extending ADR-094. The remainder of
this ADR specifies that model. A per-op resource stream, a subsystem that duplicates the
delivered billing meter, or an OS-level enforcement layer the daemon has no privilege to
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

Every dispatch already produces one `EventKind::Audit` row. This design adds a `resource`
object to that row's existing JSON `payload`, with no new row and no migration:

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
verb like `stats`); this is the number quota and billing count, because it is replayable
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

Two separate mechanisms — one local, one cloud — would mean building and reconciling a
meter twice and risking drift on what a "unit" even is. One mechanism with two policies
keeps a single attribution unit across internal stability and revenue, at the cost of
designing the counter's durability and shared-state model once, correctly, for the
multi-seat topology.

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
   risk and remains the billing-safe fallback. This is the riskiest assumption in this
   design and is not resolved here: a measurement spike to confirm or refute per-actor
   embedder-CPU capture is needed before `cost_unit` weights are finalized, ahead of Stage 1
   shipping any billing-facing use of the number.

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
  four consumers (observability, scheduling posture, Gate quota, and billing) instead of
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
  precisely so a measurement gap in one does not compromise the other's use for billing.
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

- Fleet-wide or cross-machine scheduling and orchestration outside this daemon.
- Replacing or taking ownership of the external GPU-contention lock convention.
- The delivered cloud billing meter and its reserve/finalize/payment integration — the
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

**Status**: Proposed (amendment to ADR-103's Stage 1 design; not yet implemented)

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
  already the billing-safe fallback. For every verb that is not embedding-bearing,
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

**Absence has exactly two meanings.** An audit row with no `resource.cost_unit` field is
one of:

1. A pre-amendment event, written before a producer emitted this field.
2. A dispatch that errored. The deferred audit path persists an `Error`-outcome row
   (`crates/khive-runtime/src/pack.rs:1260-1274`) with no successful handler `Value` to
   read `item_count` from, so `resource.cost_unit` is omitted rather than computed for
   any failed or incomplete dispatch.

Because this amendment keeps Decision (b)'s every-dispatch scope, there is no third
"verb not yet covered" case: every dispatch that resolves `Ok` gets a `resource.cost_unit`
value, embedding-bearing or not. A missing value is never inferred, defaulted to zero, or
backfilled after the fact. This mirrors the absence-is-exclusion convention
`counts_by_work_class` already applies to `work_class`: a row with no `work_class` is
skipped, not counted into a default bucket.

### Part 2: daemon-startup embedder warmups get phase-span events, attributed to the daemon principal

`KgPack::warm()` (`crates/khive-pack-kg/src/dispatch.rs:55-57`) and
`KnowledgePack::warm()` (`crates/khive-pack-knowledge/src/pack.rs:101-109`) each call the
runtime's embedder once at daemon construction (or lazily on first pack install) to warm
it. Both run through `PackRuntime::warm(&self)` (`crates/khive-runtime/src/pack.rs:232`),
which takes no `NamespaceToken` argument, so neither call executes inside `dispatch()` or
under the Gate. There is no actor in scope, no audit row is written (there is no dispatch
to attach one to), and both embedder-warmup passes are currently invisible on the event
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

This is Stage 1 work: it extends Decision (c) with two additional emission sites, and
requires no `work_class` enum amendment, no `EventKind` amendment, and no schema
migration. It is a wiring gap in two `warm()` implementations, closed by reusing an
already-proven mechanism, not new design surface.

### Consequences

- Positive: `cost_unit` becomes usable for Stage 3 quota accounting without a follow-up
  correction once the formula ships, since it already accounts for both the batch-size
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
