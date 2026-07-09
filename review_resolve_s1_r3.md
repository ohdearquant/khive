Verdict: APPROVE - commit 86b00651 satisfies the r2 re-review condition: `snapshot()` now invokes `prune_outer_map`, and the added regression proves snapshotting one stale key removes a different stale key that was not queried.
Findings: 0 Blocker, 0 High, 0 Medium, 0 Low

## Looks Right

- `crates/khive-runtime/src/reference_ring.rs:134`-`:157` keeps the shared `prune_outer_map` helper as the single TTL plus outer-budget sweep: it evicts stale entries from every ring, drops empty keys, then applies the least-recently-touched outer-key budget pass.
- `crates/khive-runtime/src/reference_ring.rs:185` still calls that helper from `admit`, and `crates/khive-runtime/src/reference_ring.rs:200`-`:212` now calls the same helper from `snapshot` after collecting the queried ring's live entries. That closes the r2 gap where read-only snapshot traffic could leave unrelated stale outer-map keys behind indefinitely.
- `crates/khive-runtime/src/reference_ring.rs:520`-`:544` adds the requested regression, `snapshot_prunes_other_stale_keys_it_did_not_query`: two actor keys age out, only `actor:queried` is snapshotted, and the test asserts `actor:other` was removed from the outer map.
- The existing queried-key cleanup test remains intact at `crates/khive-runtime/src/reference_ring.rs:505`-`:517`, so the fix covers both the directly queried stale key and unrelated stale keys.

## Commands Run

- `date -Iseconds`: confirmed review time.
- `git status --short --branch`: confirmed `feat/resolve-s1` with existing untracked review/report artifacts.
- `git show --stat --name-status --oneline 86b00651`: confirmed the fix commit only changes `crates/khive-runtime/src/reference_ring.rs`.
- `git show --patch --find-renames --find-copies --function-context --stat 86b00651`: inspected the full commit diff.
- `sed -n '1,220p' review_resolve_s1_r2.md`: verified the exact r2 re-review guidance.
- `nl -ba crates/khive-runtime/src/reference_ring.rs | sed -n '120,230p'` and `sed -n '490,570p'`: verified current implementation and test line references.
- `rg -n "prune_outer_map|snapshot_prunes_other_stale_keys_it_did_not_query|snapshot_prunes_a_key_that_ages_out_entirely" crates/khive-runtime/src/reference_ring.rs`: confirmed call sites and regression locations.
- `git diff --check 86b00651^ 86b00651`: clean.
- `mcp__khive.request` with `memory.recall`, `search`, `knowledge.suggest`, and `knowledge.compose`: recalled the prior r2 review principle and loaded a lightweight reviewer domain brief.

## What I Did Not Check

- Did not compile, run clippy, or run tests, per instruction. I treated lambda's reported fmt/clippy clean and 1156 passing tests as external evidence, not independently observed results.
- Did not broaden the review beyond commit `86b00651` and the r2 finding contract.

## Re-Review Guidance

No further re-review needed for the r2 Medium. A future review can be broad only if additional commits land after `86b00651`.

Domain utility: LOW - the useful context came from prior khive reviewer memory and local code evidence; the composed storage-performance brief was only marginally relevant to this narrow correctness re-review.
