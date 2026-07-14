# Test rationale notes

Long-form test rationale extracted from `#[cfg(test)]` doc-comments across
khive-db. Test code doesn't render on docs.rs, so in-source comments were
trimmed to short summaries; this file keeps the full "why" for each test —
what regression/incident it guards, what edge case it covers, and any
non-obvious setup detail.

## `checkpoint.rs` tests

### `log_tx_registry_oldest_debug_reports_oldest_open_entry`

`#[serial(tx_registry)]`: the registry is a process-wide singleton shared
across every test in this binary — see `pool.rs`'s and `sql_bridge.rs`'s
registry tests, which share this same serial group (these three were
previously unserialized and could race, corrupting each other's
`oldest()`/`snapshot()` reads).

This test does NOT hardcode "checkpoint_tick_test" as the expected label:
production write paths elsewhere in this same test binary (vectors/graph/text
stores) also register short-lived registry entries while their own tests
run, and `serial(tx_registry)` only serializes against the OTHER tests in
that same group, not against every write path in the crate. Instead it
samples `oldest()` itself immediately before invoking the function under
test and asserts the logged label matches whatever the registry considers
oldest at that instant — deterministic regardless of unrelated concurrent
registry churn, while still verifying `log_tx_registry_oldest_debug`
correctly surfaces the registry's own `oldest()` answer.

### `checkpoint_task_exits_via_shutdown_signal_with_live_event_store_pool_clone`

Regression for issue #774: on the production boot path, the daemon passes
`run_checkpoint_task` both `pool` directly and an `event_store` that
internally retains its own `Arc::clone` of the same pool
(`SqlEventStore::new_scoped`). A strong-count-based exit condition can never
fire in that shape, because the task always observes at least two live
clones — its own `pool` argument plus the one buried in `event_store`. This
test reproduces that exact ownership shape (a real `SqlEventStore` holding a
sibling clone) and asserts the task still exits promptly via the
watch-channel signal, proving the fix does not depend on `Arc::strong_count`
at all.

### `checkpoint_high_water_does_not_block_behind_reader`

Regression: a high-water tick must NOT block behind an active read
transaction.

Isomorphism guarantee: this test FAILS if `checkpoint_once` regresses to
`PRAGMA wal_checkpoint(TRUNCATE)`. Confirmed by reasoning: TRUNCATE inherits
RESTART semantics and will invoke the busy handler (sleeping up to
`busy_timeout`) while waiting for the open reader snapshot to release. With
`busy_timeout = 2000ms` a TRUNCATE regression causes the call to take
~2000ms, blowing the <500ms assertion. PASSIVE returns in <1ms even with an
open reader, because PASSIVE never waits for readers.

Why `busy_timeout = 2000ms` and threshold `< 500ms`: the original 200ms
busy_timeout / 50ms threshold was too tight for contended CI runners where
PASSIVE legitimately takes 50-200ms under parallel-test load. Raising the
busy_timeout to 2000ms keeps the PASSIVE path well below 500ms while a
TRUNCATE regression blocks for ~2000ms — a 4x safety margin on both sides.

An idle reader connection (no `BEGIN`) does NOT pin frames and would not
cause TRUNCATE to wait — an actual open read transaction is required for
the isomorphism to hold.

### `checkpoint_config_rejects_reversed_tx_thresholds`

Fix: a reversed pair — `KHIVE_TX_WARN_SECS` >= `KHIVE_TX_MAX_AGE_SECS` —
must not be honored independently. Before this fix, WARN_SECS=120 /
MAX_AGE_SECS=30 parsed both values successfully (each is independently
positive) and produced a sweep that emits `Stale` at 30s while never
reaching the `Warn` crossing until 120s — inverting the intended severity
ladder. Both thresholds must instead fall back to their defaults together.

### `checkpoint_config_rejects_equal_tx_thresholds`

Same invariant, the degenerate equal case: WARN_SECS == MAX_AGE_SECS would
make an entry cross both rungs on the exact same tick every time,
collapsing the two-rung severity ladder into one. Must also fall back to
defaults, not merely reject a strictly-reversed pair.

### `skipped_tick_does_not_reset_high_water_crossing_state`

Regression: a Skipped tick must NOT reset `was_above_high_water`.

Before the fix, `checkpoint_once` returned `0` on both a genuinely-empty WAL
and a writer-busy skip. The task treated `0` as an observed page count and
reset `was_above_high_water`, re-arming the rate limit on every busy tick.
With the fix, `CheckpointTick::Skipped` leaves crossing state unchanged.

This test drives `crossing_warn` directly (the pure function that owns the
decision) rather than going through the async task, which would require a
logging harness.

### `tx_age_sweep_stale_replacement_without_intervening_clear_still_names_new_entry`

Fix: a stale entry (A) that closes and is immediately replaced by an
ALREADY-stale entry (B) on the very next observed tick — no intervening
below-threshold or empty tick, unlike `tx_age_sweep_rearms_after_entry_clears`
— must still emit both rungs for B. Before the identity-tracking fix,
`was_above_warn` and `was_above_max_age` were already `true` from A, so B's
crossing was silently swallowed: the alert stayed latched to a departed
caller while a different long-lived span was now pinning the database.

### `tx_age_sweep_uses_configured_thresholds_not_hardcoded_defaults`

`KHIVE_TX_WARN_SECS` / `KHIVE_TX_MAX_AGE_SECS` are read into the config via
`from_env` at `run_checkpoint_task` construction time, so this closes the
loop from env var to the actual emitted rung (the earlier
`checkpoint_config_env_override` test only asserts the config fields
themselves).

### `tx_age_sweep_names_long_lived_reader_pinning_wal_past_high_water`

Integration-level regression for the incident this ADR fixes: a real `BEGIN
DEFERRED` reader pins a WAL snapshot (exactly like
`checkpoint_high_water_does_not_block_behind_reader`) while also being
registered in the shared `tx_registry` (simulating an instrumented
long-lived-reader call site such as `graph.rs`'s `graph_traverse_read`),
writes drive `wal_pages` past `high_water_pages`, and — with a
millisecond-scale `tx_max_age_secs` so the test does not sleep for real
minutes — the Plank 1 sweep escalates to `Stale` naming that exact reader,
alongside the existing Plank 0 high-water WARN. This is the "detection
works, mitigation missing" gap from the incident: the sweep now gives the
operator the specific, escalating, un-silenced signal that a single
one-shot high-water WARN does not.

### `tx_age_sweep_own_entry_survives_concurrent_older_registration`

Regression for #926: reproduces the exact race that made
`tx_age_sweep_names_long_lived_reader_pinning_wal_past_high_water` flaky,
directly rather than hoping cargo's test scheduler happens to interleave two
unrelated tests. `tx_registry` is a process-wide singleton; a decoy entry
registered before this test's own entry is genuinely older, so raw
`oldest()` cannot return the test fixture — exactly what an unrelated,
concurrently-running write path (e.g. `graph_upsert_edges`) could do in the
real suite. The fix (looking up this test's own entry by label via
`snapshot()` instead of trusting global `oldest()`) must still correctly
name and escalate THIS entry despite that older decoy.
