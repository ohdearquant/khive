# khive-brain-core — regression test strategy

Why each of this crate's non-obvious regression tests exists and what invariant it
guards. Each heading names the test function and its source file; the `.rs`
doc-comment on the test itself carries only a 1–3 line summary that points here.

## `posterior.rs::snapshot_restore_eviction_equivalence` (BRAINCORE-AUD-001)

Snapshot/restore must be eviction-equivalent to uninterrupted execution.
Scenario: capacity 2, observe A then B, snapshot, restore, then observe C —
the restored path must evict A (the oldest), not B, exactly like the live
in-memory path would.

## `posterior.rs::oversized_snapshot_restore_is_bounded_by_capacity` (BRAINCORE-AUD-001)

A snapshot with more entries than the configured capacity must restore
bounded, not exceed it.

## `profile.rs::adr048_ess_cap_mean_shift_ge_0_3` (ADR-048 Phase-1 gate)

ESS cap convergence: after 200 positive events followed by 200 opposing
events, the salience posterior mean must shift by at least 0.3. This proves
the ESS cap keeps the posterior responsive to new evidence rather than
letting accumulated mass drown out a reversal.

## `profile.rs::legacy_entity_posteriors_restore_uses_deterministic_order_and_capacity` (BRAINCORE-AUD-001)

A legacy snapshot (version 0, no order metadata) with more entries than the
configured capacity must restore bounded, using a deterministic
ascending-`Uuid` compatibility order.

## `brain_state.rs::sort_fallback_candidates_produces_created_at_then_id_order`

`sort_fallback_candidates` must produce `(created_at ASC, id ASC)` order on
an intentionally REVERSE-sorted input vector. This is a fail-before/pass-after
unit test for the helper itself: the old `HashMap.values().find_map()`
implementation would have returned the first element from HashMap iteration
order (non-deterministic). The helper under test sorts its input in-place;
passing a reverse-order slice guarantees the test would fail against any
implementation that skips the sort.

## `brain_state.rs::resolve_fallback_is_deterministic`

End-to-end: `BrainState::resolve` must select the earliest-created profile
regardless of HashMap insertion order. This is a secondary integration guard
that complements the `sort_fallback_candidates` helper unit tests above.

## `brain_state.rs::router_state_and_adapter_set_round_trip`

`router_state` and `adapter_set` survive a `to_snapshot` / `from_snapshot`
round-trip byte-for-byte. This is a fail-before/pass-after test: it would
fail if either conversion dropped the new fields.

## `brain_state.rs::snapshot_missing_router_and_adapter_fields_defaults_to_empty`

Deserializing a `BrainStateSnapshot` JSON that omits `router_state` and
`adapter_set` must succeed and yield empty maps (no migration required). The
test serializes a fresh snapshot, strips both keys from the JSON object, then
re-deserializes — proving that `#[serde(default)]` is wired correctly.

## `brain_state.rs::validate_brain_state_snapshot_with_capacity_rejects_profile_state_over_capacity` (BRAINCORE-AUD-001)

Capacity violation in a named `profile_states` entry (not just the default
`balanced-recall-v1` profile) must also be rejected — the capacity bound
applies per-profile, not just to the default.

## `brain_state.rs::restore_to_snapshot_restore_idempotency`

A validated snapshot must restore→re-snapshot→restore to the same state —
the validation gate must not itself introduce drift, and a snapshot that
passes validation once must keep passing after a round-trip through
`BrainState`.
