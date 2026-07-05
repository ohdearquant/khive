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
   (`comm.heartbeat`); #599 stays open because the general stderr-loss problem is
   unresolved.

2. **The `comm.heartbeat` note (#606)**, `crates/khive-pack-comm/src/handlers.rs:900-1030`
   (function `handle_heartbeat` at `handlers.rs:925`, id computed by `heartbeat_note_id` at
   `handlers.rs:900-909` via a deterministic `Uuid::new_v5` on `(namespace, channel_kind,
   channel_slug)`, so repeated calls **update the same row**). `created_at` is preserved as
   first-seen time across updates (`handlers.rs:916-917`), not bumped per call. It tracks
   `consecutive_failures` (reset on success, incremented on failure, `handlers.rs:996-1013`)
   and retains `last_error` across a subsequent success: real sequence information, but only
   a scalar counter and a "last known bad thing," not the ordered transition sequence itself.
   Durable and queryable via `comm.health()` (`handlers.rs:1060-1075`); answers "what is the
   state right now," not "in what order did the last N transitions happen."

3. **The append-only `events` table**, `crates/khive-storage/src/event.rs:15-32` (struct
   `Event`, doc comment: "Storage-level event record. Every verb execution produces one.
   Immutable once appended.") backed by `crates/khive-db/sql/events-ddl.sql:4-21`, with
   `idx_events_ns_created_id` on `(namespace, created_at DESC, id DESC)`
   (`events-ddl.sql:37`) and `idx_events_session` on `(namespace, session_id, created_at,
   id)` (`events-ddl.sql:38`). This is genuinely ordered, durable, and cross-process
   (SQLite-backed), unlike (1) and (2). It is already wired into the daemon's default
   construction: `VerbRegistry::dispatch` (`crates/khive-runtime/src/pack.rs:878-951`)
   appends one `EventKind::Audit` row per verb call whenever `self.event_store` is
   `Some(..)` (`pack.rs:922`), and both production server-construction paths set that up
   unconditionally when authorization succeeds: `crates/khive-mcp/src/server.rs:293-296`
   and `crates/khive-mcp/src/serve.rs:917-920` (see "Recon citation corrected" below). Since
   `record_channel_heartbeat` (`serve.rs:419-450`) persists via `registry.dispatch("comm.
   heartbeat", ...)` (`serve.rs:444`), **every channel-poll tick already produces one
   `EventKind::Audit` row today**, on the daemon, independent of this ADR. Its payload is
   the gate's `AuditEvent` (allow/deny decision, actor, verb name, `pack.rs:914-940`), not
   the richer `HeartbeatOutcome` the poll loop itself computed; that richer payload lives
   only inside the heartbeat note (mechanism 2), not inside the `Audit` event row. Why this
   existing row cannot serve as the sequencing anchor is addressed in Alternatives.

`EventKind` (`crates/khive-types/src/event.rs:69-122`) is a closed Rust enum, 26 variants
(`EventKind::ALL`, `event.rs:126-153`; iterated by the round-trip test at `event.rs:822-830`
that resolved the earlier "verified via `EventKind::ALL` at `event.rs:823`" recon note: that
line is a use of `ALL` inside the test loop, not its declaration, which is at `event.rs:126`).
None of the 26 names a channel-poll-lifecycle or checkpoint-cycle transition. The SQL side
stores `kind` as a plain `TEXT` column with no `CHECK` constraint (`events-ddl.sql:10`): the
closedness lives entirely in the Rust type, not the schema.

**Conclusion (load-bearing for this ADR, matching the recon): the ordered, durable
substrate this ADR needs already exists and is already live on the daemon.** The gap is
coverage, not storage: `channel_poll_loop`'s internal decisions (`is_backoff_eligible`,
`log_eligible_poll_failure`, the escalation/de-escalation of backoff state) happen between
`registry.dispatch(...)` calls, inside the loop body (`crates/khive-mcp/src/serve.rs:283-
374`), so no amount of instrumenting `dispatch()` alone captures them. This ADR adds new
`EventKind` variants and explicit `append_event` calls from inside `channel_poll_loop` and,
for #617, from inside `run_checkpoint_task`, reusing the existing `EventStore` capability.
It does not add a new storage substrate.

### Recon citation corrected

The recon (`TELEMETRY_SURFACE.md` §3(b)) stated the `with_event_store` wiring happens at
"`server.rs:294-295` and `serve.rs:758-759` and `serve.rs:1147-1148`", three sites. Re-
verified against this worktree (byte-identical to the audited SHA, confirmed via `diff
<(git show origin/main:crates/khive-mcp/src/serve.rs) crates/khive-mcp/src/serve.rs`,
zero-line diff): `serve.rs:758` and `serve.rs:1147` are unrelated functions
(`build_registry_for_multi_backend`'s doc comment and `build_server_multi_backend`
respectively). There is exactly **one** `with_event_store` call site in `serve.rs`, at
`serve.rs:919` (inside the block `serve.rs:917-920`, guarded the same way as `server.rs:293-
296`: `authorize(Namespace::local())` then `events(&tok)`). A workspace-wide `grep -rn
with_event_store crates/` confirms exactly two non-test production call sites
(`server.rs:295`, `serve.rs:919`) plus test-only call sites in `pack.rs`,
`khive-pack-kg/tests/integration.rs`, and `khive-pack-knowledge/tests/fixes.rs`. The
substantive claim ("this is wired for the production daemon/server") holds; the specific
second citation was one call site, not two.

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
are declared, `event.rs:69-122`), not an open `kind: String` free-form field. The SQL column
is already untyped `TEXT` (`events-ddl.sql:10`), so this is purely a Rust-side addition; no
migration is needed to add variants, only to add rows using them. Rationale: an open string
kind would let a typo silently create a new, unindexed telemetry category that no query or
dashboard is written against; the existing `EventKind::ALL` + `FromStr`/`Display` round-trip
test (`event.rs:822-830`) is the mechanism that keeps every consumer (a query helper, a
health check) honest about the full set. New variants:

- `ChannelPollStarted`: start of one `channel_poll_loop` tick for one `(kind, slug)`.
  Emitted once per tick per channel, **not** conditional on outcome. See Decision 5 for why
  this is the one unconditional (non-edge-triggered) variant in the taxonomy.
- `ChannelPollSucceeded`: emitted only as a **recovery edge**, when the `Ok(envelopes)`
  branch (`serve.rs:316`) runs for a channel whose backoff state currently shows at least
  one recorded failure. Concretely: `backoffs.get(&backoff_key).map(|b| b.attempt() >
  0).unwrap_or(false)`, checked **before** `backoff.record_success()` resets it
  (`serve.rs:317-318`; `ImapBackoff::attempt()`, `crates/khive-channel-email/src/
  backoff.rs:81`). This reuses existing per-credential backoff state; no new state is added
  for this variant. A routine success on an already-healthy channel (no entry in `backoffs`,
  or an entry with `attempt() == 0`) emits nothing. Payload carries `envelope_count`.
- `ChannelPollFailed`: emitted on the poll's `Err(e)` branch (`serve.rs:346`) when either (a)
  this is the first failure recorded for this `(kind, slug)` since the last success, or (b)
  `channel_error_class(&e)` (`serve.rs:394-402`) differs from the class of the last emitted
  `ChannelPollFailed` for this `(kind, slug)`. This requires one small new piece of
  in-process state, `last_error_class: HashMap<(String, String), &'static str>`, added
  alongside the existing `backoffs` map in `channel_poll_loop`. The entry for a `(kind,
  slug)` key is cleared on **every** successful poll, i.e. inside the `Ok(envelopes)` branch
  itself (`serve.rs:316`), unconditionally and independent of whether that same tick also
  emits `ChannelPollSucceeded`. Clearing cannot be tied to the `ChannelPollSucceeded` event,
  because that event only fires when `ImapBackoff::attempt() > 0`, and non-backoff-eligible
  `"config"`-class failures never populate `backoffs` in the first place: a config-class
  failure followed by a successful poll would leave `attempt() == 0` throughout, so
  `ChannelPollSucceeded` never fires, and if clearing depended on it, the stale `"config"`
  entry in `last_error_class` would survive indefinitely, wrongly suppressing the next,
  separate config-failure episode of the same class as "not a change." Clearing on the
  `Ok(envelopes)` branch itself avoids this: it runs on every success regardless of backoff
  eligibility, so a subsequent failure after any recovery, backoff-eligible or not, is
  correctly treated as "first failure" again. This state is independent of `backoffs`
  because `ChannelPollFailed` fires on every failure class, including the non-backoff-eligible
  `"config"` class, while `backoffs` only tracks backoff-eligible failures. Payload carries
  `error_class` and the error message.
- `ChannelBackoffArmed`: fired only on a `log_eligible_poll_failure` escalation edge
  (`serve.rs:462-482`, `tick.should_warn`), payload carries the new backoff step and delay.
  This directly supersedes the WARN-vs-DEBUG decision that today only exists as a tracing
  level (see Decision 3).
- `ChannelBackoffReset`: fired when `backoff.record_success()` (`serve.rs:317-318`) clears a
  previously-armed backoff state (i.e., the channel had a nonzero backoff step and this tick
  reset it to zero). Not fired on an already-healthy channel's routine success. Uses the same
  existing `ImapBackoff::attempt()` state as `ChannelPollSucceeded`; the two variants are
  emitted together on the same recovery tick when applicable, but are logically distinct
  (one is "polling is healthy again," the other is "the backoff delay is cleared").
- `ChannelHeartbeatPersistFailed`: the `record_channel_heartbeat` best-effort-write-failed
  path (`serve.rs:445-448`). Today this failure is only visible as a discarded `tracing::
  warn!` line under #599's default deployment shape; this variant makes "the durable
  heartbeat write itself failed" durably observable without requiring stdout capture.
- `ConfigLocked`: fired once per config key, at first read, for each `OnceLock`-cached env
  config value identified by the singleton audit (`crates/khive-pack-memory/src/handlers/
  common.rs:40` `recall_profile_enabled`, `:45` `ann_overfetch_max_rounds`;
  `crates/khive-pack-kg/src/handlers/context.rs:35` `context_profile_enabled`). Payload
  carries the config key and the locked-in value. The emission mechanism (closure enqueue,
  async drain) is specified in Decision 2 and Decision 7(a).
- `CheckpointOutcomeRecorded`: one row per checkpoint cycle while `wal_pages >=
  config.warn_pages` (`checkpoint.rs:277`, `above_warn`), plus exactly one additional row on
  the tick where `wal_pages` drains back below `warn_pages` (the recovery/drain edge). See
  Decision 4 for why both halves are required and how #617's WARN query reads them.

Non-goals for this variant set are listed in Decision 8; in particular, boot consolidation
(`build_registry_for_multi_backend`) already fails loud via `anyhow::bail!` on error
(recon §1, "Tracing sites NOT in scope") rather than logging, so it needs no new
`EventKind`: a non-zero process exit is already an unambiguous signal.

### 2. Emission contract: best-effort, in-process, direct `append_event`, not a new verb

`channel_poll_loop` already holds a `khive_runtime::VerbRegistry` by value (`serve.rs:283-
287`, parameter `registry`), the same type whose `dispatch()` already persists `Audit`
events (`pack.rs:922`), but `VerbRegistry`'s `event_store` field is private
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

`channel_poll_loop` calls `registry.event_store()` once per tick (cheap `Arc` clone or
`None`) and, at each lifecycle site, constructs an `Event` via `Event::new(namespace,
"channel_poll_loop", EventKind::ChannelPollFailed, SubstrateKind::Event, actor)` (mirroring
`Event::new`'s signature, `event.rs:35-42`, and the existing construction at
`pack.rs:932-940`) and calls `store.append_event(event).await`. This does **not** go through
`registry.dispatch()` and does **not** add a new wire-surface verb (per Decision 8's
non-goal): it calls the storage-level `EventStore::append_event` trait method directly
(`crates/khive-storage/src/event.rs:190`), the same trait `dispatch` itself uses.

**The checkpoint side needs a different injection boundary, not `VerbRegistry`.**
`run_checkpoint_task` is a free function defined in `khive-db`
(`pub async fn run_checkpoint_task(pool: Arc<ConnectionPool>, config: CheckpointConfig)`,
`crates/khive-db/src/checkpoint.rs:253`) and is spawned from `khive-runtime`'s daemon-startup
path (`tokio::spawn(run_checkpoint_task(pool, cfg))`, `crates/khive-runtime/src/
daemon.rs:560`, imported via `use khive_db::{run_checkpoint_task, ...}` at `daemon.rs:27`).
It never sees a `VerbRegistry`, and the dependency chain runs one direction only
(`khive-storage -> khive-db -> khive-runtime`), so `khive-db` cannot depend on
`khive-runtime` to reach `VerbRegistry::event_store()`. It _can_, however, reach the
`EventStore` trait directly, because `khive-db` already depends on `khive-storage`
(`crates/khive-db/Cargo.toml`, `khive-storage = { path = "../khive-storage" }`) and the trait
is declared there (`crates/khive-storage/src/event.rs:188`, re-exported at
`khive-storage/src/lib.rs:20-22`).

The injection shape this ADR specifies:

- `run_checkpoint_task`'s signature grows two parameters: `pub async fn run_checkpoint_task
  (pool: Arc<ConnectionPool>, config: CheckpointConfig, event_store: Option<Arc<dyn
  EventStore>>, namespace: String)`. `khive-db` gains no new crate dependency; `EventStore`
  is already reachable through `khive-storage`.
- The `DaemonDispatch` trait (`crates/khive-runtime/src/daemon.rs:252`) gains one more
  default-`None` accessor, mirroring the existing `pool_for_checkpoint` pattern
  (`daemon.rs:281-290`):

  ```rust
  /// Return the event sink to thread into the checkpoint task, if any.
  ///
  /// The default implementation returns `None`, matching `pool_for_checkpoint`'s pattern:
  /// implementors backed by a configured `EventStore` should return `Some(store)`.
  fn event_store_for_checkpoint(&self) -> Option<Arc<dyn EventStore>> {
      None
  }
  ```

- `khive-mcp`'s `DaemonDispatch` implementor (`crates/khive-mcp/src/daemon.rs:66-106`, the
  same `impl` block that already provides `pool_for_checkpoint` at `daemon.rs:103-105` by
  returning `self.pool()`) implements this new method by returning `self.registry.
  event_store()` (using the Decision 2 accessor above; `KhiveMcpServer` already holds a
  `registry: VerbRegistry` field, `crates/khive-mcp/src/server.rs:186`).
  (The same one-directional-dependency reasoning applies to Decision 7a's `ConfigLocked`
  drain below: the drain site must live in a crate that is both reachable from the
  `OnceLock` producer sites and does not require `khive-db` to depend on `khive-runtime`.
  `run_checkpoint_task` is ruled out for the same reason it needed the parameters above;
  Decision 7a places the drain in `VerbRegistry::dispatch` instead, which already lives in
  `khive-runtime` alongside the producers' existing dependency on that crate.)
- The spawn site (`crates/khive-runtime/src/daemon.rs:558-561`) becomes:

  ```rust
  if let Some(pool) = dispatcher.pool_for_checkpoint() {
      let cfg = CheckpointConfig::from_env();
      let event_store = dispatcher.event_store_for_checkpoint();
      let namespace = dispatcher.namespace().to_string();
      tokio::spawn(run_checkpoint_task(pool, cfg, event_store, namespace));
      tracing::info!("WAL checkpoint task started");
  }
  ```

  `dispatcher.namespace()` already exists on the trait (`daemon.rs:273`) and is reused
  rather than adding a third parameter source.

This keeps the dependency direction intact (no `khive-db -> khive-runtime` edge is added)
and follows the same "default `None`, override where a concrete sink exists" pattern already
established by `pool_for_checkpoint`.

Sync-inline vs. best-effort: **best-effort, matching the #606 precedent**, for every emission
site in both loops. `record_channel_heartbeat`'s own failure path (`serve.rs:445-448`)
already establishes that a durable-write failure on the poll loop is logged and swallowed,
never propagated to interrupt polling; the loop's job is to keep polling channels (or
checkpointing WAL pages), not to guarantee telemetry durability. The same `if let Err(e) =
store.append_event(...).await { tracing::warn!(...); }` shape applies to every new call site
in this ADR: on an append failure, `tracing::warn!` (which, per #599, may itself be
discarded under the default deployment shape, an acknowledged, not hidden, limitation) and
continue the loop.

Namespace: `channel_poll_loop` events append into `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE`
(the same constant `record_channel_heartbeat` already uses at `serve.rs:427`), not
`ingest_namespace`, so channel-lifecycle events and channel-health notes live in the same
namespace and a single query can join or filter across both. `run_checkpoint_task`'s events
append into the `namespace` parameter threaded from `dispatcher.namespace()` above (the
dispatcher's configured default namespace; the checkpoint task is a per-process, not
per-channel, concern, so there is no channel-health-specific namespace to prefer).

### 3. Ordering and sequencing-assertability: retire the duplicated `CaptureSubscriber` harnesses

A test asserts sequencing by querying `events` filtered by `kind IN (...)` and a `session_id`
or a synthetic per-test namespace, ordered by `(created_at, id)`, the exact shape
`idx_events_ns_created_id` and `idx_events_session` already support (`events-ddl.sql:37-38`).
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
`crates/khive-runtime/src/pack.rs:2554-2577`, gate-dispatch tests; see "Recon citation
corrected" above for the third). Once `log_eligible_poll_failure`'s escalation decision is
itself an appended `ChannelBackoffArmed` row and the WAL WARN-ladder decision (#617) is
built on appended `CheckpointOutcomeRecorded` rows, both `serve.rs`'s and `checkpoint.rs`'s
tests can assert on persisted state via one shared query helper instead of maintaining
independent `Subscriber` implementations. This ADR does not remove the existing
`tracing::warn!`/`debug!` calls (human-readable log lines remain useful when logs are
actually captured); it adds the persisted alternative and specifies that **new** tests for
this behavior should assert on the persisted row, not a captured tracing event. Existing
`CaptureSubscriber` tests are not deleted by this ADR (that is an implementation-time
cleanup, out of scope for a docs-only ADR), but they become removable once the corresponding
`EventKind` lands.

### 4. #617 consumer: windowed query over `CheckpointOutcomeRecorded`, this ADR provides the substrate only

The Blocker in round-1 review of this ADR was that emitting `CheckpointOutcomeRecorded` only
while `above_warn` is true makes the N=3 query vacuous: no row ever exists with `above_warn =
false`, so three isolated single-cycle crossings spread across a week (each cycle elevated
for exactly one tick, then draining back down before the next crossing days later) would
each independently satisfy "the last 3 rows are all `above_warn = true`" once enough isolated
crossings had accumulated, with no way to tell them apart from one genuinely sustained
3-cycle elevation.

The fix (Decision 1's `CheckpointOutcomeRecorded` bullet): emit a row on **every** tick where
`above_warn` is true (unchanged from the original proposal, and required because ADR-091's
condition, `docs/adr/ADR-091-wal-snapshot-lifetime.md:412-414`, is defined per checkpoint
_cycle_, not per crossing), **plus exactly one additional row on the tick where `above_warn`
transitions from true to false** (the drain/recovery edge), with `above_warn = false` in that
row's payload. This is cheap: it adds at most one row per elevation episode, not one row per
below-threshold tick, and reuses the existing `was_above_warn` bool `run_checkpoint_task`
already threads through the loop (`checkpoint.rs:256`) rather than adding new state:

```rust
let was_elevated = was_above_warn; // capture before crossing_warn mutates it
let warn_crossed = crossing_warn(above_warn, &mut was_above_warn);
if above_warn {
    append_event(CheckpointOutcomeRecorded { above_warn: true, wal_pages, .. });
} else if was_elevated {
    append_event(CheckpointOutcomeRecorded { above_warn: false, wal_pages, .. });
}
```

This defeats exactly the false-positive scenario above: each isolated single-cycle crossing
now produces the pair `[above_warn=true, above_warn=false]` (rise, then drain on the very
next tick), so the three isolated crossings across a week produce the sequence `... true,
false, [gap], true, false, [gap], true, false`. The 3 most-recent rows at any point in that
sequence always include at least one `false` row, so "last 3 rows all `above_warn = true`"
never fires. A genuine 3-consecutive-cycle sustained elevation, by contrast, produces `true,
true, true` with no interposed `false` row, because the drain row is only emitted on the
transition back below threshold, which by definition has not yet happened while still
elevated.

#617's N=3-consecutive-cycle WARN detection reads this as a windowed query: fetch the 3 most
recent `CheckpointOutcomeRecorded` rows for the process's namespace, ordered by `(created_at,
id) DESC`; if fewer than 3 rows exist (e.g. shortly after daemon start), do not fire; if all
3 have `payload.above_warn = true`, the WARN fires. **This ADR provides the substrate (the
taxonomy, the emission contract, and the drain-row addition that makes the query
non-vacuous); #617 implements the actual query, the `tracing::warn!` downgrade/promotion
described in the ADR-091 amendment, and any additional hysteresis #617's own design wants on
top of this minimum.** N stays owned and tunable by lambda:khive per the amendment's text,
unaffected by this ADR.

### 5. Retention/volume: edge-triggered for every variant except `ChannelPollStarted`

Per-tick unconditional emission (one row on every tick regardless of state) is rejected by
the numbers. At the checkpoint task's default 500ms cadence (`KHIVE_CHECKPOINT_INTERVAL_MS`,
cited in the ADR-091 amendment, `ADR-091-wal-snapshot-lifetime.md:410`), an unconditional
per-tick `CheckpointOutcomeRecorded` row would be `86400s / 0.5s = 172,800 rows/day`, on one
process, for one telemetry mechanism alone, at all times, including when WAL pressure is
healthy. That baseline is rejected; the design in Decision 4 above instead emits zero rows
while healthy and rows only while (or on the edge of leaving) an elevated state.

Under that design, the worst case is a sustained elevation: one `CheckpointOutcomeRecorded`
row per 500ms tick for as long as `wal_pages` stays at or above `warn_pages`, i.e. `3600s /
0.5s = 7,200 rows/hour` while continuously elevated. This is bounded in practice, not
indefinite: ADR-091 Plank 2's TRUNCATE escalation (`checkpoint.rs:428-506`, armed once
`wal_pages` crosses the much higher `truncate_high_water_pages`, default 20000) actively
reduces `wal_pages` once pressure is severe enough to reach it, and ordinary write bursts
that only cross `warn_pages` (default 2000) are, per the ADR-091 amendment's own framing,
"expected, self-resolving event[s]" rather than sustained-for-hours conditions. A healthy
process emits 0 `CheckpointOutcomeRecorded` rows/day.

At the channel-poll happy-path cadence (`HAPPY_PATH_INTERVAL = Duration::from_secs(5)`,
`serve.rs:296`), the one unconditional variant, `ChannelPollStarted`, is `86400s / 5s =
17,280 rows/day/channel`, today's channel registry has one channel (`EmailChannel`,
`serve.rs:111-114`), so 17,280/day, but the same loop iterates `channels.iter()` per tick
(`serve.rs:313`), so this scales linearly with future channel count (#112-#115 track
Telegram/WhatsApp additions). This is on top of the `Audit` row every `comm.heartbeat`
dispatch already writes today (also 17,280/day/channel, unrelated to this ADR, per the
"already produces one `EventKind::Audit` row today" fact in Context above); i.e.,
`ChannelPollStarted` alone roughly doubles the events-table growth rate attributable to this
one loop, before counting checkpoint volume. Why the existing `Audit` row cannot substitute
for `ChannelPollStarted` as the sequencing anchor (avoiding this doubling) is addressed in
Alternatives.

Every other new variant (`ChannelPollSucceeded`, `ChannelPollFailed`, `ChannelBackoffArmed`,
`ChannelBackoffReset`, `ChannelHeartbeatPersistFailed`, `ConfigLocked`) fires only on a state
transition, matching the existing tracing discipline where it already exists
(`log_eligible_poll_failure`'s doc comment, `serve.rs:452-454`: "warn! only on an escalation
edge... debug! on a repeat at the same step"; the ADR-091 `crossing_warn` gate,
`checkpoint.rs:277-294`). Steady-state healthy-channel volume with this design is therefore
~17,280 `ChannelPollStarted` rows/day/channel plus near-zero edge-triggered rows; an active
incident (auth failure, backoff escalating and de-escalating, recovering) adds
single-digit-to-low-tens of edge-triggered rows for the incident's duration, not thousands.
`ConfigLocked` fires at most once per config key per process lifetime (bounded by the number
of `OnceLock` sites the singleton audit identified: 3 today).

Prune policy: this ADR does **not** introduce a prune mechanism for the `events` table. No
generic events-table retention policy exists today (`memory.prune`,
`crates/khive-pack-memory/src/handlers/prune.rs`, is scoped to the `memory` note kind only);
event-log retention is an open question already recorded in two prior ADRs:
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

Once lifecycle events are durable in the `events` table, the evidence half of #599's concern
(losing all record of daemon activity when stdout/stderr go to `/dev/null`) is addressed for
the lifecycle-event coverage this ADR adds, matching the precedent #615 (per the issue's
comment thread) already set for `comm.heartbeat` specifically. This ADR does **not** close
#599. The human-readable half of #599's ask, a default rotating log file honoring
`KHIVE_LOG` at the `spawn_daemon()` call site (`crates/khive-mcp/src/daemon.rs:250-252`), is
untouched by this ADR; an operator debugging by reading logs (not querying the `events`
table) still gets nothing under the default deployment shape. #599 stays open.

### 7. Singleton-audit folds

(a) **Env-cached `OnceLock` config values** (`crates/khive-pack-memory/src/handlers/
common.rs:40,45`; `crates/khive-pack-kg/src/handlers/context.rs:35`) get a `ConfigLocked`
event, closing the singleton audit's top-ranked finding (`SINGLETON_AUDIT.md` §4 item 1,
"Medium" severity, "Disposition: fold into telemetry ADR"). Each of these sites is a sync
`OnceLock::get_or_init` closure in a pack-handler free function, with no `VerbRegistry` or
`EventStore` in scope and no ability to `.await` from inside the closure. The mechanism:

- The closure itself calls a new, cheap, synchronous function, `khive_runtime::
  config_ledger::record_config_locked(key: &'static str, value: impl Into<String>)`, which
  pushes `(key, value)` onto a process-wide pending-emission slot (a `std::sync::Mutex<Vec<
  (&'static str, String)>>` behind a `OnceLock`, living in `khive-runtime` since both
  `khive-pack-memory` and `khive-pack-kg` already depend on it) and sets a companion
  `static PENDING: AtomicBool` to `true` (`Ordering::Release`). Because `OnceLock::
  get_or_init` guarantees its closure runs exactly once per key across all callers, each
  config key is enqueued **exactly once** per process lifetime, this part of the story is
  unchanged from the original "fires exactly once" intent.
- **Drain site: `VerbRegistry::dispatch`, not `run_checkpoint_task`.** `run_checkpoint_task`
  lives in `khive-db` (`crates/khive-db/src/checkpoint.rs:253`), and Decision 2 above
  already establishes that `khive-db` cannot depend on `khive-runtime`, where this ledger
  lives, because the dependency chain runs one direction only
  (`khive-storage -> khive-db -> khive-runtime`). Draining from `run_checkpoint_task` would
  reintroduce the exact dependency-direction violation Decision 2 exists to avoid. Instead,
  the drain runs from `VerbRegistry::dispatch` (`crates/khive-runtime/src/pack.rs:878-951`),
  which is in the **same crate** as `config_ledger`, already holds `self.event_store`
  (`pack.rs:922`), and is guaranteed to run at least once shortly after any producer site
  fires: every `OnceLock` config read this ADR instruments (`common.rs:40,45`,
  `context.rs:35`) happens inside a pack-handler function, and pack handlers only run inside
  a `VerbRegistry::dispatch` call, so a dispatch has already happened by the time there is
  anything in the ledger to drain, and the very next dispatch (on any verb, from any caller)
  drains it. This is a stronger liveness guarantee than the checkpoint task offered:
  `run_checkpoint_task` only runs when `dispatcher.pool_for_checkpoint()` is `Some` (a
  file-backed pool), while `channel_poll_loop` never runs at all on a daemon with no
  channels configured; `dispatch` is the one call site guaranteed to be live on any daemon
  that could have produced a `ConfigLocked` entry in the first place.

  Immediately after the existing `if let Some(store) = &self.event_store` gate in `dispatch`
  (`pack.rs:922`), add a cheap pending-check fast path so the hot dispatch path pays
  near-zero cost on the (overwhelmingly common) case where the ledger is empty:

  ```rust
  if let Some(store) = &self.event_store {
      if config_ledger::PENDING.swap(false, Ordering::AcqRel) {
          for (key, value) in config_ledger::drain_config_locks() {
              let event = Event::new(
                  gate_req.namespace.as_str(),
                  "config_ledger",
                  EventKind::ConfigLocked,
                  SubstrateKind::Event,
                  format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
              )
              .with_payload(json!({ "key": key, "value": value }));
              if let Err(e) = store.append_event(event).await {
                  tracing::warn!(key, "failed to append ConfigLocked event: {e}");
              }
          }
      }
      // ... existing Audit event append (pack.rs:923-950), unchanged ...
  }
  ```

  The `AtomicBool::swap` is the fast path: a single relaxed-ish atomic read-and-clear per
  dispatch call when the ledger is empty (which is every dispatch call after at most a
  handful of early ones per process lifetime, since each config key is enqueued at most
  once). Only when the flag was `true` does dispatch touch the `Mutex` at all.
- The exactly-once guarantee moves from "the row is written exactly once" (not achievable
  synchronously from inside a sync `OnceLock` closure) to a two-part story: the closure
  enqueues exactly once (guaranteed by `OnceLock::get_or_init`); the drain removes and emits
  each queued entry at most once, because `drain_config_locks()` does a `mem::take` on the
  pending `Vec` under the same lock that guards the flag swap, so a concurrent second
  dispatch racing the drain either sees the flag already cleared (no-op) or drains an empty
  `Vec` (no-op); nothing is left to re-emit on a later dispatch. What is **not** guaranteed:
  an entry can be lost if the process crashes between enqueue and the next dispatch call
  (best-effort, consistent with every other emission site in this ADR). Drain latency is
  now bounded by the next `VerbRegistry::dispatch` call rather than by the checkpoint
  interval: on a serving daemon, that is at most one poll tick (`channel_poll_loop`'s
  `comm.heartbeat` dispatch, `HAPPY_PATH_INTERVAL = 5s`, `serve.rs:296`) if no other verb is
  dispatched sooner, and typically far less, since any client request also dispatches.
  Both the crash-loss and the bounded-delay properties are acceptable given this data's use
  (auditing what value a running daemon locked in, not a correctness dependency).

  This drain-site choice (moving off `run_checkpoint_task` and onto `VerbRegistry::dispatch`)
  is a design delta from this ADR's prior revision and is called out here explicitly for
  Leo's confirmation, on the strength of the one-directional-dependency rationale above.

(b) **`tx_registry.rs` (ADR-091 Plank 0)** stays out of scope for this ADR, per the audit's
own framing (`SINGLETON_AUDIT.md` §4 item 2): it is a per-process, in-memory, observe-only
registry (`crates/khive-storage/src/tx_registry.rs:1-7`), correctly documented as such and
already correctly excluded from cross-process claims. It **may** be sampled into a future
`EventKind` (e.g., "oldest open transaction at checkpoint time") as an input, but this ADR
does not spec that: it would require deciding how a per-process snapshot becomes a durable
row, which is a separate design question from the lifecycle taxonomy this ADR adds.

(c) **`ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS` scoping nit**
(`crates/khive-pack-knowledge/src/knowledge/vamana.rs:170-171`) is accepted with a comment,
not fixed by this ADR: it is functionally inert in production (the static is unconditionally
compiled in, but only test code writes it, so it always reads 0 and production always falls
through to the real `ANN_WARM_WAIT_TIMEOUT_MS` constant). Listed here per Decision 8's
non-goals, not actioned.

### 8. Non-goals

- **No new storage substrate.** The `events` table, `Event` struct, and `EventStore` trait
  are unchanged except for the new `EventKind` variants (an additive Rust enum change), the
  new `VerbRegistry::event_store()` accessor, the new `DaemonDispatch::
  event_store_for_checkpoint()` accessor, and the small `khive-runtime::config_ledger`
  pending-emission module (Decision 2, Decision 7a). No new migration, no new table, no
  schema change to `events-ddl.sql`.
- **No new wire-surface verb.** A read verb over lifecycle events (e.g., something like
  `comm.health-for-events`, surfacing a channel's recent transition sequence the way
  `comm.health()` surfaces its current state) is plausible future work, but this ADR does
  not spec it. All new emission (Decision 2) calls the storage-level `EventStore::
  append_event` trait method directly; no call site constructs a new request through the
  verb-dispatch DSL or adds a new entry to the `request` tool's verb catalog. Decision 7a's
  `ConfigLocked` drain runs from inside the existing `VerbRegistry::dispatch` function body
  (an internal implementation detail of that function, not a caller-visible verb), which is
  a different thing from adding a new verb a caller could invoke.
- **No #599 log-file remedy.** See Decision 6: #599 stays open.
- **No tracing removal.** Existing `tracing::{info,warn,error,debug}!` call sites in
  `serve.rs` and `checkpoint.rs` are unchanged; they remain the human-readable channel
  (useful whenever logs are actually captured) alongside the new durable events.
- **No fix for the `ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS` scoping nit** (Decision 7c), accepted
  as-is.
- **No events-table prune/retention mechanism** (Decision 5), deferred pending measurement,
  consistent with the open questions already recorded in ADR-032 and ADR-041.

## Failure modes

- **`append_event` fails inside `channel_poll_loop` or `run_checkpoint_task`.** Best-effort:
  `tracing::warn!` and continue (Decision 2), matching #606's precedent. The poll/checkpoint
  loop's primary job (polling channels, checking WAL pages) is never blocked by a telemetry
  write failure.
- **`registry.event_store()` / `dispatcher.event_store_for_checkpoint()` returns `None`**
  (no `EventStore` configured, e.g. a test harness that builds a bare `VerbRegistry` or a
  `DaemonDispatch` implementor that never overrides the default). Every new call site treats
  this as a no-op (mirrors `dispatch`'s own `if let Some(store) = &self.event_store` gate,
  `pack.rs:922`): lifecycle telemetry is best-effort-present, not a hard dependency for the
  poll loop or the checkpoint task to function.
- **A future channel addition (#113 Telegram, #114 email already shipped, #115 WhatsApp)
  multiplies `ChannelPollStarted` volume linearly.** Acknowledged in Decision 5's arithmetic;
  not a correctness risk, a volume-planning input for the deferred prune decision.
- **`ConfigLocked`'s enqueue-then-drain split (Decision 7a) can lose an entry on a crash
  between enqueue and the next `VerbRegistry::dispatch` call, or delay it by however long
  the daemon goes between dispatches.** Acceptable: this is auditing data (what value a
  running daemon locked in), not a correctness dependency, and matches the best-effort
  discipline of every other emission site in this ADR. In practice the delay is bounded by
  at most one poll tick on a serving daemon with the email-channel feature active (Decision
  7a), since `channel_poll_loop` itself dispatches `comm.heartbeat` every tick.
- **`last_error_class` (Decision 1's `ChannelPollFailed` bullet) is process-local, in-memory
  state.** A daemon restart resets it, so the first failure after a restart is always
  treated as "first failure" even if the same error class was already ongoing before the
  restart. This is consistent with `ImapBackoff`'s own state, which resets identically on
  restart, and with the fact that `ChannelPollStarted` already anchors "the loop restarted"
  as a distinguishable event in the sequence.

## Consequences

- New workspace-additive `EventKind` variants (Decision 1): `ChannelPollStarted`,
  `ChannelPollSucceeded`, `ChannelPollFailed`, `ChannelBackoffArmed`, `ChannelBackoffReset`,
  `ChannelHeartbeatPersistFailed`, `ConfigLocked`, `CheckpointOutcomeRecorded`: the closed
  enum grows from 26 to 34 variants. The `EventKind::ALL` round-trip test
  (`event.rs:822-830`) must be extended to cover each new variant, per the existing pattern.
- Two new public methods: `VerbRegistry::event_store(&self) -> Option<Arc<dyn EventStore>>`
  and `DaemonDispatch::event_store_for_checkpoint(&self) -> Option<Arc<dyn EventStore>>`
  (default `None`, Decision 2); `run_checkpoint_task`'s signature grows two parameters
  (`event_store`, `namespace`). No wire-surface (MCP verb) change.
- Two small pieces of new in-process state, both explicitly owned by this ADR (not "no new
  decision logic," as an earlier draft of this ADR claimed): (1) `last_error_class:
  HashMap<(String, String), &'static str>` in `channel_poll_loop`, bounded by the number of
  registered channels, cleared on every successful poll regardless of which events that
  success emits; (2) the `khive-runtime::config_ledger` pending-emission `Vec` plus its
  companion `AtomicBool` flag, bounded by the number of `OnceLock` config sites (3 today)
  and drained to empty on the next `VerbRegistry::dispatch` call, not on a checkpoint tick.
  `run_checkpoint_task`'s existing `was_above_warn` bool (Decision 4) is reused as-is, no new
  state there. Beyond these two additions, `channel_poll_loop`, `VerbRegistry::dispatch`, and
  `run_checkpoint_task` gain a handful of best-effort `append_event` calls at existing
  decision points computed from data `channel_error_class`, `is_backoff_eligible`,
  `ImapBackoff`, and `crossing_warn` already produce today.
- `events` table growth increases by roughly 17,280 rows/day/channel steady state (the one
  unconditional variant) plus incident-bounded edge-triggered rows, plus up to 7,200 rows/
  hour during a sustained WAL-pressure elevation (Decision 5), on top of the existing,
  already-unaddressed per-dispatch `Audit` growth. No prune mechanism ships with this ADR;
  the open retention question (ADR-032, ADR-041) is unchanged in kind, only in degree.
- The three duplicated `CaptureSubscriber` tracing-capture test harnesses (`serve.rs`,
  `checkpoint.rs`, `pack.rs`) become retirable once the corresponding `EventKind` variants
  land and their consumer tests are rewritten against persisted state (Decision 3); this ADR
  does not delete them (implementation-time work, out of scope for this docs-only ADR).
- #599 (daemon stderr loss) and #617 (its own N=3 counter implementation) both remain open,
  tracked separately; this ADR is the shared substrate both build on.

## Alternatives considered

- **Use the existing per-dispatch `Audit` row (already written on every `comm.heartbeat`
  call, at the same ~17,280/day/channel volume as the proposed `ChannelPollStarted`) as the
  sequencing anchor instead of adding a new variant.** Rejected for three reasons, all
  grounded in facts established in Context above: (1) the `Audit` row's payload is the
  gate's `AuditEvent` (`pack.rs:914-940`), which carries the allow/deny decision, actor, and
  verb name, not channel identity (`channel_kind`/`channel_slug`), so it cannot be filtered
  or joined to a specific channel's lifecycle without a secondary lookup; (2) its timestamp
  is stamped at dispatch completion, end-of-tick, from inside `record_channel_heartbeat`'s
  `registry.dispatch("comm.heartbeat", ...)` call (`serve.rs:444`), which runs _after_ the
  tick's other lifecycle decisions (backoff arm/reset, error classification) have already
  happened, so mid-tick transitions would sort _before_ the anchor that is supposed to
  bound them, breaking the "started, then transitioned" ordering a sequencing query needs;
  (3) the row is skipped exactly when `registry.dispatch(...)` itself fails (an error path
  distinct from a channel-poll failure), which is precisely the case where a sequencing
  query would most need an anchor to reason about. `ChannelPollStarted`, by contrast, is
  stamped at the top of the tick before any of these decisions are made, and is emitted
  unconditionally regardless of dispatch outcome.
- **Instrument only `VerbRegistry::dispatch`, richer `Audit` payloads.** Rejected: the
  channel-poll-loop's internal transitions (auth failed, backoff armed) are not verb
  dispatches; they happen between `registry.dispatch("comm.heartbeat", ...)` calls inside
  the loop body. No amount of enriching the existing `Audit` event's payload captures a
  transition that never goes through `dispatch()` at all.
- **A new wire-surface verb (e.g. `comm.append_lifecycle_event`) instead of a direct
  `EventStore::append_event` call.** Rejected for this ADR: adds a new MCP-visible verb for
  a purely internal instrumentation concern, and would itself recurse into `dispatch()`,
  producing a second `Audit` row for every lifecycle row (doubling volume beyond the
  Decision 5 numbers) for no benefit; the direct `EventStore` trait call is simpler and
  already the mechanism `dispatch` itself uses internally.
- **Open `kind: String` instead of a closed enum.** Rejected: the SQL column is already
  untyped `TEXT` with no `CHECK` constraint, so an open string would silently accept typos
  with no compile-time or `EventKind::ALL` round-trip protection; the closed-enum-plus-
  additive-migration pattern the codebase already uses for `EventKind` is preserved.
- **Unconditional per-tick emission for every variant** (simplest to implement). Rejected on
  the volume arithmetic in Decision 5 (172,800 rows/day/process for checkpoint alone at
  500ms cadence, at all times, including when healthy); the sessions.db lesson (ADR-093) is
  that unmeasured, unbounded per-tick accumulation is exactly the growth pattern worth
  avoiding by design, not discovering after the fact.
- **A monotonic checkpoint-cycle counter in the payload instead of the drain/recovery row**
  (an alternative fix for Decision 4's blocker). Rejected as the primary mechanism: it would
  work, but it requires threading and persisting an additional counter field through every
  `CheckpointOutcomeRecorded` row, whereas the drain-row approach reuses the `was_above_warn`
  bool `run_checkpoint_task` already carries and adds at most one extra row per elevation
  episode. The drain-row approach is adopted as cheaper and requiring no new persisted
  state beyond the rows themselves.
- **Extend the `comm.heartbeat` note itself into an append-only history (drop the
  `Uuid::new_v5` upsert key).** Rejected: `comm.health()`'s current-state contract
  (`handlers.rs:1060-1075`) depends on the one-row-per-channel invariant; changing it to an
  append-only shape would require a new read-side aggregation query for every existing
  `comm.health()` caller and duplicates the `events` table's ordering guarantees that already
  exist elsewhere in the schema.

## Test list (informative, implementation-time)

- `channel_poll_loop` emits `ChannelPollStarted` exactly once per tick per registered
  channel, regardless of poll outcome.
- A poll success after a prior backoff-eligible failure emits `ChannelPollSucceeded` and
  `ChannelBackoffReset`; a poll success on an already-healthy channel (no prior failure)
  emits neither.
- A poll failure emits `ChannelPollFailed` on the first failure for a channel and again on
  any subsequent failure whose `error_class` differs from the previous emitted failure's
  class; a repeated failure at the same class emits nothing further (verifies the
  `last_error_class` state transition, including that the entry is cleared on the `Ok(
  envelopes)` branch of every intervening success, not on `ChannelPollSucceeded`
  specifically). A dedicated case: a `"config"`-class failure, followed by a successful poll
  (no `ChannelPollSucceeded` fires, since `"config"` failures never populate `backoffs`),
  followed by a second `"config"`-class failure, must emit `ChannelPollFailed` on the second
  failure too, because the intervening success cleared `last_error_class` regardless of
  which events that success itself emitted.
- `log_eligible_poll_failure`'s escalation edge (`tick.should_warn`) emits exactly one
  `ChannelBackoffArmed` row; a same-step repeat emits zero (mirrors the existing `debug!`-
  not-`warn!` discipline, `serve.rs:462-482`).
- `record_channel_heartbeat`'s dispatch-failure path emits `ChannelHeartbeatPersistFailed`
  and does not panic or abort the poll loop.
- `ConfigLocked` is enqueued exactly once per config key across concurrent early readers
  (race test analogous to the existing `OnceLock` staleness tests, if any exist, for
  `common.rs:40,45` / `context.rs:35`), and is drained to exactly one persisted row via the
  next `VerbRegistry::dispatch` call made after the enqueue (asserted by dispatching one
  no-op verb and checking a `ConfigLocked` row now exists), not via a checkpoint tick;
  dispatching a second time in a row emits nothing further for that key.
- A `CheckpointOutcomeRecorded` sequence whose 3 most-recent rows are all `above_warn = true`
  (a genuine 3-consecutive-cycle sustained elevation) satisfies the #617 WARN query. A
  sequence of isolated single-cycle crossings (each followed by an immediate `above_warn =
  false` drain row before the next, much later, crossing) never produces 3 consecutive
  `above_warn = true` rows in the most-recent-3 window, and does not satisfy the query, even
  after many isolated crossings have accumulated.
- Sequencing assertion: for a synthetic multi-transition scenario (poll started, auth
  failed, backoff armed, poll started, poll succeeded, backoff reset), the ordered
  `(created_at, id)` query returns exactly that sequence.
