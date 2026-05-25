# v022-polish Issue Closure Log

## Summary

- Closed: 0 issues
- Skipped: 15 issues
- Need follow-up: 0 new issues to file

The target of ≥12 closures was **not met**. Verification found that none of the 15 wiring-category
issues has the required combination of (a) a specific resolving commit SHA, (b) a file:line proof
that the original condition is fixed, and (c) no remaining partial/deferred work in the same scope.
Three issues (#397, #385, #380) are closest to resolved but still have explicitly-deferred code paths.

## Closed Issues

_None. No `gh issue close` commands were executed._

## Skipped Issues

| Issue # | Title | Reason |
|---------|-------|--------|
| #397 | [adr-031] EmbedderRegistry implementation (integration codex MAJ-3) | Partial only. `crates/khive-runtime/src/runtime.rs:138-139` still marks per-pack `EmbedderRegistry` plumbing as future work; legacy `embedding_model` field remains; pack-extensible registry trait and registration pattern absent. |
| #392 | [c22 follow-up] ProposalApplyWorker + ProposalApplied emission | Event types exist but `crates/khive-pack-kg/src/lib.rs:206-208` dispatches only `propose/review/withdraw`; `handlers.rs:1926-1955` approves inline with no changeset application worker or `ProposalApplied` emission. |
| #391 | [c22 follow-up] proposals-projection-worker (ADR-046 sec5) | Projection writes remain inline in KG handlers (`handlers.rs:1781-1800`, `:1939-1955`, `:2062-2074`); no `PackEventConsumer` projection worker, expiry update, double-vote semantics, or `ProposalApplied` projection path. |
| #385 | [c20 follow-up] ADR-043 sec8 startup backfill (steps 2-4) | Partial registry/tag plumbing landed but `crates/khive-db/src/migrations.rs:353-380` defers sqlite-vec rebuild/backfill; no `EmbeddingModelChanged` emission found. |
| #380 | [c21 follow-up] engine migrate: EmbedMigrationWorker + actual queueing (ADR-043 D2-D6) | `crates/kkernel/src/engine.rs:187-191` still returns "engine migrate is not yet implemented"; migrate queueing, worker, event emission, and tests absent. |
| #379 | [c09 follow-up] single-endpoint OR still leaf-flattens to AND in variable-length pattern | Bug path unchanged: `crates/khive-query/src/ast.rs:43-57` flattens `And`/`Or`; variable-length WHERE joins with `AND` at `sql.rs:1075,1082`. |
| #376 | memory: implement or remove scaffolded RecallConfig fields (reranker_params, fallback_during_migration) | Scaffold remains: `crates/khive-pack-memory/src/config.rs:23-25,48-51` still declares unreachable fields. |
| #375 | memory: wire recall.rerank into main recall path (ADR-033 sec6) | Main recall still calls `compute_score` directly at `handlers.rs:545-548`; `recall.rerank` remains a stub path with 0.0 scores at `:733-768`. |
| #370 | [c08 follow-up] Pack config from TOML, not env (F155 ADR-027) | Runtime boot still reads env/defaults: `khive-runtime/src/runtime.rs:190` reads `KHIVE_PACKS`; TOML pack loading not implemented. |
| #368 | [c08 follow-up] SubstrateCoordinator D2-D6 implementation (F162 phase 2) | `crates/kkernel/src/coordinator/mod.rs:124-125` explicitly defers D2-D6 locator cache, fan-out search, traversal, and WAL cascade. |
| #367 | [c08 follow-up] Runtime decoupling from khive-db (F014/F049 phase 2) | `crates/khive-runtime/Cargo.toml:18,32` still depends directly on `khive-db` and `rusqlite`. |
| #366 | [c23 follow-up] khive-vcs-adapters: ship at least one Rust impl or remove scaffold | Crate is still a scaffold: `crates/khive-vcs-adapters/src/lib.rs:15-18` exposes only P0 trait/types; no `impl FormatAdapter` found. |
| #364 | [c23 follow-up] F201: run_sync remote archive fetch + hash pin verification | `crates/khive-vcs/src/sync.rs:91-103` reads local files only; remote fetch, `sha256:` pin, `meta.json`, and `--repin` absent. |
| #344 | [deferred] khive-score -> ruvector-core shim migration (post upstream) | Still upstream/deferred; `crates/khive-score/Cargo.toml:16` has no `ruvector-core` dependency. |
| #342 | [follow-up to #336] HNSW: route similarity_from_distance through khive-score canonical layer | HNSW still owns local conversion: `crates/khive-hnsw/src/distance.rs:137`; boundary not moved. |

## Follow-up Needed

Three issues are closest to closure but require additional implementation before they can close:

| Issue # | Title | Missing Work |
|---------|-------|--------------|
| #397 | EmbedderRegistry implementation | Pack-extensible registry trait + registration pattern |
| #385 | ADR-043 sec8 startup backfill | sqlite-vec rebuild/backfill + `EmbeddingModelChanged` emission |
| #380 | EmbedMigrationWorker + actual queueing | Migration worker, queueing, event emission, tests |
