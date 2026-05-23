# ADR-078: Multi-Engine Embedding Architecture — Umbrella

**Status**: proposed (umbrella — concrete decisions split into ADR-081/082/083/084 on
2026-05-22)\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-058 (Fold Cognitive Primitives), ADR-061 (Retrieval Infrastructure)\
**Partially supersedes**: ADR-013 §"What v0.1 ships" (single-model embedding row), ADR-057
§"Why project config beats global config" (single-model invariant), ADR-061 §1 (single-
embedder assumption in `khive-runtime`)\
**Related**: ADR-076 (kkernel/MCP split) — engine registry is constructed in `kkernel` once at
boot and shared across packs; ADR-079 (pack-scoped backends) — runtimes per pack consume
filtered engine views; ADR-080 (SubstrateCoordinator) — coordinator does not generate
embeddings (callers pre-compute) but the per-(model, dim) vector tables it routes are shaped
by this ADR\
**Split into**: ADR-081 (Embedder trait + Registry), ADR-082 (Engine TOML config), ADR-083
(Runtime API change), ADR-084 (Pack multi-engine orchestration)

## Context

khive-internal (the predecessor codebase, archived at
`.khive/archive/khive-internal/`) implemented a multi-engine embedding system: multiple peer
embedding models indexed in parallel, queried in parallel, fused with per-model weights.
`deploy/engine.toml` was the canonical schema; `apps/cli/src/server/unified.rs:414` was the
canonical parser.

The current open-core khive collapsed this to single-model:

```rust
// crates/khive-runtime/src/runtime.rs (current — regression site)
pub struct RuntimeConfig {
    pub embedding_model: Option<EmbeddingModel>,        // ← one
}
pub struct KhiveRuntime {
    embedder: Arc<OnceCell<Arc<dyn EmbeddingService>>>, // ← one
}
// embedder() constructs NativeEmbeddingService::with_model(model) — pinned
// vector_search() / hybrid_search() take no model parameter — assumes singleton
```

This is a regression. Multi-engine is not an aspiration — it shipped, was tuned (Chinese
discrimination crisis 2026-03-26, mE5 migration), and was retained as a core design property
through every subsequent refactor. The 2026-03-26 summary recorded the next step explicitly:
_"Multi-index architecture: engine.toml + code supports `Vec<EmbedModelConfig>`. Add Qwen3 as
peer model after HNSW namespace split."_ The open-core port erased this without an ADR.

### What "multi-engine" means here

Distinct from multi-tenant (namespace isolation per ADR-007) and distinct from model
migration (one model at a time, swap atomically per ADR-040). Multi-engine means:

- **N peer embedding services run concurrently** in the same process
- Each service can be a different provider (lattice-embed native, OpenAI API, custom)
- Each service has its own vector index (separate sqlite-vec table + HNSW)
- Every write embeds with all N services and stores in all N indices
- Every query embeds with all N services and searches all N indices in parallel
- Results merge via weighted RRF using per-service weight
- Per-service score normalization (noise_floor, max_similarity, threshold) calibrates
  cross-service comparability

Why: different models have complementary recall. BGE excels at English semantics; mE5 covers
Chinese/multilingual; MiniLM is fast and cheap; Qwen3 is strongest but heaviest. No single
model dominates; running peers in parallel is empirically the highest-quality configuration
the project has measured. Operationally, peer models also provide engine-failure
isolation — remote-API model outage doesn't take down search.

### Why this is "engine" not "model"

A model is an `EmbeddingModel` enum variant (BGE small, Qwen3 0.6B, etc.) — a specific
weights file with a specific dimensionality. An engine is a complete embedding service: trait
implementation + model handle + cache + concurrency policy + provider semantics (local
inference vs. HTTP API). Two engines can implement the same model (local BGE vs. hosted
BGE); the same engine can serve multiple models (a single `NativeEmbeddingService` instance
accepts a `model: EmbeddingModel` per call, per its trait signature).

The ADR name uses "engine" because that's the substitutable unit and the failure-isolation
boundary. Sub-ADRs use "model" or "engine" depending on which is more precise at that layer.

## Decision — umbrella scope

Three independent layers, each with a single concern, split across four ADRs:

```
┌──────────────────────────────────────────────────────────────────────────┐
│  apps / MCP layer (kkernel binary, khive CLI)            (ADR-082)       │
│  - Reads engine TOML at startup                                          │
│  - Constructs N EmbeddingService instances per [[engines]] array        │
│  - Holds Arc<EmbedderRegistry> with names, weights, scoring params       │
│  - Per-pack filter() applied at pack construction (ADR-081 D8)          │
└──────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌──────────────────────────────────────────────────────────────────────────┐
│  packs (pack-memory, pack-kg, etc.)                       (ADR-084)      │
│  - Receive Arc<EmbedderRegistry> (filtered) at construction              │
│  - At write: registry.embed_document_all → Vec<(EngineConfig, Vec<f32>)> │
│  - At query: registry.embed_query_all → fan-out search via runtime       │
│  - Fuse per-engine results via khive_fusion (weights from registry)      │
└──────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌──────────────────────────────────────────────────────────────────────────┐
│  khive-runtime — storage + dispatch + CRUD                (ADR-083)      │
│  - No embedder field. No lattice-embed dep.                              │
│  - vector_search(ns, model_id, query_vec, top_k, kind) — model_id routes │
│    to the correct sqlite-vec table; no embedding generation here         │
│  - Multiple vector tables coexist, one per (model_id, dim) pair          │
│  - hybrid_search likewise takes model_id; fan-out is the caller's job    │
└──────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌──────────────────────────────────────────────────────────────────────────┐
│  khive-embed — trait + registry + lattice adapter         (ADR-081)      │
│  - Embedder trait, EmbedderRegistry, EngineConfig                        │
│  - LatticeEmbedder adapter behind feature "lattice" (default)            │
│  - filter() returns Arc<EmbedderRegistry-subset> sharing engine Arcs     │
└──────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌──────────────────────────────────────────────────────────────────────────┐
│  khive-storage / khive-db                                                │
│  - vectors_for_namespace(model_id, dims, ns) — per-(model, dim) tables   │
│    via vec_model_key. No schema change needed.                           │
└──────────────────────────────────────────────────────────────────────────┘
```

Embedding is the apps/pack layer's responsibility. Runtime is downstream of pre-computed
embeddings — it stores by `(namespace, model_id, dim)` and searches by the same key.

### Where each decision lives

| Concern                                                                                                                 | Sub-ADR                                                      | One-line scope                        |
| ----------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------ | ------------------------------------- |
| `Embedder` trait + `EmbedderRegistry` + asymmetric retrieval + D8 (shared via Arc, filtered per pack) + lattice adapter | [ADR-081](ADR-081-embedder-trait-and-registry.md)            | Provider-agnostic embedding contract  |
| `[[engines]]` TOML schema + user/project override + vector table naming + single-engine fallback                        | [ADR-082](ADR-082-engine-configuration-schema.md)            | Operator-facing configuration surface |
| `KhiveRuntime` drops embedder; `vector_search`/`hybrid_search`/`upsert_vector` take `model_id` + pre-computed vector    | [ADR-083](ADR-083-runtime-api-caller-computed-embeddings.md) | Runtime API change                    |
| Pack handlers fan out across engines, per-engine score normalization, weighted RRF                                      | [ADR-084](ADR-084-pack-multi-engine-orchestration.md)        | Pack-level orchestration pattern      |

## Alternatives Considered (umbrella-level)

These alternatives argue against the multi-engine architecture itself. Sub-decision
alternatives (registry placement, single multi-model service, retrieval crate owns fan-out,
global single index) live in the relevant sub-ADR.

### A. Defer multi-engine — ship single-model first

Pros: smaller scope, faster to land. Cons: this is what already happened in the open-core
port, and the multi-engine code path needs to land before any verb consumer can be migrated
to use it. Deferring is what got us into this regression.

Rejected. The current single-model implementation is the regression we're fixing.

### B. Run multi-engine as a separate service / sidecar

Spawn a dedicated embedding-server process that packs talk to over IPC. Pros: process
isolation; can scale embeddings independently. Cons: per-call IPC cost on every retrieval
query; conflicts with khive's "single MCP daemon, in-process" model; ADR-076's kkernel/MCP
split is the right level of separation. Embedding lives in-process.

Rejected. RuVector's federation pattern is a different concept (cross-cluster, not
cross-process within the same machine).

## Consequences (umbrella-level)

Detailed consequences live in each sub-ADR. Umbrella-level:

### Positive

- **Multi-engine restored** — the design property documented in khive-internal returns to
  open-core; engine TOML schema preserved verbatim
- **Provider-agnostic** — `Embedder` trait permits OpenAI/Cohere/custom implementations
  without modifying runtime or storage (ADR-081)
- **Runtime decoupled from lattice-embed** — `khive-runtime/Cargo.toml` drops the direct
  dep; binary size + dependency surface shrink (ADR-083)
- **Failure isolation** — outage of one engine doesn't take down search; remaining engines
  continue (ADR-083 + ADR-084)
- **Asymmetric retrieval correctness** — E5/Qwen3 prefixes handled at the registry boundary
  (ADR-081)
- **Calibration knobs preserved** — per-engine `noise_floor`/`max_similarity`/`threshold`
  /`weight` survive the port (ADR-082)
- **Pack autonomy** — different packs run different scoring on the same multi-engine
  candidate set (ADR-084)

### Negative

- **N× embedding cost per query/write** — mitigated by parallel embedding + per-engine cache
- **N× storage per write** — N vector tables; tolerable for research KGs; future ADR may
  introduce write-policy allowlist (ADR-082 OQ-2)
- **Configuration burden** — users must think about which engines to run; mitigated by
  single-engine fallback in ADR-082 + default config in ADR-079
- **Pack handler complexity** — recall/search handlers grow ~50 LOC for fan-out (ADR-084)

### Neutral

- **`khive-retrieval` unchanged** — trait surface unaffected; adapters consume per-engine
  tables instead of a singleton
- **`khive-fusion` unchanged** — `FusionStrategy::Weighted` already supports multi-engine
  fusion
- **`khive-fold` / objectives unchanged** — operate on the candidate set after fusion
- **MCP wire protocol unchanged** — multi-engine is internal to handlers

## Migration Plan

The umbrella's three phases (originally Phase A / B / C) map onto the sub-ADRs:

| Plan phase | Sub-ADR           | Description                                                       |
| ---------- | ----------------- | ----------------------------------------------------------------- |
| Phase A    | ADR-081           | New `khive-embed` crate; no behavior change                       |
| Phase B    | ADR-083           | Runtime API change (single-engine still, but callers pre-compute) |
| Phase C    | ADR-082 + ADR-084 | Enable multi-engine in config + pack fan-out                      |

Implementation details are in `.khive/notes/plans/plan_20260522_runtime_restoration.md`.
Each phase ships independently and leaves the build green.

## Open Questions (umbrella-level)

Sub-ADRs own their own open questions. Umbrella-level:

1. **`khive-embed` crate placement.** Resolved: platform layer for v1 (single crate with
   feature flag); refactor to foundation-layer adapter pattern if non-lattice provider
   ships. Tracked in ADR-081.

## References

### khive-internal evidence

- `foundation/embed/DESIGN.md` — 10 models, MRL, two-layer cache, asymmetric retrieval
- `foundation/embed/src/model.rs` — `EmbeddingModel`, `ModelConfig`, `EmbeddingKey` bridge
- `deploy/engine.toml` — canonical multi-engine TOML being restored
- `apps/cli/src/server/unified.rs:414-664` — `resolve_embed_models`, `lore_storage_backend`,
  `backend_for`, `lore_backend_for`, multi-engine + multi-backend wiring
- `platform/db/src/backend/mod.rs:54-59` — `StorageBackend`
- `platform/service/src/backend.rs:420` — `ServiceBackend.extra_vectors`
- `.khive/notes/summaries/summary_20260326_165542_recall_overhaul_multi_index_architecture.md`
- `.khive/notes/summaries/summary_20260428_153434_mcp_qa_audit_short_ids_embedding_fix_email_sync.md`
- `.khive/archive/engine_v1/src/config.rs:79-285` — `EmbedModelConfig` V1 shape

### Current open-core evidence

- `crates/khive-runtime/src/runtime.rs:18-238` — single-backend, single-embedder regression

### Cross-references

- ADR-013 — retrieval port scope; superseded for single-model assumption (this umbrella)
- ADR-040 — embedding model migration; multi-engine subsumes migration as a special case
  (add new engine, deprecate old, remove when reads cease)
- ADR-057 — CLI configuration; this umbrella extends to multi-engine
- ADR-061 — retrieval infrastructure; multi-engine fits as a registry of Objective inputs
- ADR-076 — kkernel/MCP split; registry constructed in kkernel
- ADR-079 — pack-scoped backends; per-pack engine filter applied per ADR-081 D8
- ADR-080 — SubstrateCoordinator; coordinator does not generate embeddings (callers
  pre-compute) but per-(model, dim) table sharding from this ADR family enables it
- ADR-081 — Embedder trait + Registry sub-ADR
- ADR-082 — Engine TOML config sub-ADR
- ADR-083 — Runtime API change sub-ADR
- ADR-084 — Pack multi-engine orchestration sub-ADR
