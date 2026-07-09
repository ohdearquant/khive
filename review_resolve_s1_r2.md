Verdict: APPROVE-WITH-FIXES
Findings: 0 Blocker, 0 High, 1 Medium, 0 Low

### [Medium] Snapshot Still Does Not Run The Outer-Map Prune

Evidence: `crates/khive-runtime/src/reference_ring.rs:134` defines `prune_outer_map`, including stale-key cleanup and `max_outer_keys` budget eviction; `crates/khive-runtime/src/reference_ring.rs:185` is its only call site. `crates/khive-runtime/src/reference_ring.rs:194`-`:206` implements `snapshot` by evicting stale entries only from the requested `(namespace, actor)` ring and removing only that key if empty.

Why this matters: The r1 fix asked for outer-map cleanup on admit and snapshot. The admit path is now bounded, but a read-only daemon that only snapshots after keys age out will not prune stale, unqueried outer keys, and the regression at `crates/khive-runtime/src/reference_ring.rs:499`-`:511` only proves cleanup for the queried key. This leaves part of the outer-map TTL contract dependent on a future admission.

Suggested fix: Call `prune_outer_map` from `snapshot` as well, with the queried key as the exempt key, before or after taking the requested snapshot. Add a test with two aged-out keys where snapshotting one actor also removes the other stale actor key.

## Looks Right

- Namespace key parity is fixed: admission uses `token.namespace().as_str()` at `crates/khive-runtime/src/pack.rs:1343`-`:1345`, and lookup uses the same key at `crates/khive-runtime/src/reference_resolution.rs:172`-`:174`. The regression at `crates/khive-pack-kg/src/dispatch.rs:1302`-`:1334` uses non-local `default_namespace="lambda:leo"` and asserts ring confidence `0.95`, so it would not pass by search fallback.
- Non-entity ring admission is fixed for the reviewed shapes: `substrate_admits_as_entity` rejects `kind=edge|event` and any top-level `content` at `crates/khive-runtime/src/reference_ring.rs:252`-`:267`; `display_name` now only reads `name` at `crates/khive-runtime/src/reference_ring.rs:215`-`:219`. The note-content fallback is gone.
- The outer-map LRU implementation itself is real on admission: `prune_outer_map` sorts by each ring's newest `touched_at` at `crates/khive-runtime/src/reference_ring.rs:148`-`:156`, and re-admission updates recency by removing the old id and pushing a fresh entry at `crates/khive-runtime/src/reference_ring.rs:173`-`:180`.
- Ring mutex poisoning is fixed in implementation paths: `lock_state` recovers with `into_inner()` plus `tracing::warn!` at `crates/khive-runtime/src/reference_ring.rs:103`-`:112`, and the only ring-state `.lock().unwrap()` matches left are test-only at `crates/khive-runtime/src/reference_ring.rs:505` and `:560`.
- Search confidence is fixed: singleton/decisive search resolutions report `SEARCH_RESOLVED_CONFIDENCE = 0.6` at `crates/khive-runtime/src/reference_resolution.rs:83`, `:240`-`:253`; close multi-hit candidates return `Ambiguous` at `:245`-`:256`; raw RRF remains in `ReferenceCandidate.score` at `:225`-`:232` and the wire candidate score at `crates/khive-pack-kg/src/handlers/resolve.rs:64`-`:74`.
- UUID/prefix parity is fixed and documented as entity-only: full UUIDs resolve only `Resolved::Entity` at `crates/khive-runtime/src/reference_resolution.rs:120`-`:127`, prefixes are filtered through the same entity-only check at `:129`-`:168`, and `resolve_id_string_passthrough_is_entity_only` proves a real note id is `get`-able but both full UUID and short prefix return `not_found` at `crates/khive-pack-kg/src/dispatch.rs:1342`-`:1393`.
- The fix-commit tests I inspected are not tautological. The remaining gap is coverage for global outer-map pruning during `snapshot`, which is the Medium finding above.

## Commands Run

- `date -Iseconds`: confirmed review time.
- `git status --short --branch`: confirmed `feat/resolve-s1` at `958fd5ae`, with unrelated untracked r1 artifacts.
- `git show --stat --oneline --decorate 958fd5ae` and `git show --name-only 958fd5ae`: identified the fix-commit surface.
- `sed` / `nl` / `rg` / `git diff`: read the r1 review, the four changed files, relevant handlers, and the targeted tests.
- `git diff --check 958fd5ae^ 958fd5ae`: clean.
- `mcp__khive.request` with `memory.recall` and `knowledge.suggest`: memory recall returned prior khive review principles; knowledge suggested no domains.

## What I Did Not Check

- Did not compile, run clippy, or run tests, per instruction. I treated lambda's reported fmt/clippy/1155-test/smoke-test results as external evidence, not as independently observed results.
- Did not broaden the review beyond commit `958fd5ae` except where needed to verify the r1 finding contracts and real handler response shapes.

## Re-Review Guidance

Narrow follow-up only: verify `snapshot` invokes the same outer-map TTL/budget prune as `admit`, with a regression that snapshots one key and proves a different stale key was removed.

Domain utility: LOW — `knowledge.suggest` returned no domain IDs, so the useful external context was limited to prior khive reviewer memory and local code evidence.
