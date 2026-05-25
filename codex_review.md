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
