# ADR-170: Dedicated events daemon — observational writes leave the domain store

- Status: Proposed
- Date: 2026-08-24

## Context

khive runs as a single daemon over a single SQLite file. SQLite serializes writers
per file, so every write — domain mutations and observational telemetry alike —
queues on one writer lane. The event plane (ADR-103) makes the observational share
dominant: a 24-hour production window measured ~76,600 event-plane rows written to
the main store (audit rows one-per-dispatch at ~72% of them, channel-poll lifecycle,
config-lock records, embedder lifecycle, phase and checkpoint records, recall/search/
feedback events) against ~2,540 domain-mutation dispatches in the same window — a
roughly 30:1 ratio. By dispatch count, ~96% of main-store write traffic is
observational. Producing query:
`brain.event_counts(since="2026-08-23T12:00:00Z", exhaustive=true)` — an exact,
non-sampled full-window aggregate (76,624 events); the domain-mutation figure is
the sum of that call's `counts_by_verb` entries for mutating verbs.

Under load this shows up as `writer_task_begin_busy` refusals (ADR-067 Amendment 4)
on domain writes: operations that take under 10ms uncontended time out behind
telemetry. The two write classes also carry different durability requirements —
domain mutations must not be lost; observational rows tolerate losing the last few
seconds on a crash. One writer lane cannot price those classes differently.

The existing daemon already demonstrates the process boundary this ADR needs: a
Unix socket with length-prefixed frames and peer-uid admission
(`crates/khive-runtime/src/daemon.rs`), and all event persistence already flows
through one seam, the `EventStore` trait
(`crates/khive-storage/src/event.rs`) implemented by `SqlEventStore`
(`crates/khive-db/src/stores/event.rs`), with batched writers (ADR-133) and the
brain pack's readers on the same trait.

## Decision

Split event persistence into a dedicated events daemon that owns a separate
SQLite file. Observational writes leave the domain store entirely.

1. **Events daemon.** A new subcommand of the same binary runs an events daemon
   that owns `events.db` beside the main store, writes to it through the existing
   `SqlEventStore`, and binds its own Unix socket using the same framing and
   peer-uid admission as the existing daemon socket. It is the only **resident**
   writer of `events.db`; the sole exception is the daemonless embedded mode of
   point 4, whose short direct appends SQLite's per-file cross-process exclusion
   already serializes.

2. **Forwarding event store (writes).** In the domain process, a
   socket-forwarding implementation of `EventStore` replaces the local one for
   appends: events enter a bounded in-memory queue drained by a background
   forwarder that ships framed batches to the events daemon. The hand-off is
   fire-and-forget. On queue overflow or a dead socket the append is **dropped**,
   a degradation counter increments, and the drop reason is logged. The domain
   request path never blocks on, and shares no lock with, event persistence.
   The bounded-queue drop policy is the loss-tolerant durability class made
   concrete: the loss window is the queue depth plus the flush interval, both
   configuration with defaults stated in code.

3. **Reads.** `query_events`, `count_events`, and `get_event` open `events.db`
   read-only from the reading process. SQLite WAL supports one writer process and
   many reader processes natively. Read connections are short-lived so they do
   not pin WAL checkpoints.

4. **Embedded mode.** One-shot CLI and test contexts without a daemon use the
   in-process `SqlEventStore` against `events.db` directly. SQLite's per-file
   cross-process exclusion covers the rare overlap with a running events daemon;
   every event transaction is a short append.

5. **Lifecycle.** The domain daemon spawns and supervises the events daemon at
   startup. If the events daemon dies, the domain daemon keeps serving, drops
   events loudly (counter + log), and attempts respawn with backoff. Domain
   availability never depends on events-daemon liveness.

6. **Cutover.** New events go to `events.db` from the first boot of this code.
   Rows written before the cutover remain in the main store and are not unioned
   into windowed event queries; recent-window queries converge on the new store
   immediately, and the orphaned history ages out of query windows. This is
   acceptable for the telemetry durability class and is stated here rather than
   hidden. A backfill tool can follow if history proves needed. Storage tenure
   of the legacy rows is likewise explicit: they age in place in the main store
   indefinitely — nothing deletes them as part of this change — until an
   operator runs a purge or backfill tool, which ships separately if wanted.

## Consequences

**Positive.**

- Effective writer lanes double: SQLite's single-writer constraint is per file,
  so the ~96% observational share of write traffic stops competing with domain
  mutations at all.
- Failure domains separate: an events-side stall (index build, checkpoint, disk
  pressure on the events file) can no longer time out a domain write, and vice
  versa.
- Durability classes become code: the drop policy at the bounded queue is the
  loss-tolerant contract, not an annotation.

**Negative / accepted.**

- A new loss mode exists by design: events dropped under overflow or during an
  events-daemon outage are gone. The degradation counter and log line are the
  mandatory visibility for it; the counter is surfaced through the existing
  diagnostics surface so a silent-drop deployment is observable.
- Pre-cutover event history is invisible to windowed queries against the new
  store.
- The binary grows a subcommand and the deployment grows a supervised child
  process.

**Compliance note.** Audit rows on this plane are operational telemetry. Any
future audit class with retention or non-loss requirements is a domain write by
definition and must ride the domain store's transaction, not this lane; adopting
such a class requires revisiting the event-kind classification, not loosening
this daemon's drop policy.

## Alternatives considered

- **In-process side lane (separate file, same process).** Rejected: removes lock
  sharing but keeps one failure domain — a stall in the shared process still
  takes both planes down — and does not create the process boundary later
  consumers of the stream need.
- **Central broker for the event stream.** Rejected: re-centralizes what this
  change decomposes; consumers can tail the events store directly.
- **Multi-process direct writes to `events.db` (no daemon).** Rejected for the
  hot path: reintroduces cross-process writer contention on the busiest file.
  Retained only for the rare embedded mode.

## Forward

A follow-up daemon consuming this stream (recall-weighting state, computed off
the event feed, answering small read-only coefficient queries, degrading soft
when unavailable) builds on the same socket pattern and the same durability
reasoning. It is deliberately out of scope here; validating the hand-off
primitive on the events lane comes first.

## References

- ADR-067 (write-owner daemon; Amendment 4: writer-begin busy contract)
- ADR-103 (event plane)
- ADR-133 (durable audit batching)
- `crates/khive-storage/src/event.rs` (`EventStore` seam)
- `crates/khive-db/src/stores/event.rs` (`SqlEventStore`)
- `crates/khive-runtime/src/daemon.rs` (socket framing and admission pattern)
