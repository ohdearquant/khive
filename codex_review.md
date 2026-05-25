# Codex Review - impl-c05 (prior review)

Verdict: REQUEST CHANGES
Findings: 0 Critical, 4 Major, 1 Medium, 0 Suggestions

## Findings

### [Major] Workspace all-target CI still compiles stale vector API call sites

Evidence: `crates/khive-db/src/backend.rs:517` still calls `insert(id, kind, "local", vec![...])`; `crates/khive-db/src/backend.rs:528` still constructs `VectorSearchRequest { query_embedding: ... }`; `crates/khive-db/src/backend.rs:551` repeats the old four-argument `insert` call. `crates/khive-db/src/backend.rs:309` also trips clippy's `redundant_closure` lint.

Why this matters: the branch changes the public `VectorStore` and `VectorSearchRequest` contract, but the all-target workspace gate catches old test code under feature unification. The requested `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` do not pass.

Suggested fix: update the vector-enabled backend tests to pass `field` plus `Vec<Vec<f32>>`, replace `query_embedding` with `query_vectors` plus the new required fields, remove the redundant closure, then rerun fmt/clippy/test with all targets.

### [Major] `VectorSearchRequest.filter` reintroduces ADR-044's rejected silent-drop path

Evidence: ADR-044 specifies `search_with_filter(&self, request: &VectorSearchRequest, filter: &VectorMetadataFilter)` at `docs/adr/ADR-044-vector-store-extensions.md:185` and explicitly rejects `Option<VectorMetadataFilter>` on `VectorSearchRequest` at `docs/adr/ADR-044-vector-store-extensions.md:474`. The implementation adds `pub filter: Option<VectorMetadataFilter>` to `VectorSearchRequest` at `crates/khive-storage/src/types.rs:192`, while `SqliteVecStore::search` only reads `query_vectors`, `namespace`, and `kind` at `crates/khive-db/src/stores/vectors.rs:337` through `crates/khive-db/src/stores/vectors.rs:353`.

Why this matters: callers can now pass a non-empty filter to `search()` and get unfiltered results even though `SqliteVecStore::capabilities()` advertises `supports_filter = false`. That is the exact failure mode ADR-044 separates into `search_with_filter`.

Suggested fix: remove `filter` from `VectorSearchRequest` and keep filter pushdown exclusively on `search_with_filter`, or make `search()` reject any non-empty request filter with `StorageError::Unsupported`. Align the `search_with_filter` signature with ADR-044's borrowed parameters and add the specified debug assertion for backends that claim filter support without overriding.

### [Major] Sparse single insert cannot preserve substrate kind, so kind-filtered sparse search is broken

Evidence: ADR-031's sparse store contract includes a `kind: SubstrateKind` parameter for `insert_sparse` at `docs/adr/ADR-031-multi-engine-retrieval.md:503`. The implemented trait omits kind at `crates/khive-storage/src/sparse.rs:13` through `crates/khive-storage/src/sparse.rs:19`; the SQLite upsert hard-codes `kind` to `''` at `crates/khive-db/src/stores/sparse.rs:198` through `crates/khive-db/src/stores/sparse.rs:200`; search applies `AND kind = ?2` when `SparseSearchRequest.kind` is set at `crates/khive-db/src/stores/sparse.rs:335` through `crates/khive-db/src/stores/sparse.rs:340`.

Why this matters: records inserted through the primary `insert_sparse` API disappear from any kind-filtered sparse search. The only path that writes a real kind is `insert_batch`, which makes the single-record API semantically weaker than the batch API.

Suggested fix: add `kind: SubstrateKind` to `SparseStore::insert_sparse` and persist it, or replace the single-record API with a `SparseRecord`-based insert. Add a regression test that inserts an entity sparse vector and verifies `kind: Some(SubstrateKind::Entity)` returns it while `Note` does not.

### [Major] Dense vector `field` is public but not part of storage identity

Evidence: `VectorRecord` documents `field` as the embedding field represented by the record at `crates/khive-storage/src/types.rs:178`, but the sqlite-vec table still declares only `subject_id TEXT PRIMARY KEY` at `crates/khive-db/src/backend.rs:253`. Both single and batch inserts delete by only `subject_id` and `namespace` before inserting at `crates/khive-db/src/stores/vectors.rs:218` through `crates/khive-db/src/stores/vectors.rs:226` and `crates/khive-db/src/stores/vectors.rs:251` through `crates/khive-db/src/stores/vectors.rs:282`.

Why this matters: the API now accepts a field name, but inserting another field for the same subject in the same namespace deletes the previous one. That makes the new field dimension misleading and prevents callers from storing separate `entity.body`, `entity.title`, or other field records.

Suggested fix: make dense vector identity include `field` wherever the backend can support it, or document and enforce that sqlite-vec accepts exactly one field per subject by rejecting conflicting field inserts instead of silently replacing them.

### [Medium] Required contract/compliance test paths were not added

Evidence: ADR-009 calls for backend contract tests under `khive-db/tests/contract/` at `docs/adr/ADR-009-backend-architecture.md:294`, and ADR-044 calls for a vector filter compliance harness at `crates/khive-storage/src/tests/compliance/vector_filter_suite.rs` at `docs/adr/ADR-044-vector-store-extensions.md:521`. The branch adds inline tests in `crates/khive-db/src/stores/sparse.rs:521`, but `find crates/khive-db -maxdepth 3 -type d` shows no `tests/contract` directory and `find crates/khive-storage/src -maxdepth 4 -type f` shows no compliance module.

Why this matters: the cluster acceptance criteria require regression coverage for the changed public APIs and schema behavior. Inline sparse happy-path tests help, but they miss the backend contract path and the filter compliance fixture needed to prevent another silent filter drift.

Suggested fix: add the contract test directory or amend the ADR/cluster plan if inline tests are the intended standard. Add at least one compliance-style test for vector filter behavior, even if sqlite-vec's expected result is `Unsupported`.

## Looks Right

- `khive-storage` now exports `capability`, `entity`, `error`, `event`, `graph`, `note`, `sparse`, `sql`, `text`, `types`, and `vectors`, matching the current eight-trait ADR-005 shape.
- `StorageCapability` matches the current accepted ADR-005 enum shape (`Sql`, `Notes`, `Entities`, `Graph`, `Events`, `Vectors`, `Sparse`, `Text`), not the stale audit summary that still mentioned `Admin`.
- `VectorStoreCapabilities` includes `supports_orphan_sweep`, and sqlite-vec correctly advertises filter/batch/quantization/update/orphan-sweep as false.
- `search_batch` follows the current ADR-044 per-query error isolation semantics, despite the older cluster summary saying it should abort as `StorageResult<Vec<Vec<_>>>`.
- Targeted `cargo test -p khive-storage -p khive-db` passes from the actual Rust workspace directory when `RUSTC_WRAPPER=` bypasses the local sccache sandbox issue.

## Commands Run

- `git status --short --branch`: clean worktree on `show/adr-001-015-alignment/impl-c05`.
- `cargo fmt --all -- --check 2>&1 | tail -5` from the worktree root: did not verify formatting because there is no root `Cargo.toml`; the repo's Rust workspace is under `crates/`.
- `cargo check --workspace 2>&1 | tail -10` from the worktree root: failed with `could not find Cargo.toml`.
- `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20` from the worktree root: failed with `could not find Cargo.toml`.
- `cargo test --workspace 2>&1 | tail -30` from the worktree root: failed with `could not find Cargo.toml`.
- `RUSTC_WRAPPER= cargo check --workspace` from `crates/`: passed.
- `cargo fmt --all --check` from `crates/`: failed with formatting diffs in `khive-storage/src/types.rs`, `khive-storage/src/sparse.rs`, and `khive-storage/src/vectors.rs`.
- `RUSTC_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings` from `crates/`: failed with stale vector API calls and a clippy redundant-closure error.
- `RUSTC_WRAPPER= cargo test --workspace` from `crates/`: failed compiling `khive-db` vector-enabled tests with stale vector API calls.
- `RUSTC_WRAPPER= cargo test -p khive-storage -p khive-db` from `crates/`: passed, 75 `khive-db` tests and 11 `khive-storage` tests.
- `RUSTC_WRAPPER= make ci` from the worktree root: failed at the format check.

## What I Did Not Check

- I did not post this review to GitHub.
- I did not run external lore `suggest`/`compose`; those MCP tools are not available in this session.
- I did not run optional all-features checks after clippy/test already failed on required gates.

## Re-Review Guidance

Run a broad re-review after fixes. The next pass should include all-target clippy/test, sparse kind filtering, vector filter unsupported behavior, and dense vector field identity.

Domain utility: SKIPPED — no lore domain tool is available here; I used the local khive PR and spec-alignment review skills instead.

VERDICT: REQUEST CHANGES

---

# Codex Review - impl-c06 Event Observable and Provenance Model

Verdict: REJECT

Scope reviewed: commit `20a8599` on `show/adr-001-015-alignment/impl-c06` against `show/adr-001-015-alignment/integration`, with the cluster-06 spec and accepted ADRs as the contract.

## Findings

### Critical - Migration versioning violates the accepted ADR-015 ledger

Evidence:
- `docs/adr/ADR-015-schema-migrations.md:37` assigns V5 to ADR-043 `embedding_pipeline_extensions`.
- `docs/adr/ADR-015-schema-migrations.md:38` assigns V6 to ADR-046 `event_sourced_proposals_index`.
- `docs/adr/ADR-015-schema-migrations.md:39` assigns V7 to ADR-041 `event_observations_and_session_id`.
- `docs/adr/ADR-015-schema-migrations.md:40` assigns V8 to ADR-022 `events_namespace_ts_id_idx`.
- `crates/khive-db/src/migrations.rs:184` labels event observability and provenance as V5.
- `crates/khive-db/src/migrations.rs:211` registers version 5 as `event_observability_provenance`.
- `crates/khive-db/src/migrations.rs:358` builds all event observability SQL from a V5 migration helper.
- `crates/khive-db/src/migrations.rs:407` through `crates/khive-db/src/migrations.rs:412` creates the event kind/session/proposal/provenance indexes under that same V5 helper.
- `crates/khive-db/src/migrations.rs:433` and `crates/khive-db/src/migrations.rs:443` assert latest migration version/count is 5.

Why this blocks: the diff steals V5 from ADR-043 and collapses ADR-046, ADR-041, and ADR-022 schema ownership into one version. That breaks the migration ledger contract, makes later cluster ordering unsafe, and can strand databases that already apply an ADR-043 V5 migration.

Fix: preserve or implement ADR-043 as V5, split event proposal/index/provenance/query-index work into the ADR-assigned V6, V7, and V8 migrations, and update migration tests to assert the accepted ledger names and latest version.

### High - `list(kind="event")` does not expose the required event filter contract

Evidence:
- `docs/adr/ADR-022-events-query-surface.md:88` through `docs/adr/ADR-022-events-query-surface.md:96` require event-list wire fields `kind`, `kinds`, `verb`, `verbs`, `outcome`, `actor`, `substrate`, `since`, and `until`.
- `docs/adr/ADR-022-events-query-surface.md:175` through `docs/adr/ADR-022-events-query-surface.md:183` define canonical `EventFilter` fields including `kinds`, `verbs`, `actors`, `substrates`, `after`, `before`, `session_id`, `observed`, and `selected`.
- `docs/adr/ADR-041-event-provenance-projection.md:285` through `docs/adr/ADR-041-event-provenance-projection.md:291` add `observed`, `selected`, and `session_id`.
- `docs/adr/ADR-046-event-sourced-proposals.md:287` through `docs/adr/ADR-046-event-sourced-proposals.md:295` add `payload_proposal_id`.
- `crates/khive-storage/src/event.rs:157` through `crates/khive-storage/src/event.rs:168` defines the storage-side fields.
- `crates/khive-pack-kg/src/handlers.rs:205` through `crates/khive-pack-kg/src/handlers.rs:225` only accepts `verb`, `verbs`, `outcome`, single `actor`, single `substrate`, `since`, and `until`; it has no event `kind`/`kinds`, `ids`, `actors`, `substrates`, `session_id`, `observed`, `selected`, or `payload_proposal_id`.
- `crates/khive-pack-kg/src/handlers.rs:475` through `crates/khive-pack-kg/src/handlers.rs:482` builds an `EventFilter` with only verbs, one substrate, one actor, after, and before.

Why this blocks: storage has the new filter fields, but the public verb handler silently leaves most of them unreachable. Event consumers cannot query by typed event kind, session, observed/selected provenance, or proposal id through the MCP/list surface that ADR-022/041/046 require.

Fix: extend `ListParams` and `event_filter_from_params` to map the full event filter surface, including `EventKind` parsing and multi-value actor/substrate forms, and add handler-level regression tests that fail when these parameters are ignored.

### High - `RerankExecuted` provenance projection does not decode the ADR-042 payload shape

Evidence:
- `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:252` through `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:258` define `RerankExecuted` payload fields `candidates: Vec<Uuid>`, `reranked: Vec<(Uuid, HashMap<&'static str, f32>)>`, and `final_scores: Vec<(Uuid, f32)>`.
- `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:264` through `docs/adr/ADR-042-local-rerank-via-lattice-inference.md:267` require `Selected` rows from the rerank output order.
- `docs/adr/ADR-041-event-provenance-projection.md:176` through `docs/adr/ADR-041-event-provenance-projection.md:178` require `RerankExecuted` to project both `Candidate` and `Selected` observations.
- `crates/khive-db/src/stores/event.rs:297` through `crates/khive-db/src/stores/event.rs:300` routes `RerankExecuted` through the generic rank decoder.
- `crates/khive-db/src/stores/event.rs:314` through `crates/khive-db/src/stores/event.rs:330` accepts only arrays of UUID strings and returns an empty vector when a field is absent.
- `crates/khive-db/src/stores/event.rs:361` through `crates/khive-db/src/stores/event.rs:363` tries `selected`, then `reranked`, then `final_scores`, but the first missing `selected` field returns `Ok(Vec::new())`, so the ADR-042 fields are never consulted.
- `crates/khive-db/src/stores/event.rs:933` through `crates/khive-db/src/stores/event.rs:937` tests a synthetic `"selected": [uuid]` payload instead of the ADR-042 `final_scores` tuple payload.

Why this blocks: real ADR-042 rerank events will insert candidate rows but no selected rows. That breaks `EventFilter.selected`, provenance-aware folds, and the cluster's observable event payload contract while the current tests still pass.

Fix: make the decoder event-kind-specific. For `RerankExecuted`, parse `final_scores` as ordered `[id, score]` tuples for `Selected` rows and keep `candidates` as input candidate rows; add a regression test using the exact ADR-042 payload shape.

### High - The EventView consumer contract is implemented only as a synthetic empty dispatch hook

Evidence:
- `docs/adr/ADR-041-event-provenance-projection.md:222` through `docs/adr/ADR-041-event-provenance-projection.md:241` define `EventView` as the fold consumer surface and require runtime fetch of the event row plus matching `event_observations` before invoking `on_event`.
- `docs/adr/ADR-041-event-provenance-projection.md:584` through `docs/adr/ADR-041-event-provenance-projection.md:589` require `PackEventConsumer::on_event(&EventView)`.
- `crates/khive-runtime/src/pack.rs:30` through `crates/khive-runtime/src/pack.rs:35` exposes only `DispatchHook::on_dispatch(&EventView)`.
- `crates/khive-runtime/src/pack.rs:538` through `crates/khive-runtime/src/pack.rs:549` synthesizes an audit event and constructs `EventView { observations: Vec::new() }`; there is no persisted event lookup or JOIN with `event_observations`.
- `crates/khive-pack-brain/src/lib.rs:316` through `crates/khive-pack-brain/src/lib.rs:318` still documents a synthesized event, and `crates/khive-pack-brain/src/lib.rs:334` folds only `&view.event`.

Why this blocks: the raw `&Event` signature is gone, but the ADR-041 consumer semantics are not present. Consumers never receive persisted provenance observations through a real `PackEventConsumer::on_event` path, so the cluster only partially addresses F216.

Fix: add the actual event consumer delivery path required by ADR-041, fetch `(event, observations)` from storage before invoking consumers, and update brain/fold tests to assert non-empty provenance reaches a consumer for a projected event.

## What Looks Correct

- `crates/khive-runtime/src/operations.rs:232` through `crates/khive-runtime/src/operations.rs:239` now takes `&NamespaceToken` and passes `EventFilter` directly to storage, matching current ADR-022 wording.
- `crates/khive-db/src/stores/event.rs:994` covers deterministic event ordering by `created_at DESC, id DESC`.
- Storage-level tests cover several new filters (`kind`, `session_id`, `observed`, `selected`, `payload_proposal_id`), but the public handler and ADR-042 payload shape are not covered.

## Commands Run

Exact prompt commands from the repository root:
- `cargo fmt --all -- --check 2>&1 | tail -5`: failed because `/Users/lion/khive-work/worktrees/adr-001-015-alignment-impl-c06` has no root `Cargo.toml`.
- `cargo check --workspace 2>&1 | tail -10`: failed with Cargo exit 101 for the same missing root manifest.
- `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20`: failed with Cargo exit 101 for the same missing root manifest.
- `cargo test --workspace 2>&1 | tail -30`: failed with Cargo exit 101 for the same missing root manifest.

Equivalent workspace-manifest commands:
- `cargo fmt --manifest-path crates/Cargo.toml --all -- --check`: passed.
- `cargo check --manifest-path crates/Cargo.toml --workspace`: passed.
- `RUSTC_WRAPPER= cargo clippy --manifest-path crates/Cargo.toml --workspace --all-targets -- -D warnings`: passed. The same command without clearing `RUSTC_WRAPPER` failed because `sccache` could not run in this sandbox.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml --workspace`: passed.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml -p khive-types -p khive-storage -p khive-db -p khive-runtime`: passed.
- `RUSTC_WRAPPER= make ci`: passed, including Rust tests, contract tests, Deno tests, and smoke tests.
- `git diff --check show/adr-001-015-alignment/integration...HEAD`: passed.

## Re-Review Guidance

Re-review should focus first on the migration ledger split and the public `list(kind="event")` handler surface. After those are fixed, add an ADR-042-shaped rerank event regression test and an EventView consumer test that proves projected observations reach a consumer.

Domain utility: SKIPPED - lore suggest/compose tools were not available in this session; review used the local ADR corpus and khive review skill.

VERDICT: REJECT
