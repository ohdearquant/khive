# ADR-136: Fair write admission — execute the queue default-on pathway with production evidence

- **Status:** Proposed
- **Date:** 2026-08-02
- **Relates to:** ADR-135 (write scaling: demand before ownership — this record executes its
  F2 flip pathway), ADR-131 (admission control; Amendment 1 in ADR-135 defers its Decision 1
  default), ADR-067 (write-owner WriterTask), ADR-133 (reduce writer acquisitions on the
  request path), ADR-091 Amendment 5 (checkpoint on a dedicated connection)
- **Reference issues:** #1654 (cross-process writer contention), #1655 (mark-read contract
  divergence), #1657 (empty degraded recall)

## Context

### The measured failure shape

With the checkpoint path moved off the pool writer (ADR-091 Amendment 5, #1652), a
server-side instrument now records every pool-writer admission timeout as a structured
NDJSON row. One afternoon of ordinary concurrent traffic against the deployed server
produced the following, all under normal messaging and knowledge-write load — no bulk
ingest, no misuse:

- Repeated bursts of `timed out after 5s waiting for sqlite writer connection`. In one
  measured instance, two clients doing ordinary message sends and reads in the same
  thirty-second window starved four operations between them. This is a capacity property
  of the write path, not an anomalous workload.
- Bursts show a five-second cadence: sequential rows spaced almost exactly one checkout
  deadline apart, i.e. queued waiters each burning their full fixed deadline and failing in
  turn while the writer stays busy. Effective writer occupancy in those windows was 25-30
  seconds continuous.
- The victim set is wider than callers: best-effort mark-read patches degrade inside
  success-shaped responses (see #1655), the server's own outbox poller loses list queries,
  channel heartbeat persistence fails cycles, and per-model recall silently degrades to
  FTS-only (#1657) — retrieval quality drops precisely when the store is busiest.
- One row in the same series is a `database is locked` failure on a **standalone**
  (non-pool) writer connection — a measured instance of the multi-writer conflict class
  that ADR-135's routing census warned about.
- Every failure retried clean immediately afterward. The store is healthy; admission is
  the bottleneck.

This series is the production caller-error baseline that ADR-135 F2 names as a
prerequisite for evaluating its default flip.

### The mechanism, read at source

- A logical write (a message send, a note create with embeddings) performs **several short
  writer acquisitions**, not one long hold: the row insert, the FTS document, one vector
  insert per registered embedding model, audit accounting, and any outbox row. Embedding
  computation itself runs in parallel tasks **outside** any writer hold — "storage writes
  remain in the parent after this drain succeeds"
  (`crates/khive-runtime/src/operations.rs`, embed drain documentation). Long holds are not
  the problem; sustained near-saturation utilization across many short holds is.
- The deployed write path is the legacy pool writer: one connection behind a
  `parking_lot::Mutex` (`crates/khive-db/src/pool.rs`). That mutex is **unfair** — barging
  is permitted — and checkout waits at most a **fixed deadline** (five seconds by default)
  before returning the timeout error (ADR-131 documents this path and notes "there is no
  admission queue ahead of this mutex").
- Unfair acquisition plus a fixed deadline under saturation produces **non-FIFO waiter
  death**: a later-arriving acquisition can barge ahead of a waiter that has already spent
  most of its deadline, so failures land on the unlucky rather than the last, and cheap
  single-acquisition operations (a mark-read patch) die behind multi-acquisition write
  sequences. The measured "reads lose, sends win" pattern follows directly.

### The governing decision record

ADR-067 accepted a dedicated `WriterTask`: a single task owning a standalone writer
connection, serializing writes through a **bounded FIFO queue** (default capacity 256),
with hardened failure semantics (#1614): a connection that cannot be demonstrably restored
to a clean state retires the task, queued requests fail explicitly as not-started, and
rollback outcomes are classified rather than assumed. ADR-131 built the admission contract
on top of it, including a caller-visible saturation result, and its Decision 1 set the
queue default to on.

**ADR-135 (accepted) deferred that default** via its Amendment 1 to ADR-131: at its
routing census, `SqlBridge` writes opened standalone connections without consulting the
queue, and `with_writer_unmanaged` bypassed the queue unconditionally, so a default-on
queue would have asserted a single-admission property the code did not have. F2 names the
conditions under which the recommendation flips: strict routing (no request-path bypass),
observable direct-writer violations, fail-closed queue spawn, explicit classification of
non-request writers, and an A/B control on production-representative load showing queue-on
lowers caller errors without hidden fallbacks.

This ADR does not contradict that deferral. It records that the two triggering conditions
have materially narrowed since the census, contributes the production failure evidence F2
requires, and schedules the remaining work plus the A/B that F2's own flip clause names.

### Routing census at this record's head

Read at source on the deployed revision:

- `SqlBridge` write dispatches now route through the `WriterTask` when the flag is on
  (`crates/khive-db/src/sql_bridge.rs`: apply-changeset, execute-batch, and single-write
  paths all check the queue handle first); remaining unmigrated dispatches fall back to a
  standalone connection and are enumerable.
- Exactly **three** unconditional `with_writer_unmanaged` call sites remain, all
  transaction-owning maintenance operations: `vec_delete_subjects` and `orphan_sweep`
  (`stores/vectors.rs`), `fts_rename_namespace` (`stores/text.rs`). The hot request path
  (`with_writer`) already prefers the queue.
- The `database is locked` sink row above is the measured cost of the remaining
  standalone-writer traffic coexisting with the pool writer today — the flip is not what
  introduces that class; strict routing is what retires it.

ADR-133 (Proposed) attacks the complementary axis: it reduces how many acquisitions the
request path performs at all. Fewer acquisitions lower utilization; fair admission decides
who waits and who fails when utilization is nonetheless high. The two compose and neither
substitutes for the other. **This record's pathway stands alone: it does not assume
ADR-133 lands first**, and its acceptance criterion is defined against the measured
baseline, not against a post-ADR-133 load profile.

## Decision

### D1 — Complete ADR-135 F2's strict-routing preconditions

The enumerated bypass work becomes scheduled implementation, tracked under #1654:

1. Migrate the three remaining `with_writer_unmanaged` transaction-owning closures onto
   the `WriterTask` transactional dispatch (or classify them explicitly as non-request
   maintenance writers with a documented conflict story), and remove the helper from
   request-reachable code.
2. Migrate or explicitly classify the remaining `SqlBridge` standalone-connection
   dispatches.
3. Queue spawn failure with the flag on is fail-closed (writes error; no silent fallback
   to the mutex path), and any direct-writer acquisition while the queue is enabled is
   observable in the admission-timeout sink's process ledger.

### D2 — A/B under the measured load shape, then flip under F2's clause

Before the code default changes, the queue runs flag-enabled on the deployed server in a
declared window, with the sink recording both arms. The load model is the measured
baseline above — concurrent multi-client messaging with embed-carrying writes, the shape
that produced the cross-client collision — not synthetic-only load. The flip executes when
F2's condition is met: queue-on materially lowers caller errors (the five-second-cadence
burst class disappears) without hidden fallbacks, and a route audit confirms accepted
writes retain their result semantics. Then `write_queue_enabled` defaults to `true` for
file-backed pools.

### D3 — Internal maintenance writers follow the dedicated-connection precedent

Server-internal loops whose cadence must not contend with request traffic (the outbox
poller and channel heartbeat are the measured victims) are candidates for the ADR-091
Amendment 5 pattern: a dedicated standalone connection, sized and scoped per loop, with an
explicit ADR-135-style classification. Per-loop adoption is left to implementation, gated
on the same measurement discipline as D4.

### D4 — Acceptance is measured, and the rollback switch has a named trigger

The admission-timeout sink (one NDJSON file per process) is the acceptance instrument. The
criterion: under load equivalent to the measured baseline, the five-second-cadence burst
class disappears from the sink, and no new failure class appears in its place.

The environment opt-out (`KHIVE_WRITE_QUEUE=0`) remains after the default flips. The named
observables that trigger pulling the default back: (a) queue-saturation results appearing
in the sink at a rate exceeding the baseline's timeout rate under equivalent load, (b) any
writer-task retirement (unclean-connection shutdown) in production, or (c) `database is
locked` rows rising rather than falling after strict routing lands. Any one of these
reverts the default pending diagnosis; the sink rows are the evidence either way.

## Alternatives considered

- **Flip the default now, ahead of strict routing.** Rejected — this is exactly what
  ADR-135 F2 deferred, and the standalone-connection lock failure in the measured series
  shows the multi-writer conflict class is live. A default that asserts single admission
  while bypass paths exist would misreport real writer demand.
- **Raise the checkout deadline.** Hides the failure without changing who fails; latency
  under saturation becomes unbounded and unordered. Rejected.
- **Swap in a fair mutex.** Fairness without backpressure, capacity bounds, or
  observability; waiters still die at a fixed deadline under saturation, just in FIFO
  order. Strictly weaker than the accepted queue that already exists. Rejected.
- **Cost-classed admission lanes** (separate queues for cheap patches vs multi-acquisition
  write sequences). Deferred, not rejected — and per ADR-135 Amendment 2, class-weighted
  service requires its own amendment to ADR-131 before any implementation. FIFO ordering
  plus ADR-133's acquisition reduction may be sufficient.

## Consequences

- Contention becomes ordered waiting with explicit saturation, instead of probabilistic
  five-second failures landing on arbitrary victims — including the server's own
  maintenance loops and the retrieval path's quality degradation.
- The single-writer guarantee is unchanged: exactly one writer task per pool, per
  ADR-067's rationale; strict routing makes the property actually hold rather than
  nominally hold.
- Callers that today implement retry-on-timeout keep working unchanged; they should see
  the timeout branch stop firing on the default path.
- ADR-131's admission contract becomes the deployed reality its metrics assume; the
  undercounting ADR-135 warned about (bypassed writes invisible to admission metrics)
  ends with strict routing.
- The best-effort mark-read divergence (#1655) and the empty-degraded-recall gap (#1657)
  shrink in incidence but remain contract questions in their own right; this record does
  not resolve them.
