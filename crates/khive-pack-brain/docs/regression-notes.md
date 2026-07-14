# Regression test notes

Source: `crates/khive-pack-brain/src/tests.rs` (private `mod tests`, internal to the crate).

## `concurrent_cold_load_does_not_clobber_live_state`

Deterministic interleaving test for the concurrent cold-load race. Interleaving
manufactured by the test-only `POST_LOAD_HOOK` in `persist.rs`:

1. Loader B is spawned; it calls `ensure_loaded` for "race-ns", completes the async DB
   scan, then PAUSES at the hook before acquiring the final tracker lock (it signals
   "reached" on a oneshot and awaits "proceed").
2. While B is paused, the test task runs Loader A to completion (no hook active for A —
   hook was already consumed by B's `.take()`). A publishes "race-ns" as active.
3. The test task mutates state via `brain.bind` (adds one binding to A's namespace).
   Binding count is now 1.
4. The test task sends "proceed" to B. B resumes, enters the final tracker block, and:
   - OLD code: sees `current_ns = Some("race-ns")`, takes the `swap_namespace` path, and
     B's stale cold-loaded `brain_state` (binding count 0) overrides the live state. Final
     binding count = 0. TEST FAILS.
   - FIXED code: re-checks `is_active("race-ns")` — true — and returns early without
     touching `*state`. Final binding count = 1. TEST PASSES.

FAIL-before / PASS-after evidence is produced by running:
```
cargo test -p khive-pack-brain -- concurrent_cold_load_does_not_clobber_live_state
```
against the reverted commit and against the fixed commit.

## `dispatch_gate_race_is_observable_without_gate`

Proves the dispatch atomicity race EXISTS without the gate.

Directly manufactures the interleaving that `dispatch()` without a gate would permit: A
calls `ensure_loaded(ns-a)` → B calls `ensure_loaded(ns-b)` (swaps slot to ns-b) → A runs
its handler against the now-ns-b slot.

This test does NOT go through `dispatch()` — it manually sequences the `ensure_loaded` +
handler steps in the exact order that a scheduler can produce, giving a deterministic
reproduction of the race. It runs against the PRODUCTION code (not a simulated removal) by
calling `ensure_loaded` and the handler directly, bypassing the gate — proving the gate is
NECESSARY, not just incidental.

Expected: A's `brain.bind` lands in the wrong namespace's bookkeeping. Since #457/#458,
`handle_bind` durably persists through `persist_brain_state_mutation`, which also
republishes `tracker.active_namespace` as `token.namespace()` (`bare-ns-a`) on success —
even though the live state it mutated was actually ns-b's (loaded in step 2). That
mislabels the save-restore bookkeeping, so the corruption now surfaces on the OPPOSITE
side from before the durability fix: ns-b's slot comes back empty (its live content, plus
A's leaked write, gets saved under the wrong namespace key), and A's leaked binding
resurfaces under ns-a instead. The exact shape of the corruption changed; the underlying
point — that bypassing the gate corrupts namespace isolation — still holds.

## `dispatch_gate_prevents_cross_namespace_slot_swap`

Proves the dispatch gate FIXES the race: with the gate held across `ensure_loaded` +
handler, no concurrent namespace swap can occur between the two steps.

Interleaving manufactured by `DISPATCH_INTERLEAVE_HOOK` in `pack.rs` (`cfg(test)` only),
which fires inside `dispatch()` AFTER `ensure_loaded` returns and BEFORE the handler
acquires `self.state`. While A is paused at the hook: with the gate, B is blocked on
`dispatch_gate.lock().await`; without the gate, B would run and swap the slot.

Test sequence: (1) A enters `dispatch()`, acquires gate, calls `ensure_loaded(ns-a)`,
pauses. (2) B tries to enter `dispatch()` — blocked on the gate. (3) Test releases A — A's
handler runs `brain.bind` for ns-a. (4) A completes, releases gate; B runs
`ensure_loaded(ns-b)` + `brain.bind`. (5) Assert `bindings(ns-a) == 1`,
`bindings(ns-b) == 1`.

PASS (gate present): each namespace sees exactly its own binding. FAIL (gate absent): see
`dispatch_gate_race_is_observable_without_gate` above.

## `feedback_note_target_resolves_through_observed_as_signal` (#831)

ADR-041 permits `brain.feedback` targets on entities AND notes, but the emitted event
previously always carried `SubstrateKind::Event`, so the `event_observations` decoder
hard-coded `ReferentKind::Entity` and the `observed_as_signal` synthetic-edge query
lowering only admitted entity referents — a note target's signal observation existed in
storage but was unreachable from `observed_as_signal`. This test feeds a REAL stored note
through `brain.feedback` end-to-end and confirms it resolves via the canonical ADR-041 §11
GQL query.

## `ensure_loaded_publication_is_atomic`

Tests the invariant by: (1) loading namespace A and writing a binding (observable state
mutation), (2) loading namespace B — saves A's state, loads fresh B state, (3) switching
back to namespace A — restores the saved state, (4) after each `ensure_loaded` call the
tracker fields must be consistent.
