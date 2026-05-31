# Issue Sweep: DB Cleanup — Status

Generated: 2026-05-31 (implementer op3)

## khive Stats (at op start)

khive MCP unavailable during this session (tool blocked). khive verbs logged below reflect
upstream ops (analyst a1) as reported in fix_brief.md.

## Per-Issue Table

| issue# | root cause file:line | fix applied | test name + result | status |
|--------|---------------------|-------------|-------------------|--------|
| #540 | `crates/khive-runtime/src/pack.rs:684,689,691,696,706,725,734,742,785,792,1660,1711,1813,1889,1917,1923,1931,2136,2425,2445,2457,2514,2596,2801,2806,2838,2850,2880`; `crates/khive-gate/src/lib.rs:199,207,209,394,443` | Replaced ADR-029/033/035 with ADR-018; EventStore substrate rows also cite ADR-004/ADR-005. 37 citation rows, docs-only. Commit `ea79bdd`. | docs-only — no test needed; `cargo check --workspace` PASS | **RESOLVED** |
| #550 | `crates/khive-db/src/stores/vectors.rs:184-579` (939 LOC SqliteVecStore), `crates/khive-db/src/backend.rs:258-370` (vectors_for_namespace), `crates/khive-db/src/extension.rs:1-42` (ensure_extensions_loaded). sqlite-vec is the **primary** VectorStore path, not a fallback. No HNSW/Vamana DB-level replacement exists. ADR-030 requires `kkernel migrate-vectors` which is absent. | Not implemented — fails contained gate (8-11 files, 500-900 LOC, migration tooling required). Proposal recorded below. | N/A | **SKIPPED** (scope exceeds contained gate; migration tooling required per ADR-030) |

## #540 — Root Cause Detail

ADR-029/033/035 are v1 ADR numbers. ADR-018 is the current authorization-gate contract; ADR-004/ADR-005 cover the EventStore audit substrate. All 37 stale citation rows identified by explorer (e1) and mapped by analyst (a1) in `fix_brief.md` were replaced.

Verification command (post-fix):
```bash
rg -n "ADR-0(29|33|35)" \
  crates/khive-runtime/src/pack.rs \
  crates/khive-gate/src/lib.rs \
  crates/khive-gate-rego || true
# Expected: no output (confirmed CLEAN)
```

cargo check output: `Finished dev profile [unoptimized + debuginfo] target(s) in 17.31s`

## #550 — Contained Gate Failure + Proposal

### Why Skipped

| Gate | Required | Observed |
|------|----------|----------|
| File count | <5 | 8-11 files across db/runtime/retrieval/knowledge/kkernel |
| LOC delta | <200 | vectors.rs alone is 939 LOC; replacement estimated 500-900 LOC |
| No migration tool | Not needed | ADR-030 explicitly requires `kkernel migrate-vectors` or auto-rebuild; absent from `crates/kkernel/src/main.rs:42-69` |

sqlite-vec is the primary path: `StorageBackend::vectors_for_namespace` (backend.rs:258-370) constructs `SqliteVecStore` directly; HNSW count in `crates/khive-db/src` = 0.

### Proposal For Future Work

A dedicated retirement branch must implement at minimum:

| Target | Required change |
|--------|-----------------|
| `crates/khive-db/Cargo.toml:11,29,39-41` | Remove sqlite-vec dep/feature after replacement compiles |
| `crates/khive-db/src/extension.rs:1-42` | Delete or reduce sqlite-vec auto-extension hook |
| `crates/khive-db/src/backend.rs:44-63` | Remove `ensure_extensions_loaded()` calls once vector factory no longer needs vec0 |
| `crates/khive-db/src/backend.rs:235-370` | Replace `vectors()` + `vectors_for_namespace()` with HNSW/Vamana-backed factory |
| `crates/khive-db/src/stores/vectors.rs:1-939` | Replace `SqliteVecStore` with non-sqlite-vec impl |
| `crates/khive-db/src/stores/mod.rs:7` | Keep `vectors` module name to reduce caller churn |
| `crates/khive-runtime/src/retrieval.rs:270-274` | Update KNN docs promising sqlite-vec brute-force exact cosine |
| `crates/khive-pack-knowledge/src/knowledge/vamana.rs:256-303` | Stop raw SQL scanning `vec_{model}` tables; use new vector corpus abstraction |
| `crates/kkernel/src/main.rs:42-69` + new command module | Add `kkernel migrate-vectors` (ADR-030 requirement) |
| `crates/kkernel/src/vector.rs:92-138` | Update capability reporting that names `SqliteVecStore` |
| `crates/khive-db/tests/contract/vector_filter.rs:1-185` | Replace sqlite-vec contract expectations with HNSW behavior |
| `crates/khive-retrieval/Cargo.toml:47-49` + `adapters/mod.rs:13,84` | Remove sqlite-vec feature coupling |

**Guardrails (non-negotiable):**
- Do NOT delete sqlite-vec data or `vec_*` tables in existing user databases
- Do NOT edit V1 or historical migration entries in `migrations.rs`
- Migration/rebuild must be explicit, idempotent, and safe to rerun
- Existing V14/V16/V17 migration entries must remain available for existing DBs

**Estimated scope:** ~600-900 LOC delta, ~10 files, requires 1 new command module.

## khive Usage Report

| Layer | Verbs called | What mattered |
|-------|-------------|---------------|
| explorer (e1) | `stats`, `memory.recall`, `search`, `knowledge.search`, `get` | Prior finding `75af832d` confirmed sqlite-vec is primary path — validated analyst hypothesis before analyst wrote brief |
| analyst (a1) | `memory.recall`, `stats`, `knowledge.suggest`, `knowledge.search`, `search`, `get` | Domain search reinforced migration risk. Memory recall `75af832d` matched explorer finding. Wrote decision `0fa8d9b3`, memory note `03edaf69`, link `a08bd5fd`, brain feedback `01d31ffc`. |
| implementer (i1/this op) | khive MCP unavailable — tool blocked during this session | Upstream recalls (analyst `03edaf69`, `0fa8d9b3`) were read from `fix_brief.md` and informed scope decisions |

**Memory/proposal IDs from upstream (a1):** memory `03edaf69`, decision `0fa8d9b3`, link `a08bd5fd`
