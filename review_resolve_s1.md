Verdict: APPROVE-WITH-FIXES
Findings: 0 Blocker, 2 High, 4 Medium, 0 Low

### [High] Ring Admission And Lookup Use Different Namespace Keys

Evidence: `/Users/lion/projects/khive/khive/.khive/workspaces/20260709/unified-verb/DRAFT-ADR-unified-verb.md:189` defines `resolve_reference(nl_ref, namespace, actor)` and `:197` makes the ring per `(namespace, actor)`. `crates/khive-runtime/src/pack.rs:1015` derives `ns` from the request identity/default namespace, `:1156` mints a default-path token whose primary namespace is `local`, and `:1332` admits into the ring under `ns.as_str()`. But `crates/khive-runtime/src/reference_resolution.rs:122`-`:124` looks up the ring with `token.namespace().as_str()`.

Why this matters: In the existing non-local default namespace path, e.g. the tested `default_namespace="lambda:leo"` case, a same-actor `create` with no explicit `namespace` admits under `lambda:leo`, while a later same-actor `resolve` snapshots `local`. That makes the S1 ring invisible on a supported dispatch identity path.

Suggested fix: Thread the dispatch-resolved namespace into `resolve_reference` and use the same key for admission and lookup, or deliberately key both admission and lookup on the token namespace if `local` is the intended S1 namespace. Add a regression with non-local `default_namespace`.

### [High] Ring Can Resolve Non-Entity Records Despite Entity-Id Contract

Evidence: The ADR says the ring matches "entities this actor touched" at `/Users/lion/projects/khive/khive/.khive/workspaces/20260709/unified-verb/DRAFT-ADR-unified-verb.md:191` and stores "entity ids" at `:197`-`:198`. `crates/khive-runtime/src/reference_ring.rs:168`-`:170` admits any `create|get|update|delete` result with an `id`, and `:126`-`:140` even falls back to a note `content` snippet as the display name. `crates/khive-pack-kg/src/handlers/create.rs:327`-`:355` shows `create(kind=note)` returns a note id/content, while `crates/khive-pack-kg/src/handlers/get.rs:69`-`:89` returns notes, edges, and events through generic `get`.

Why this matters: A recent `create(kind="observation", content="the old record")` or `get` of a note can make `resolve(refs=["the old record"])` return a note id at ring confidence, while the fallback path is entity-only hybrid search. Later slices that substitute resolved refs into write plans would receive the wrong substrate.

Suggested fix: Restrict ring admissions to entity ids only, plus `link` endpoint ids. For generic `get/update/delete`, check the returned `kind`/substrate before admitting; do not use note content as a ring name for S1 entity resolution.

### [Medium] Outer Ring Map Is Unbounded Across Actors

Evidence: `crates/khive-runtime/src/reference_ring.rs:40` stores rings in `HashMap<RingKey, VecDeque<RingEntry>>`; `:95`-`:99` evicts stale entries only inside the current ring after creating/fetching that key; `:115`-`:122` snapshots only the requested key and does not remove empty keys. The ADR requires daemon-warm ephemeral state with TTL/size eviction at `/Users/lion/projects/khive/khive/.khive/workspaces/20260709/unified-verb/DRAFT-ADR-unified-verb.md:388`-`:390`.

Why this matters: Each inner ring is capped at 64 entries, but distinct `(namespace, actor)` keys are never removed or capped. A daemon serving many transient actor ids can grow memory without TTL-based outer eviction.

Suggested fix: Prune empty/stale keys during admit/snapshot or on a cheap periodic sweep, and add a maximum outer-key budget with LRU/age eviction.

### [Medium] Ring Mutex Poisoning Can Panic The Dispatch Hot Path

Evidence: `crates/khive-runtime/src/reference_ring.rs:97` and `:117` call `lock().expect("reference ring mutex poisoned")`. `crates/khive-runtime/src/pack.rs:1327`-`:1333` invokes `ReferenceRing::admit` after a successful pack dispatch, before returning the already-successful result.

Why this matters: S1 admission is supposed to be best-effort daemon-warm cache maintenance. If the mutex is poisoned, a successful write/read dispatch can panic instead of returning its result, violating the “admission failure must never fail the op” hot-path requirement.

Suggested fix: Treat poisoning as a cache miss/admission skip with a warning, or recover the inner state via `into_inner()` and continue best-effort. Do the same for `snapshot`.

### [Medium] Hybrid-Search Singleton Resolution Uses An Impossible Absolute Threshold

Evidence: `crates/khive-runtime/src/reference_resolution.rs:51`-`:60` says RRF scores are not fixed 0..1 confidence values, but `:187`-`:195` still requires a single search candidate to score at least `0.7` before resolving. RRF scoring is `sum 1/(k + rank)` in `crates/khive-fusion/src/rrf.rs:8`; the existing tests document a rank-1 single-source hit as `1/61` at `crates/khive-fusion/src/rrf.rs:87`-`:92` and a best two-source hit as `2/61` in `crates/khive-retrieval/tests/fusion_surface.rs:27`-`:29`.

Why this matters: A lone hybrid-search fallback hit will normally return `Ambiguous`, not `Resolved`, even when there are no close candidates. That under-delivers the S1 fallback contract at `/Users/lion/projects/khive/khive/.khive/workspaces/20260709/unified-verb/DRAFT-ADR-unified-verb.md:193`-`:195` and makes `confidence` mix ring constants with raw RRF values.

Suggested fix: Convert search scores into a documented search-stage confidence before applying thresholds, or use a deterministic margin/presence rule for singleton hits and reserve raw RRF as `score` in candidates.

### [Medium] Id-String Passthrough Is Inconsistent For Full UUIDs Versus Prefixes

Evidence: The S1 row requires UUID/short-prefix id strings to resolve through the existing id path at `/Users/lion/projects/khive/khive/.khive/workspaces/20260709/unified-verb/DRAFT-ADR-unified-verb.md:263`. `crates/khive-runtime/src/reference_resolution.rs:89`-`:95` resolves a full UUID only through `runtime.resolve_by_id`, which `crates/khive-runtime/src/operations.rs:3217`-`:3234` limits to entities/notes. The prefix path at `crates/khive-runtime/src/operations.rs:3120`-`:3124` scans `entities`, `notes`, `events`, and `graph_edges`, and generic `get` resolves edge/event/pack-private UUIDs at `crates/khive-pack-kg/src/handlers/get.rs:83`-`:98`.

Why this matters: The S1 row says UUID/short-prefix id strings resolve gracefully through the existing id path. Today a graph edge full UUID returns `NotFound`, while its short prefix can resolve; pack-private UUIDs also miss even though generic `get(id)` can resolve them.

Suggested fix: Reuse the same full by-id resolution surface as `get`, or explicitly restrict both UUID and prefix passthrough to entity ids and document that restriction.

## Looks Right

- Failed ops do not admit: admission is gated by `if let Ok(ref ok_val) = result` in `pack.rs:1327`.
- `search`/`list` result sets do not admit: `ring_admissions_for` only matches `create|get|update|delete|merge|link` at `reference_ring.rs:167`-`:186`.
- Bulk `create`/`link` exclusion is a sound reading of “caller named or received one specific id”; the response `attempted` guard at `reference_ring.rs:159`-`:160` keeps plural shapes out of the ring.
- `link` admits both endpoints from the returned singleton edge JSON at `reference_ring.rs:176`-`:184`.
- `resolve` is thin/read-only and returns per-ref `resolved`/`ambiguous`/`not_found` wire shapes in `handlers/resolve.rs:23`-`:80`.
- Merge-ordering note: `tests/smoke_test.py:213` correctly says 77 for this branch alone; after PR #761 lands first, this branch should rebase to 78. This is not a defect in commit `b1d2a3b4`.

## Commands Run

- `date -Iseconds`: confirmed review started before the deadline.
- `git status --short --branch`: confirmed `feat/resolve-s1` at `b1d2a3b4`, with unrelated untracked `impl_report_resolve_s1.md`.
- `git show --stat --oneline --decorate --no-renames b1d2a3b4`: inspected changed-file surface.
- `rg`/`sed`/`nl`: read the draft ADR sections, `CLAUDE.md`, and all changed implementation/test files.
- `git diff --check b1d2a3b4^ b1d2a3b4`: clean.

## What I Did Not Check

- Did not compile or run tests, per instruction. I did not independently verify the reported cargo/fmt/clippy/workspace-check results.
- Did not benchmark the scripted S1 reference-resolution call-count gate.

## Re-Review Guidance

Narrow re-review after fixes: focus on namespace key consistency, ring outer-map eviction, mutex poison handling, and search-confidence semantics. Add targeted tests for non-local request identity/default namespace and prefix/full-UUID parity.

Domain utility: SKIPPED — no lore/domain tool was exposed in this session, so the review used the local draft ADR plus khive review references.
