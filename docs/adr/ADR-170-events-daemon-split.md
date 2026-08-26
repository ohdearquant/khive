# ADR-170: Dedicated events daemon — the audit lane leaves the domain store

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
SQLite file, routed **by append class** rather than moving the whole event
plane.

**Why not the whole plane.** The legacy `events` table is not a pure
observational sink: it has raw-SQL consumers whose correctness depends on
event rows being co-resident with domain state in one file. Enumerated during
implementation: the schedule drain's creator-provenance fence (a raw query
that fail-closes dispatch when the provenance event is absent), the kg
projection worker's guarded `INSERT INTO events … SELECT … WHERE` (a
single-statement write whose guard reads main-store state), and the graph
query compiler's cross-substrate UNION (`entities`/`notes`/`events`/
`graph_edges` in one SQL statement, including event rows reachable as graph
endpoints). Moving every event to another file breaks each of these by
construction — the first one was caught as a hard test failure, the others
would have failed silently. The write classes divide cleanly instead: the
ADR-133 idempotent audit-batch lane (verb-dispatch audit rows plus the
config-lock records that ride the same flusher — ~62,600 of the ~76,600
measured rows, ~82%) has none of these consumers, while the plain-append
classes (channel lifecycle, embedder lifecycle, phase/checkpoint,
recall/search/feedback, pack provenance) are low-volume domain-adjacent facts
that stay put.

1. **Events daemon.** A new subcommand of the same binary runs an events daemon
   that owns the events database (`<main-file-name>.events.db`, e.g.
   `khive.db.events.db`, derived from the main database's full file name with
   the path canonicalized when it exists — **including the final component**:
   a stem-derived name would silently share one sidecar between `a.db` and
   `a.sqlite`, an uncanonicalized parent would mint distinct sidecars for
   aliases of one database, and an unresolved final-component symlink would
   let a link in an attacker-writable directory relocate the predictable
   sidecar and socket paths beside a file the attacker controls. Derivation
   therefore resolves the configured database path fully before deriving
   either name), writes to it through the existing
   `SqlEventStore`, and binds its own Unix socket using the same framing and
   peer-uid admission as the existing daemon socket. The socket boundary is
   fail-closed on both ends, with the same controls the domain daemon
   enforces (`crates/khive-runtime/src/daemon.rs`): the bind side vets the
   socket's parent directory with the directory trust predicate (owned by
   the daemon's euid or root, not writable by group or other, swap-resistance
   checked, unreadable metadata fails closed) and asserts mode `0600` on the
   bound socket; the connect side applies the same directory predicate to
   the path before connecting, refusing directories that fail it, since
   peer-uid admission authenticates the client to the server but nothing
   otherwise authenticates a pre-bound listener to the client. It is the only
   **resident** writer of that file; the sole exception is the daemonless
   embedded mode of point 4, whose short direct appends SQLite's per-file
   cross-process exclusion already serializes.

2. **Split event store (writes).** In the domain process, the `EventStore`
   the runtime hands out routes by append class. The idempotent audit-batch
   surface (`append_events_idempotent`, ADR-133) goes to the events lane as a
   synchronous framed round-trip, preserving the batch's per-row idempotency
   dispositions; the flusher already batches and already runs off the request
   path, so the round-trip amortizes across the batch. Plain appends
   (`append_event`/`append_events`) stay on the domain store unchanged. A
   fire-and-forget path to the events daemon also ships — a bounded in-memory
   queue drained by a background forwarder; on queue overflow or a dead
   socket the append is **dropped**, a degradation counter increments, and
   the drop reason is logged — for producers that later opt their telemetry
   class into the lane explicitly (channel lifecycle is the first candidate).
   The bounded-queue drop policy is the loss-tolerant durability class made
   concrete: the loss window is the queue depth plus the flush interval, both
   configuration with defaults stated in code. The domain request path never
   shares a writer lock with the audit lane.

   Outage semantics for the synchronous audit lane are bounded at both ends,
   stated here because an unstated retry policy is either a silent drop or an
   unbounded buffer. On a failed round-trip the audit flusher retries a
   generation up to its configured attempt cap with a short backoff
   (defaults: 3 attempts, 20ms), then fails that generation terminally — the
   terminal reason is surfaced to submitters, not retried forever. The
   flusher's pending buffer is hard-capped (default 4096 rows); at the cap
   new submissions are refused with an explicit admission-exhausted outcome
   rather than growing without bound.

   **A surfaced terminal failure fails the submitting operation — the audit
   lane is not loss-tolerant for obligation-bearing rows.** The batch this
   lane carries includes records existing contracts require to be committed
   before an operation reports success (dispatch outcomes, authorization
   denials, accounting rows). The mechanism that holds that contract today
   is unchanged by this ADR: the dispatch path folds the audit obligation's
   outcome into the verb result (`fold_audit_obligation`,
   `crates/khive-runtime/src/pack.rs` — a successful operation with a failed
   audit outcome returns the audit error), exactly as a writer-lane
   admission-exhausted refusal fails the verb on the single-store layout
   today. So during an events-daemon outage the observable behavior is loud
   refusal of the affected operations, never a success report whose audit
   row silently vanished; the loss-tolerant drop class in this design covers
   only the fire-and-forget telemetry queue above, whose producers opted
   into it explicitly. The supervisor's respawn probe bounds how long an
   outage — and therefore the refusal window — lasts.

3. **Reads.** Trait-level reads (`query_events`, `count_events`, `get_event`)
   merge both stores, so consumers on the trait observe one event plane
   whichever side holds a row; windowed queries re-sort the merged prefix in
   the stores' shared order before applying the requested window. The lane
   side of a read is a framed round-trip in daemon deployments and a direct
   open in embedded mode. Raw-SQL readers of the legacy `events` table keep
   reading exactly the rows that never moved. Two consumer classes sit
   outside the trait and get an explicit contract each:
   - _By-id event fetch._ The user-facing `get` path resolves an event id
     against the legacy store first and falls back to the sidecar on a miss,
     so a moved audit row remains fetchable by id. This is a requirement of
     the cutover, not an optimization — without it, ids returned by merged
     listings would dangle for the get path.
   - _Graph addressability._ Two contracts with different mechanics, kept
     separate. The **annotation contract** (ADR-002: an `annotates` target
     may be any existing UUID, events included) is preserved across the
     split: the guarded endpoint existence check for annotation targets is
     a point lookup by id, so it resolves through the same legacy-first,
     sidecar-fallback path the by-id fetch above already requires — a
     moved audit row remains a valid `annotates` target, and an id naming
     no row in either store still fails the check loudly. What narrows is
     **graph-query reachability**, which the ADR-002 relation contract
     does not promise: the graph compiler's cross-substrate union reads
     the main `events` table only, so post-cutover audit-lane rows do not
     appear as traversal endpoints, and this ADR accepts that for the
     audit class rather than extending compiled graph SQL across two
     files. The classes that are traversal-reachable today are exactly
     the plain-append classes this routing keeps in the domain store, so
     the narrowing is confined to rows no compiled query could
     select for the domain-store plane anyway once they move.

4. **Embedded mode.** One-shot CLI and test contexts without a daemon use the
   in-process `SqlEventStore` against the events database directly for the lane side.
   SQLite's per-file cross-process exclusion covers the rare overlap with a
   running events daemon; every event transaction is a short append. The
   shared config resolver emits this socket-less mode for every file-backed
   resolution; only resident daemon hosts upgrade the resolved config to
   socket forwarding, because only they supervise an events daemon. The
   socket path is derived beside the events database it serves — a
   process-global socket location would route a second database's events to
   whichever daemon owned it.

5. **Lifecycle.** The domain daemon spawns and supervises the events daemon at
   startup. If the events daemon dies, the domain daemon keeps serving:
   fire-and-forget telemetry drops loudly (counter + log), audit-obligated
   operations refuse loudly per point 2, and the supervisor attempts respawn
   with backoff. Domain availability never depends on events-daemon liveness;
   audit-obligated verbs share the outage window as refusals, which is the
   durability contract holding, not an availability dependency being added.

6. **Cutover.** New audit-lane rows go to the events database from the first boot of
   this code; the plain-append classes keep writing the domain store, so
   nothing that reads them observes a cutover at all. Audit rows written
   before the cutover remain in the main store; because trait-level reads
   merge both stores, windowed queries still see them until they age out of
   query windows. Raw-SQL consumers of the main `events` table lose sight of
   post-cutover audit rows — acceptable for the telemetry durability class
   and stated here rather than hidden. Storage tenure of the legacy audit
   rows is likewise explicit: they age in place in the main store
   indefinitely — nothing deletes them as part of this change — until an
   operator runs a purge tool, which ships separately. That follow-up is
   worth scheduling rather than optional: at the measured audit rate
   (~55,000 rows/day historically) the accumulated legacy audit tonnage is a
   material share of main-store size, and reclaiming it is the second half
   of this ADR's contention-and-size argument.

7. **Backup and restore.** The sidecar is part of the store, not a cache: any
   backup or restore scheme that covers the main database must include the
   events database beside it, and the two are captured and restored as a
   pair. A restore that resurrects only the main file silently amputates
   post-cutover audit history from the merged event plane, so tooling that
   snapshots `<main-file-name>` must snapshot `<main-file-name>.events.db`
   whenever it exists, and portability/operations documentation is updated
   with the pair rule in the same change that ships the sidecar. Restoring a
   backup taken before the sidecar existed is well-formed — the daemon
   creates an empty sidecar on boot and merged reads simply see no lane rows.

## Consequences

**Positive.**

- Effective writer lanes double: SQLite's single-writer constraint is per file,
  so the audit lane — ~82% of measured event rows, and the one-per-dispatch
  class that scales with load — stops competing with domain mutations at all.
  The remaining plain-append classes (~14,000 rows/day measured) stay on the
  domain store and can migrate later by explicit producer-side opt-in.
- Failure domains separate: an events-side stall (index build, checkpoint, disk
  pressure on the events file) can no longer time out a domain write, and vice
  versa.
- Durability classes become code: the drop policy at the bounded queue is the
  loss-tolerant contract, not an annotation.

**Negative / accepted.**

- A new loss mode exists by design for the fire-and-forget telemetry class:
  events dropped under queue overflow or during an events-daemon outage are
  gone. The degradation counter and log line are the mandatory visibility for
  it; the counter is surfaced through the existing diagnostics surface so a
  silent-drop deployment is observable. The synchronous audit lane shares the
  outage window but not the loss mode — its cost is loud operation refusals
  (point 2), never silently missing rows behind a success report.
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
- **Multi-process direct writes to the events database (no daemon).** Rejected for the
  hot path: reintroduces cross-process writer contention on the busiest file.
  Retained only for the rare embedded mode.

## Forward

A follow-up daemon consuming this stream (recall-weighting state, computed off
the event feed, answering small read-only coefficient queries, degrading soft
when unavailable) builds on the same socket pattern and the same durability
reasoning. It is deliberately out of scope here; validating the hand-off
primitive on the events lane comes first.

## Amendment: `brain.event_counts` consistency under split reads (2026-08-26)

The per-request page cap this daemon enforces
(`khive_runtime::events_split::MAX_QUERY_EVENTS_PAGE_ROWS`, 4,096 rows) applies
to every `QueryEvents` call, not only ones that happen to exceed it by
accident. `brain.event_counts` (ADR-103 Stage 1) previously read its window
with a single bounded `query_events` call, sized up to the pack's own window
cap — sound only as long as that cap stayed under the daemon's. It does not:
`brain.event_counts(exhaustive=true)` explicitly exists to serve windows up to
2,000,000 events, and even the bounded default view's per-kind segregation
(counting `audit` separately from every other kind so a busy kind cannot crowd
a quiet one out of the shared budget — see `fetch_event_counts_window`) can
need more than one page per side. A single wide query for either case is
flatly refused by this daemon.

`crates/khive-pack-brain/src/handlers.rs`'s `collect_events_cursor_walk` fixes
this by walking a strict descending `created_at` cursor in
transport-cap-sized pages (`TRANSPORT_PAGE_ROWS`, bound to
`MAX_QUERY_EVENTS_PAGE_ROWS`), deduplicating same-microsecond ties by event id
at each page boundary, instead of issuing one wide read — the same technique
this ADR's split store already uses internally to keep merged reads inside the
cap. `khive-pack-moodboard`'s `moodboard.train_preference` judgment-snapshot
read applies the identical pattern for the same reason. Both apply uniformly,
whether or not the runtime is actually configured for split/daemon mode, so
neither pack's read behavior depends on that deployment detail.

The tradeoff this walk accepts: the exact/full-window aggregation
`brain.event_counts` documented before this change assumed one atomic
snapshot read. A cursor walk instead issues one independent page (and, at a
timestamp-tie boundary, one independent count) read per step against a live
event plane. The result is a best-effort point-in-time view, not a snapshot —
a row appended while the walk is in flight may be included or excluded
depending on where the cursor stands when it lands, but is never
double-counted (the boundary-microsecond dedup in the walk prevents that).
`window_event_total` is likewise an independent `count_events` read, not
derived from the walked rows, and can disagree with them by whatever appended
or (soft-)deleted between the two reads.

This is the intended consequence of the split, not a regression to revert:
refusing to serve `brain.event_counts` under daemon mode until a snapshot
primitive exists would be strictly worse than a documented best-effort view,
and callers needing a closed population already have the tool for it —
bound `until` in the past, per the handler's own doc. The relaxed contract is
codified in the `brain.event_counts` and `exhaustive` param descriptions in
`crates/khive-pack-brain/src/handlers.rs`, and pinned by regression tests in
`crates/khive-pack-brain/src/tests.rs` (e.g.
`concurrent_boundary_append_between_count_and_next_page_is_excluded_cleanly`,
`max_timestamp_group_is_fully_collected_not_skipped`).

## References

- ADR-067 (write-owner daemon; Amendment 4: writer-begin busy contract)
- ADR-103 (event plane)
- ADR-133 (durable audit batching)
- `crates/khive-storage/src/event.rs` (`EventStore` seam)
- `crates/khive-db/src/stores/event.rs` (`SqlEventStore`)
- `crates/khive-runtime/src/daemon.rs` (socket framing and admission pattern)
