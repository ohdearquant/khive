# ADR-083: Runtime API — Caller-Computed Embeddings, `model_id` Routing

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-005 (Storage Capability Traits), ADR-081 (Embedder trait + registry)\
**Partially supersedes**: ADR-061 §1 (single-embedder assumption in `khive-runtime`)\
**Part of**: ADR-078 (Multi-Engine Embedding umbrella)

## Context

The open-core port collapsed multi-engine to single-model partly because `khive-runtime` owned
the embedder:

```rust
// crates/khive-runtime/src/runtime.rs (current, regression site)
pub struct RuntimeConfig {
    pub embedding_model: Option<EmbeddingModel>,        // ← one model
}
pub struct KhiveRuntime {
    embedder: Arc<OnceCell<Arc<dyn EmbeddingService>>>, // ← one service
}
// vector_search() / hybrid_search() take no model parameter
// embedder() constructs NativeEmbeddingService::with_model(model) — pinned
```

ADR-078's umbrella decides that embedding generation belongs in the caller, not the runtime.
This ADR locks the **runtime API shape that makes that work** — runtime takes pre-computed
vectors plus a `model_id` parameter for table routing; it no longer constructs or owns
embedders.

## Decision

### Drop embedder ownership from `KhiveRuntime`

```rust
// crates/khive-runtime/src/runtime.rs (proposed)
pub struct RuntimeConfig {
    pub data_path: Option<PathBuf>,
    pub namespace: String,
    // embedding_model: REMOVED — runtime has no model awareness
}

pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    embedders: Arc<EmbedderRegistry>,   // per ADR-081; runtime holds the registry
                                         // but does NOT generate embeddings — see below
    // embedder: REMOVED (no OnceCell, no per-runtime model singleton)
}
```

The runtime still holds an `Arc<EmbedderRegistry>` for **metadata access** (resolving
`model_id` → `EngineConfig`, looking up `dim`, etc.). It does **not** invoke `embed*()` on
that registry — that's the caller's job (per ADR-084 for packs). The registry is on
`KhiveRuntime` because (a) the per-pack filter (D8) is naturally applied at pack
construction, which is also where the runtime is constructed; (b) pack handlers reach the
registry through `runtime.embedders()`.

### New retrieval method signatures

Every retrieval method that touches a vector table gains a `model_id: &str` parameter (for
table routing) and accepts pre-computed vectors:

```rust
impl KhiveRuntime {
    /// Vector search against the table identified by `model_id`. Caller has
    /// already computed `query_vec` using the engine they want.
    pub async fn vector_search(
        &self,
        namespace: Option<&str>,
        model_id: &str,                   // routes to vec_{snake_case(model_id)} table
        query_vec: Vec<f32>,              // caller pre-computed via the registry
        top_k: u32,
        kind: Option<SubstrateKind>,
    ) -> RuntimeResult<Vec<VectorSearchHit>>;

    /// Hybrid search — FTS5 keyword path + vector path. Caller supplies both
    /// query text (for FTS5) and pre-computed query vector (for vector path).
    pub async fn hybrid_search(
        &self,
        namespace: Option<&str>,
        model_id: &str,
        query_text: &str,
        query_vec: Vec<f32>,
        strategy: Option<FusionStrategy>,
        limit: u32,
    ) -> RuntimeResult<Vec<SearchHit>>;

    /// Write a pre-computed vector to the per-model vector table.
    pub async fn upsert_vector(
        &self,
        namespace: Option<&str>,
        model_id: &str,
        entity_id: Uuid,
        vector: Vec<f32>,
    ) -> RuntimeResult<()>;

    /// REMOVED methods:
    /// - embed(text) — runtime doesn't generate embeddings
    /// - embed_batch(texts) — same
    /// - embedder() — runtime doesn't own a single embedder
}
```

### Storage layer is unchanged

The per-(model, dim) vector table sharding already exists in `khive-db` via
`vectors_for_namespace(model_key, dim, namespace)` (per ADR-005 storage capability traits).
The `model_id` parameter passed to `vector_search` is the same identifier consumed by the
existing storage trait. **No storage schema change** is required by this ADR — the migration
work is in the runtime layer.

### Migration shim for existing single-model deployments

A deployment that predates ADR-082's `[[engines]]` array has data in a `vec_default` table
(the legacy table name from when the runtime had one model). At the boot path:

1. If `[[engines]]` is empty/missing, fall back to the built-in default engine (per ADR-082)
   with `model_id = "bge-small-en-v1.5"`.
2. If the database has a `vec_default` table but no `vec_bge_small_en_v1_5` table, run a
   one-time migration: rename `vec_default` → `vec_bge_small_en_v1_5`.
3. Subsequent writes go to `vec_bge_small_en_v1_5` (the model_id-prefixed table).
4. If both tables exist (unexpected), prefer `vec_bge_small_en_v1_5`; log warning about
   `vec_default`.

This migration is idempotent and runs once at first startup post-ADR-083 implementation.

### Runtime drops its `lattice-embed` dependency

`khive-runtime/Cargo.toml` no longer depends on `lattice-embed`. The dependency moves to
`khive-embed/Cargo.toml` (per ADR-081). Runtime depends on `khive-embed` for `EmbedderRegistry`
(metadata access) — but the lattice dependency is transitive through `khive-embed`'s feature
flag, not direct.

Binary-size impact: `khive-runtime` consumers that don't need embedding (e.g., a future
SQL-only consumer) can disable `khive-embed`'s `lattice` feature and avoid pulling
lattice-embed transitively.

## Why the runtime still holds the registry

A reasonable alternative is: runtime holds zero embedding state; the registry lives only at
the pack handler. We considered it. The trade-off:

| Runtime holds registry                                                                                | Pack handler holds registry directly                |
| ----------------------------------------------------------------------------------------------------- | --------------------------------------------------- |
| `runtime.embedders()` gives pack one accessor                                                         | Pack constructor takes registry separately          |
| Filter applied at pack construction                                                                   | Filter applied at pack construction (same)          |
| Runtime needs the registry for nothing except metadata access (kind validation, table key derivation) | Runtime never sees the registry                     |
| Test fixture: `KhiveRuntime::memory()` accepts empty registry default                                 | Test fixture: each pack must inject a registry mock |

We chose "runtime holds registry" because it keeps the pack constructor signature simple
(one runtime parameter, not two) and because the registry is a natural property of the
runtime's deployment context. The runtime does not gain any embedding-generation capability
— `runtime.embed()` does not exist after this ADR. The registry is read-only metadata from
the runtime's perspective; the only method that consumes engine semantics
(`registry.embed_query_all()`) is called by pack handlers, not by the runtime.

## Engine failure semantics — open question

When a caller's `embed_query_all()` call against the registry returns partial results (one
engine succeeded, another network-timed-out), what should the runtime do? Two options:

- **(a) Fail-fast**: any engine error fails the whole operation. Strict consistency; predictable.
- **(b) Skip-and-continue**: engines that errored are skipped; remaining engines proceed.
  Availability over consistency.

khive-internal's pattern was (b) with per-engine failure counters surfaced as observability.
v1 inherits (b) — the caller's `embed_query_all` returns a partial list; if no engines
succeeded, the caller fails the request. If at least one engine succeeded, the search
proceeds with whatever it has. This decision is **at the registry layer** (ADR-081),
inherited here.

If operators need strict-consistency behavior, a future ADR may introduce
`registry.embed_query_all_strict()` that returns Err on any per-engine failure.

## Alternatives Considered

### A. Runtime owns the registry AND exposes `embed*()` for back-compat

Keep `runtime.embed(text) -> Vec<f32>` as a convenience wrapper around
`runtime.embedders().embed_one(primary, text)`. Considered for migration ergonomics.
Rejected: this is the abstraction inversion we're removing. Convenience wrapper invites
callers to forget which engine they used, breaking multi-engine semantics. Explicit > implicit.

### B. Runtime takes `Vec<(model_id, Vec<f32>)>` for multi-engine writes

`upsert_vectors(entity_id, vectors: Vec<(model_id, Vec<f32>)>)` writes to all per-engine
tables in one call. Considered for ergonomics. Rejected: callers loop anyway (per ADR-084),
and the single-engine signature keeps the trait surface narrow. Multi-engine fan-out is
caller responsibility.

### C. Runtime constructs embedders lazily on first call

Keep the OnceCell pattern but accept an engine list. Rejected: re-introduces the embedding
generation responsibility in the runtime. Whole-point regression.

### D. Generic `vector_search<E: Embedder>` taking the embedder as type parameter

Generic over embedder; runtime calls `E::embed()` internally. Rejected: forces caller to
specify embedder at the type level; OpenAI vs BGE diversity becomes a generics problem;
incompatible with the `Arc<dyn Embedder>` registry shape.

## Consequences

### Positive

- Runtime is single-purpose — store/query, no embedding generation
- `khive-runtime/Cargo.toml` no longer depends on `lattice-embed` directly
- Multi-engine fan-out is naturally caller-side (pack handlers control which engines run)
- Engine-failure isolation: one engine's outage doesn't kill the search if at least one
  succeeded
- Cleaner test surface: runtime tests don't need to mock or load an embedder
- Schema unchanged: no migration of `vec_*` tables (other than one-time rename of
  `vec_default` → `vec_bge_small_en_v1_5`)

### Negative

- Every retrieval call-site changes signature — `model_id` + `query_vec` instead of
  computed-inline. Migration touches every consuming pack handler (per ADR-084). Mitigated:
  mechanical change, one verb at a time.
- The "primary engine" convention (`engines()[0]`) is the implicit default for single-engine
  paths; ADR-081 OQ-1 applies here.
- Migration shim for `vec_default` table is a one-time complexity at boot; documented
  clearly in operator guide.

### Neutral

- `khive-storage` is unchanged — the `VectorStore` trait already takes `model_id` indirectly
  via the per-table store handle
- The `model_id` parameter is the same string `EmbedderRegistry::engines()[i].config.name`
  produces — no separate identifier surface
- Hybrid search keeps FTS5 + vector parity; the FTS5 side doesn't need `model_id`

## Open Questions

1. **Should `vector_search` accept `Option<Vec<f32>>` for a graceful FTS5-only fallback?**
   Today: caller pre-computes vector; if they want FTS5-only, they call `text_search`
   directly. Considered API ergonomics; rejected to keep the surface narrow. Hybrid_search's
   FusionStrategy::VectorOnly / FusionStrategy::TextOnly handles the modality choice.
2. **Per-call timeout for slow embedders?** Today: caller's `embed_query_all` has whatever
   timeout the embedder's underlying service enforces. A coordinator-level timeout (cancel
   slow engines, proceed with the rest) is a future enhancement. v1: rely on per-engine
   service-level timeout config.
3. **Atomic multi-vector upsert?** Today: caller loops `upsert_vector(model_id_a)` and
   `upsert_vector(model_id_b)`. If the second fails, the first has already written. v1
   acceptable; if it becomes a problem, runtime can add a transactional batch method.

## References

- ADR-005 — Storage Capability Traits (`VectorStore::vec_model_key`)
- ADR-061 — Retrieval Infrastructure (single-embedder assumption being superseded here)
- ADR-078 — Multi-engine embedding umbrella
- ADR-081 — `Embedder` trait + `EmbedderRegistry`
- ADR-082 — Engine TOML config + table naming (`model_id` strings consumed here)
- ADR-084 — Pack multi-engine orchestration (consumer of the new runtime signatures)
- khive-internal `apps/cli/src/server/unified.rs:414-664` — historical wiring
- `crates/khive-runtime/src/runtime.rs` lines 18-238 — current regression site
