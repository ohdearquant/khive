# ADR-094: Sequencing-Assertable Lifecycle Telemetry Events

**Status**: Proposed
**Date**: 2026-07-04
**Depends on**: ADR-002 (edge ontology, unrelated substrate but same repo conventions),
ADR-041 (event provenance projection), ADR-091 (WAL snapshot lifetime, severity ladder
amendment)

## Context

### The email-channel outage program exposed a lifecycle-observability gap

The #599/#602-#606 program (all merged) hardened the daemon's email-channel poll loop:
role gating so only the daemon polls (#602), fail-closed namespace authorization before
spawning the poll loop (ADR-056 §6), and a durable per-channel heartbeat note (#606). What
it did not produce is a way to assert, in a test or in production, the **order** in which
lifecycle transitions happened: poll started, auth failed, backoff armed, heartbeat
persisted. Issue #617 (the ADR-091 severity-ladder amendment, "2026-07-04 amendment:
severity ladder + `wal_pages` units", `docs/adr/ADR-091-wal-snapshot-lifetime.md:401-422`)
needs exactly this shape of thing for the WAL checkpoint side: an N=3-consecutive-cycle
counter, which requires an ordered history of checkpoint outcomes to count over. Both
problems are the same shape: "did this state cross a threshold and stay crossed for N
consecutive occurrences," and both currently have no ordered log to query.

### Three existing telemetry mechanisms, none built for this

Recon (`.khive/workspaces/20260704/telemetry-recon/TELEMETRY_SURFACE.md`,
`SINGLETON_AUDIT.md`, audited at `origin/main` `c8c16f49`, re-verified byte-identical in
this worktree) found three mechanisms already in production:

1. **`tracing::{info,warn,error,debug}!` lines** throughout `crates/khive-mcp/src/serve.rs`.
   Per issue #599, the auto-spawned daemon's stdout/stderr are both redirected to
   `Stdio::null()` (`crates/khive-mcp/src/daemon.rs:251-252`, function `spawn_daemon`,
   verified identical to recon's citation), so under the default deployment shape every
   tracing line the daemon emits, including all of the channel-poll-loop lines below, is
   discarded. #617's comment thread records that #615 (merged) already applied "the
   option-2 durable-evidence pattern" to the email-channel evidence lane specifically
   (`comm.heartbeat`); #599 stays open because the _general_ stderr-loss problem is
   unresolved.

2. **The `comm.heartbeat` note (#606)** — `crates/khive-pack-comm/src/handlers.rs:900-1030`
   (function `handle_heartbeat`, id computed by `heartbeat_note_id` at `handlers.rs:900-909`
   via a deterministic `Uuid::new_v5` on `(namespace, channel_kind, channel_slug)` so
   repeated calls **update the same row**, `handlers.rs:883-885`). `created_at` is preserved
   as first-seen time across updates (`handlers.rs:916-917`), not bumped per call. It tracks
   `consecutive_failures` (reset on success, incremented on failure, `handlers.rs:996-1013`)
   and retains `last_error` across a subsequent success — real sequence information, but only
   a scalar counter and a "last known bad thing," not the ordered transition sequence itself.
   Durable and queryable via `comm.health()` (`handlers.rs:1060-1075`, `1097-...`); answers
   "what is the state right now," not "in what order did the last N transitions happen."

3. **The append-only `events` table** — `crates/khive-storage/src/event.rs:15-30` (struct
   `Event`, doc comment: "Storage-level event record. Every verb execution produces one.
   Immutable once appended.") backed by `crates/khive-db/sql/events-ddl.sql:4-20`, with
   `idx_events_ns_created_id` on `(namespace, created_at DESC, id DESC)`
   (`events-ddl.sql:35`) and `idx_events_session` on `(namespace, session_id, created_at,
   id)` (`events-ddl.sql:36`). This is genuinely ordered, durable, and cross-process
   (SQLite-backed), unlike (1) and (2). It is already wired into the daemon's default
   construction: `VerbRegistry::dispatch` (`crates/khive-runtime/src/pack.rs:878-951`)
   appends one `EventKind::Audit` row per verb call whenever `self.event_store` is
   `Some(..)` (`pack.rs:922`), and both production server-construction paths set that up
   unconditionally when authorization succeeds — `crates/khive-mcp/src/server.rs:293-296`
   and `crates/khive-mcp/src/serve.rs:915-919` (see "Recon citation corrected" below). Since
   `record_channel_heartbeat` (`serve.rs:419-450`) persists via `registry.dispatch("comm.
   heartbeat", ...)` (`serve.rs:443`), **every channel-poll tick already produces one
   `EventKind::Audit` row today**, on the daemon, independent of this ADR. Its payload is
   the gate's `AuditEvent` (allow/deny decision, actor, verb name — `pack.rs:914-940`), not
   the richer `HeartbeatOutcome` the poll loop itself computed; that richer payload lives
   only inside the heartbeat note (mechanism 2), not inside the `Audit` event row.

`EventKind` (`crates/khive-types/src/event.rs:68-121`) is a closed Rust enum, 26 variants
(`EventKind::ALL`, `event.rs:126-152`; iterated by the round-trip test at `event.rs:822-824`
that resolved the earlier "verified via `EventKind::ALL` at `event.rs:823`" recon note — that
line is a _use_ of `ALL`, not its declaration, which is at `event.rs:126`). None of the 26
names a channel-poll-lifecycle or checkpoint-cycle transition. The SQL side stores `kind` as
a plain `TEXT` column with no `CHECK` constraint (`events-ddl.sql:9`) — the closedness lives
entirely in the Rust type, not the schema.

**Conclusion (load-bearing for this ADR, matching the recon): the ordered, durable
substrate this ADR needs already exists and is already live on the daemon.** The gap is
coverage, not storage: `channel_poll_loop`'s internal decisions (`is_backoff_eligible`,
`log_eligible_poll_failure`, the escalation/de-escalation of backoff state) happen between
`registry.dispatch(...)` calls, inside the loop body (`crates/khive-mcp/src/serve.rs:283-
374`), so no amount of instrumenting `dispatch()` alone captures them. This ADR adds new
`EventKind` variants and explicit `append_event` calls from inside `channel_poll_loop` (and,
for #617, from inside `run_checkpoint_task`), reusing the existing `EventStore` capability
and the same `registry.events(&token)` / `runtime.events(&token)` accessor pattern already
used at the two production wiring sites above. It does not add a new storage substrate.

### Recon citation corrected

The recon (`TELEMETRY_SURFACE.md` §3(b)) stated the `with_event_store` wiring happens at
"`server.rs:294-295` and `serve.rs:758-759` and `serve.rs:1147-1148`" — three sites. Re-
verified against this worktree (byte-identical to the audited SHA, confirmed via `diff
<(git show origin/main:crates/khive-mcp/src/serve.rs) crates/khive-mcp/src/serve.rs`,
zero-line diff): `serve.rs:758` and `serve.rs:1147` are unrelated functions
(`build_registry_for_multi_backend`'s doc comment and `build_server_multi_backend`
respectively). There is exactly **one** `with_event_store` call site in `serve.rs`, at
`serve.rs:918-919` (inside `build_server_multi_backend`'s callee, guarded the same way as
`server.rs:293-296`: `authorize(Namespace::local())` then `events(&tok)`). A workspace-wide
`grep -rn with_event_store crates/` confirms exactly two non-test production call sites
(`server.rs:295`, `serve.rs:919`) plus test-only call sites in `pack.rs`, `khive-pack-kg/
tests/integration.rs`, and `khive-pack-knowledge/tests/fixes.rs`. The substantive claim
("this is wired for the production daemon/server") holds; the specific second citation
was one call site, not two.

A second correction: the recon states "two independent, structurally identical hand-rolled
[`CaptureSubscriber`] harnesses exist" (`serve.rs` and `checkpoint.rs`). There are, in fact,
**three**: `crates/khive-runtime/src/pack.rs:2554-2577` implements the same `tracing::
Subscriber` pattern (doc comment at `checkpoint.rs:608-609` explicitly cross-references it:
"Mirrors the capture subscriber in `khive-runtime/src/pack.rs`'s gate-dispatch tests"), used
to assert on the `gate.check` audit tracing line rather than the WARN/DEBUG escalation
decision the other two assert on. This strengthens, rather than weakens, this ADR's case in
Decision 3 below: the duplicated-harness pattern is not a two-off, it is a recurring
response to "I need to assert on a tracing event and there is no persisted analog," and a
third has already been built independently.

## Decision

### 1. Event taxonomy: additive variants on the existing closed `EventKind` enum

`EventKind` stays a closed, compile-time Rust enum (matching how the 26 existing variants
are declared, `event.rs:68-121`) — not an open `kind: String` free-form field. The SQL
column is already untyped `TEXT` (`events-ddl.sql:9`), so this is purely a Rust-side
addition; no migration is needed to add variants, only to add rows using them. Rationale:
an open string kind would let a typo silently create a new, unindexed telemetry category
that no query or dashboard is written against; the existing `EventKind::ALL` +
`FromStr`/`Display` round-trip test (`event.rs:822-824`) is the mechanism that keeps every
consumer (a query helper, a health check) honest about the full set. New variants:

- `ChannelPollStarted` — start of one `channel_poll_loop` tick for one `(kind, slug)`.
  Emitted once per tick per channel, **not** conditional on outcome. See Decision 5 for why
  this is the one unconditional (non-edge-triggered) variant in the taxonomy.
- `ChannelPollSucceeded` — the poll's `Ok(envelopes)` branch (`serve.rs:316`), payload
  carries `envelope_count`.
- `ChannelPollFailed` — the poll's `Err(e)` branch (`serve.rs:344`), payload carries
  `error_class` (`auth | transport | config`, from `channel_error_class`, `serve.rs:394-
  402`) and the error message.
- `ChannelBackoffArmed` — fired only on a `log_eligible_poll_failure` escalation edge
  (`serve.rs:462-482`, `tick.should_warn`), payload carries the new backoff step and delay.
  This directly supersedes the WARN-vs-DEBUG decision that today only exists as a tracing
  level (see Decision 3).
- `ChannelBackoffReset` — fired when `backoff.record_success()` (`serve.rs:317-318`) clears
  a previously-armed backoff state (i.e., the channel had a nonzero backoff step and this
  tick reset it to zero). Not fired on an already-healthy channel's routine success.
- `ChannelHeartbeatPersistFailed` — the `record_channel_heartbeat` best-effort-write-failed
  path (`serve.rs:445-448`). Today this failure is only visible as a discarded `tracing::
  warn!` line under #599's default deployment shape; this variant makes "the durable
  heartbeat write itself failed" durably observable without requiring stdout capture.
- `ConfigLocked` — fired once, at first read, for each `OnceLock`-cached env config value
  identified by the singleton audit (`crates/khive-pack-memory/src/handlers/common.rs:40`
  `recall_profile_enabled`, `:45` `ann_overfetch_max_rounds`;
  `crates/khive-pack-kg/src/handlers/context.rs:35` `context_profile_enabled`). Payload
  carries the config key and the locked-in value. See Decision 7.
- `CheckpointOutcomeRecorded` — one row per checkpoint cycle where `wal_pages` crosses (or
  remains above, on a consecutive-failure count) `warn_pages`, feeding #617. See Decision 4.

Non-goals for this variant set are listed in Decision 8; in particular, boot consolidation
(`build_registry_for_multi_backend`) already fails loud via `anyhow::bail!` on error
(recon §1, "Tracing sites NOT in scope") rather than logging, so it needs no new
`EventKind` — a non-zero process exit is already an unambiguous signal.

### 2. Emission contract: best-effort, in-process, direct `append_event`, not a new verb

`channel_poll_loop` already holds a `khive_runtime::VerbRegistry` by value (`serve.rs:283-
287`, parameter `registry`), the same type whose `dispatch()` already persists `Audit`
events (`pack.rs:922`) — but `VerbRegistry`'s `event_store` field is private
(`pack.rs:709`), with no accessor besides the internal use inside `dispatch`. This ADR adds
one small public accessor:

```rust
impl VerbRegistry {
    /// Return the configured event sink, if any (mirrors the internal use in `dispatch`).
    pub fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.event_store.clone()
    }
}
```

`channel_poll_loop` and `run_checkpoint_task` (for #617) call `registry.event_store()` once
per tick (cheap `Arc` clone or `None`) and, at each lifecycle site, construct an `Event` via
`Event::new(namespace, "channel_poll_loop", EventKind::ChannelPollFailed,
SubstrateKind::Event, actor)` (mirroring `Event::new`'s signature, `event.rs:35-42`, and the
existing construction at `pack.rs:924-937`) and call `store.append_event(event).await`.
This does **not** go through `registry.dispatch()` and does **not** add a new wire-surface
verb (per Decision 8's non-goal) — it calls the storage-level `EventStore::append_event`
trait method directly (`crates/khive-storage/src/event.rs:190`), the same trait `dispatch`
itself uses.

Sync-inline vs. best-effort: **best-effort, matching the #606 precedent.** `record_channel_
heartbeat`'s own failure path (`serve.rs:445-448`) already establishes that a durable-write
failure on this loop is logged and swallowed, never propagated to interrupt polling — the
loop's job is to keep polling channels, not to guarantee telemetry durability. The same
`if let Err(e) = store.append_event(...).await { tracing::warn!(...); }` shape applies to
every new call site in this ADR: on an append failure, `tracing::warn!` (which, per #599,
may itself be discarded under the default deployment shape — an acknowledged, not hidden,
limitation) and continue the loop.

Namespace: events append into `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` (the same constant
`record_channel_heartbeat` already uses at `serve.rs:427`), not `ingest_namespace`, so
channel-lifecycle events and channel-health notes live in the same namespace and a single
query can join or filter across both.

### 3. Ordering and sequencing-assertability: retire the duplicated `CaptureSubscriber` harnesses

A test asserts sequencing by querying `events` filtered by `kind IN (...)` and a `session_id`
or a synthetic per-test namespace, ordered by `(created_at, id)` — the exact shape
`idx_events_ns_created_id` and `idx_events_session` already support (`events-ddl.sql:35-36`).
The assertion pattern this ADR specifies:

```rust
let rows = event_store
    .list_events(EventFilter { namespace: ns, kind: Some(EventKind::ChannelBackoffArmed), .. })
    .await?;
assert_eq!(rows.len(), 1);
assert!(rows[0].created_at > poll_started_row.created_at);
```

This retires the three duplicated `tracing::Subscriber` capture harnesses named
`CaptureSubscriber` (`crates/khive-mcp/src/serve.rs:3097-...`, module
`eligible_poll_failure_log_tests`; `crates/khive-db/src/checkpoint.rs:610-...`; and
`crates/khive-runtime/src/pack.rs:2554-2577`, gate-dispatch tests — see "Recon citation
corrected" above for the third). Once `log_eligible_poll_failure`'s escalation decision is
itself an appended `ChannelBackoffArmed` row and the WAL WARN-ladder decision (#617) is an
appended `CheckpointOutcomeRecorded` row, both `serve.rs`'s and `checkpoint.rs`'s tests can
assert on persisted state via one shared query helper instead of maintaining independent
`Subscriber` implementations. This ADR does not remove the existing `tracing::warn!`/`debug!`
calls (human-readable log lines remain useful when logs are actually captured); it adds the
persisted alternative and specifies that **new** tests for this behavior should assert on
the persisted row, not a captured tracing event. Existing `CaptureSubscriber` tests are not
deleted by this ADR (that is an implementation-time cleanup, out of scope for a docs-only
ADR), but they become removable once the corresponding `EventKind` lands.

### 4. #617 consumer: windowed query over `CheckpointOutcomeRecorded`, this ADR provides the substrate only

`run_checkpoint_task` emits one `CheckpointOutcomeRecorded` row per cycle in which
`wal_pages >= config.warn_pages` (`checkpoint.rs:277`, `above_warn`), payload carrying
`wal_pages`, `warn_pages`, and whether this cycle's `crossing_warn` state is a fresh crossing
or a continued-above-threshold cycle. #617's N=3-consecutive-cycle WARN detection
(`docs/adr/ADR-091-wal-snapshot-lifetime.md:409-414`, "WARN: `wal_pages` fails to drain back
below `warn_pages` across N = 3 consecutive checkpoint cycles") reads this as a windowed
query: the 3 most recent `CheckpointOutcomeRecorded` rows for the process's namespace, all
`kind = 'checkpoint_outcome_recorded'` with `payload.above_warn = true`, ordered by
`(created_at, id) DESC` — if all 3 have `above_warn = true` and no `CheckpointOutcomeRecorded`
row with `above_warn = false` sits between them, the WARN fires. **This ADR provides the
substrate (the taxonomy + emission contract); #617 implements the actual N=3 counter logic
and the `tracing::warn!` downgrade/promotion described in the ADR-091 amendment.** N stays
owned and tunable by lambda:khive per the amendment's text, unaffected by this ADR.

### 5. Retention/volume: edge-triggered for every variant except `ChannelPollStarted`

Per-tick unconditional emission is rejected by the numbers. At the checkpoint task's default
500ms cadence (`KHIVE_CHECKPOINT_INTERVAL_MS`, cited in the ADR-091 amendment,
`ADR-091-wal-snapshot-lifetime.md:410`), a per-tick `CheckpointOutcomeRecorded` row would be
`86400s / 0.5s = 172,800 rows/day`, on one process, for one telemetry mechanism alone. At the
channel-poll happy-path cadence (`HAPPY_PATH_INTERVAL = Duration::from_secs(5)`, `serve.rs:
296`), a per-tick row per registered channel is `86400s / 5s = 17,280 rows/day/channel` —
today's channel registry has one channel (`EmailChannel`, `serve.rs:112-114`), so 17,280/day,
but the same loop iterates `channels.iter()` per tick (`serve.rs:313`), so this scales
linearly with future channel count (#112-#115 track Telegram/WhatsApp additions). This is on
top of the `Audit` row every `comm.heartbeat` dispatch already writes today (also 17,280/
day/channel, unrelated to this ADR, per the "already produces one `EventKind::Audit` row
today" fact in Context above) — i.e., unconditional per-tick lifecycle events would roughly
double the events-table growth rate from this one loop alone, before counting checkpoint
volume.

The recon confirms the existing tracing already follows an edge-triggered discipline where
it matters (`log_eligible_poll_failure`'s doc comment, `serve.rs:452-454`: "warn! only on an
escalation edge... debug! on a repeat at the same step"; the ADR-091 `crossing_warn` gate,
`checkpoint.rs:277-294`). This ADR keeps that discipline for the DB-backed events: every new
variant except `ChannelPollStarted` fires only on a state transition (backoff armed/reset,
poll outcome _change_ from the prior tick — not carried for `ChannelPollSucceeded`/`Failed`
today, see the open question below — checkpoint threshold crossing, config first-read,
heartbeat-persist failure). `ChannelPollStarted` is the one unconditional variant, retained
because it is the anchor a sequencing query needs to establish "a tick happened even though
nothing else fired" (distinguishing "channel healthy, no events" from "loop stalled, no
events"); it is cheap (`Event` rows here carry no payload beyond `channel_kind`/`slug`) and
bounded by the same 17,280/day/channel arithmetic above, which this ADR accepts as the floor
cost of sequencing-assertability for this loop. Steady-state healthy-channel volume with this
design is therefore ~17,280 `ChannelPollStarted` rows/day/channel plus near-zero edge-
triggered rows; an active incident (auth failure, backoff escalating and de-escalating)
adds single-digit-to-low-tens of edge-triggered rows for the incident's duration, not
thousands.

Prune policy: this ADR does **not** introduce a prune mechanism for the `events` table. No
generic events-table retention policy exists today (`memory.prune`,
`crates/khive-pack-memory/src/handlers/prune.rs`, is scoped to the `memory` note kind only);
event-log retention is an open question already recorded in two prior ADRs —
`docs/adr/ADR-032-brain-profile-orchestration.md:1252-1254` ("Event log retention vs replay
fidelity... tentative: time-tiered retention") and `docs/adr/ADR-041-event-provenance-
projection.md:601-604` (observation TTL tied to the same open question). Per ADR-093's
precedent (`docs/adr/ADR-093-sessions-raw-zstd-compression.md`, "Measured compression"
section) of measuring actual growth before choosing a mechanism (the sessions.db lesson),
this ADR defers a specific prune threshold until the added variants have run in production
long enough to measure real row counts, and explicitly does not treat "the events table will
grow" as a blocker to shipping bounded, edge-triggered instrumentation now. The volume this
ADR adds is a small, known-bounded increment on top of the existing (already unaddressed)
Audit-per-dispatch growth; it does not make the open retention question qualitatively worse.

### 6. #599 disposition: not closed by this ADR

Once lifecycle events are durable in the `events` table, the _evidence_ half of #599's
concern (losing all record of daemon activity when stdout/stderr go to `/dev/null`) is
addressed for the lifecycle-event coverage this ADR adds — matching the precedent #615 (per
the issue's comment thread) already set for `comm.heartbeat` specifically. This ADR does
**not** close #599. The human-readable half of #599's ask — a default rotating log file
honoring `KHIVE_LOG` at the `spawn_daemon()` call site (`crates/khive-mcp/src/daemon.rs:250-
252`) — is untouched by this ADR; an operator debugging by reading logs (not querying the
`events` table) still gets nothing under the default deployment shape. #599 stays open.

### 7. Singleton-audit folds

(a) **Env-cached `OnceLock` config values** (`crates/khive-pack-memory/src/handlers/
common.rs:40,45`; `crates/khive-pack-kg/src/handlers/context.rs:35`) get a `ConfigLocked`
event appended at first read, announcing the config key and the value locked in for the
process's lifetime. This directly closes the singleton audit's top-ranked finding
(`SINGLETON_AUDIT.md` §4 item 1, "Medium" severity, "Disposition: fold into telemetry ADR"):
today, changing one of these env vars against a running daemon silently does nothing until
restart, with no log line announcing the locked-in value; a `ConfigLocked` row makes that
value observable and queryable without requiring the operator to have been watching stdout
at the exact moment the daemon booted.

(b) **`tx_registry.rs` (ADR-091 Plank 0)** stays out of scope for this ADR, per the audit's
own framing (`SINGLETON_AUDIT.md` §4 item 2): it is a per-process, in-memory, observe-only
registry (`crates/khive-storage/src/tx_registry.rs:1-7`), correctly documented as such and
already correctly excluded from cross-process claims. It **may** be sampled into a future
`EventKind` (e.g., "oldest open transaction at checkpoint time") as an input, but this ADR
does not spec that — it would require deciding how a per-process snapshot becomes a durable
row, which is a separate design question from the lifecycle taxonomy this ADR adds.

(c) **`ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS` scoping nit**
(`crates/khive-pack-knowledge/src/knowledge/vamana.rs:170-171`) is accepted with a comment,
not fixed by this ADR — it is functionally inert in production (the static is unconditionally
compiled in, but only test code writes it, so it always reads 0 and production always falls
through to the real `ANN_WARM_WAIT_TIMEOUT_MS` constant). Listed here per Decision 8's non-
goals, not actioned.

### 8. Non-goals

- **No new storage substrate.** The `events` table, `Event` struct, and `EventStore` trait
  are unchanged except for the new `EventKind` variants (an additive Rust enum change) and
  the one new `VerbRegistry::event_store()` accessor (Decision 2). No new migration, no new
  table, no schema change to `events-ddl.sql`.
- **No new wire-surface verb.** A read verb over lifecycle events (e.g., something like
  `comm.health-for-events`, surfacing a channel's recent transition sequence the way
  `comm.health()` surfaces its current state) is plausible future work, but this ADR does
  not spec it. Emission (Decision 2) goes through the storage-level `EventStore` trait
  directly, not through `registry.dispatch()`.
- **No #599 log-file remedy.** See Decision 6 — #599 stays open.
- **No tracing removal.** Existing `tracing::{info,warn,error,debug}!` call sites in
  `serve.rs` and `checkpoint.rs` are unchanged; they remain the human-readable channel
  (useful whenever logs are actually captured) alongside the new durable events.
- **No fix for the `ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS` scoping nit** (Decision 7c) — accepted
  as-is.
- **No events-table prune/retention mechanism** (Decision 5) — deferred pending measurement,
  consistent with the open questions already recorded in ADR-032 and ADR-041.

## Failure modes

- **`append_event` fails inside `channel_poll_loop` or `run_checkpoint_task`.** Best-effort:
  `tracing::warn!` and continue (Decision 2), matching #606's precedent. The poll/checkpoint
  loop's primary job (polling channels, checking WAL pages) is never blocked by a telemetry
  write failure.
- **`registry.event_store()` returns `None`** (no `EventStore` configured, e.g. a test
  harness that builds a bare `VerbRegistry`). Every new call site treats this as a no-op
  (mirrors `dispatch`'s own `if let Some(store) = &self.event_store` gate, `pack.rs:922`) —
  lifecycle telemetry is best-effort-present, not a hard dependency for the poll loop to
  function.
- **A future channel addition (#113 Telegram, #114 email already shipped, #115 WhatsApp)
  multiplies `ChannelPollStarted` volume linearly.** Acknowledged in Decision 5's arithmetic;
  not a correctness risk, a volume-planning input for the deferred prune decision.
- **`ConfigLocked`'s "first read" is itself a race across concurrent early requests on daemon
  boot** (multiple callers could each observe the `OnceLock` as freshly-initialized-by-them
  in theory) — in practice `OnceLock::get_or_init` guarantees exactly one initializer runs,
  so the emission site is naturally single-writer; a duplicate `ConfigLocked` row is not
  possible by construction, only by a bug in an emission site placed outside the `get_or_
  init` closure. Implementation must place the `append_event` call inside the closure that
  runs exactly once, not after `get_or_init` returns.

## Consequences

- New workspace-additive `EventKind` variants (Decision 1): `ChannelPollStarted`,
  `ChannelPollSucceeded`, `ChannelPollFailed`, `ChannelBackoffArmed`, `ChannelBackoffReset`,
  `ChannelHeartbeatPersistFailed`, `ConfigLocked`, `CheckpointOutcomeRecorded` — the closed
  enum grows from 26 to 34 variants. The `EventKind::ALL` round-trip test
  (`event.rs:822-824`) must be extended to cover each new variant, per the existing pattern.
- One new public method, `VerbRegistry::event_store(&self) -> Option<Arc<dyn EventStore>>`
  (Decision 2), the only API-surface addition; no wire-surface (MCP verb) change.
- `channel_poll_loop` and (for #617) `run_checkpoint_task` each gain a handful of best-effort
  `append_event` calls at existing decision points; no new decision logic is added by this
  ADR beyond what `channel_error_class`, `is_backoff_eligible`, and `crossing_warn` already
  compute today — this ADR only makes their outcomes durable and ordered.
- `events` table growth increases by roughly 17,280 rows/day/channel (steady state, one
  unconditional variant) plus incident-bounded edge-triggered rows, on top of the existing,
  already-unaddressed per-dispatch `Audit` growth (Decision 5). No prune mechanism ships with
  this ADR; the open retention question (ADR-032, ADR-041) is unchanged in kind, only in
  degree.
- The three duplicated `CaptureSubscriber` tracing-capture test harnesses
  (`serve.rs`, `checkpoint.rs`, `pack.rs`) become retirable once the corresponding
  `EventKind` variants land and their consumer tests are rewritten against persisted state
  (Decision 3); this ADR does not delete them (implementation-time work, out of scope for
  this docs-only ADR).
- #599 (daemon stderr loss) and #617 (#617's own N=3 counter implementation) both remain
  open, tracked separately; this ADR is the shared substrate both build on.

## Alternatives considered

- **Instrument only `VerbRegistry::dispatch`, richer `Audit` payloads.** Rejected: the
  channel-poll-loop's internal transitions (auth failed, backoff armed) are not verb
  dispatches — they happen between `registry.dispatch("comm.heartbeat", ...)` calls inside
  the loop body. No amount of enriching the existing `Audit` event's payload captures a
  transition that never goes through `dispatch()` at all.
- **A new wire-surface verb (e.g. `comm.append_lifecycle_event`) instead of a direct
  `EventStore::append_event` call.** Rejected for this ADR: adds a new MCP-visible verb for
  a purely internal instrumentation concern, and would itself recurse into `dispatch()`,
  producing a second `Audit` row for every lifecycle row (doubling volume beyond the
  Decision 5 numbers) for no benefit — the direct `EventStore` trait call is simpler and
  already the mechanism `dispatch` itself uses internally.
- **Open `kind: String` instead of a closed enum.** Rejected: the SQL column is already
  untyped `TEXT` with no `CHECK` constraint, so an open string would silently accept typos
  with no compile-time or `EventKind::ALL` round-trip protection; the closed-enum-plus-
  additive-migration pattern the codebase already uses for `EventKind` is preserved.
- **Unconditional per-tick emission for every variant** (simplest to implement). Rejected
  on the volume arithmetic in Decision 5 (172,800 rows/day/process for checkpoint alone at
  500ms cadence) — the sessions.db lesson (ADR-093) is that unmeasured, unbounded per-tick
  accumulation is exactly the growth pattern worth avoiding by design, not discovering after
  the fact.
- **Extend the `comm.heartbeat` note itself into an append-only history (drop the
  `Uuid::new_v5` upsert key).** Rejected: `comm.health()`'s current-state contract
  (`handlers.rs:1060-1075`) depends on the one-row-per-channel invariant; changing it to an
  append-only shape would require a new read-side aggregation query for every existing
  `comm.health()` caller and duplicates the `events` table's ordering guarantees that already
  exist elsewhere in the schema.

## Test list (informative, implementation-time)

- `channel_poll_loop` emits `ChannelPollStarted` exactly once per tick per registered
  channel, regardless of poll outcome.
- A poll success after a prior failure emits `ChannelBackoffReset` when backoff was
  previously armed, and does not emit it when backoff was already at zero.
- `log_eligible_poll_failure`'s escalation edge (`tick.should_warn`) emits exactly one
  `ChannelBackoffArmed` row; a same-step repeat emits zero (mirrors the existing `debug!`-
  not-`warn!` discipline, `serve.rs:462-482`).
- `record_channel_heartbeat`'s dispatch-failure path emits `ChannelHeartbeatPersistFailed`
  and does not panic or abort the poll loop.
- `ConfigLocked` fires exactly once per config key across concurrent early readers (race
  test analogous to the existing `OnceLock` staleness tests, if any exist for
  `common.rs:40,45` / `context.rs:35`).
- A 3-consecutive-cycle `CheckpointOutcomeRecorded` sequence with `above_warn = true` in
  every row is queryable as the #617 WARN condition; a sequence with a `false` row in the
  middle is not.
- Sequencing assertion: for a synthetic multi-transition scenario (poll started, auth
  failed, backoff armed, poll started, poll succeeded, backoff reset), the ordered
  `(created_at, id)` query returns exactly that sequence.
