# ADR-041: Pluggable Vector Store — Trait Contract, In-House HNSW, RuVector Attribution

**Status**: proposed\
**Date**: 2026-05-19\
**Authors**: khive maintainers

## Context

khive uses `sqlite-vec` as its only vector backend, wired through the `VectorStore` trait defined
in `crates/khive-storage/src/vectors.rs` (lines 14–28). ADR-005 established the trait-only pattern
for `khive-storage`, and ADR-009 established one-crate-per-backend as the multi-backend strategy.
The architecture is already correct — but only one implementation exists.

The current `VectorStore` trait works at v0.1 scale. At v0.2 scale (100K+ entities, hybrid
retrieval policies, multi-query fan-out) it has measurable limits:

- **No HNSW**: sqlite-vec uses brute-force cosine scan. The `rebuild()` method in
  `SqliteVecStore` (vectors.rs, line 432) is a no-op with a comment confirming this. O(n) per
  query becomes the ceiling.
- **No metadata filter pushdown**: the current `VectorSearchRequest` (types.rs, lines 121–127)
  carries only `namespace` and `kind` as pre-filters. Richer per-field metadata filtering
  requires passing a subquery to the store — not expressible today.
- **No batch search path**: `search()` takes one query vector. HyDE-style multi-query requires
  calling it N times, each paying O(n) scan cost.
- **No quantization**: full f32 precision only. At 100K entities × 384 dims × 4 bytes = 147 MB
  in-process. Product quantization could cut this 8–16×.
- **Single-process boundary**: sqlite-vec lives in the same WAL-locked SQLite file as all note
  and entity data. A separate vector backend relieves write-lock contention at scale.

RuVector (`https://github.com/ruvnet/RuVector`) is the closest existing OSS project to what
khive needs at v0.2 — HNSW, scalar / product / binary quantization, metadata pre-filter
pushdown, batch search, REDB persistence. It is also a young, fast-moving codebase with a
broad dependency graph and lenient internal lint discipline. Adopting it as a Cargo dependency
would couple khive's release cadence and code-quality posture to a project we do not control.

**The decision in this ADR is to treat RuVector as algorithmic and design reference, not as a
Cargo dependency.** khive's production-grade vector backend will be an in-house crate
(`khive-vec-hnsw`) that implements the same family of algorithms using the parameter choices and
API patterns RuVector validated, written in khive's own code style and audited against khive's
own quality bar. Attribution is explicit (inline doc comments at each adopted pattern, a
dedicated section in this ADR), preserving the legitimate claim that khive is influenced by
RuVector's work while keeping our build, lint, and release surface fully under khive control.

This ADR defines the swap-in contract, the in-house implementation crate, and the explicit
attribution model for the RuVector design influences we adopt. It does not implement the
in-house backend; that is a follow-up PR scoped against this contract.

## Decision

**Expand `VectorStore` to a future-proof contract, codify `VectorStoreCapabilities` for
backend introspection, ship a production-grade in-house backend (`khive-vec-hnsw`), and
define runtime backend selection via `RuntimeConfig.vector_store_kind`.**

Five concrete decisions:

1. The `VectorStore` trait gains four new methods: `search_with_filter`, `search_batch`,
   `update`, and `capabilities`. All four are provided as default implementations on the trait.
   `search_with_filter` delegates to `search` only when the filter is empty and returns
   `StorageError::Unsupported` for non-empty filters — callers must check
   `capabilities().supports_filter` and post-filter at the runtime layer when the backend
   lacks native pushdown. `search_batch` calls `search` sequentially; `update` performs
   delete+insert; `capabilities` returns a baseline struct with all optional features disabled.
   Existing backends compile without change.
2. Each backend declares a static `VectorStoreCapabilities` struct that higher-level retrieval
   policy (hybrid search, HyDE fan-out, etc.) can introspect.
3. The ADR-009 one-crate-per-backend pattern applies: a new in-house `khive-vec-hnsw` crate
   houses the production-grade backend (HNSW + quantization + filter pushdown). `khive-db`
   retains the sqlite-vec default. **No external vector library appears in khive's dependency
   graph.** Algorithmic and API patterns adopted from RuVector are reimplemented in khive's
   own code and attributed inline.
4. `RuntimeConfig` gains a `vector_store_kind` field and each backend has a typed builder.
   The runtime yields `Arc<dyn VectorStore>` regardless of which backend is selected.
5. RuVector influence is documented explicitly (inline doc comments at each adopted pattern,
   a dedicated "Influences and Attribution" section in this ADR, a credit line in the
   `khive-vec-hnsw` README). This preserves the legitimate claim that khive draws on
   RuVector's work without taking on RuVector's release surface.

## Rationale

### Why expand the trait now rather than at v0.2 feature time?

Because adding methods to a published trait is a breaking change. v0.2 retrieval features
(hybrid filter, HyDE, re-embed flows from ADR-040) will need these methods. Defining them now
— with default impls that delegate to the base `search` path or return a conservative baseline
— means existing backends don't break, the trait surface stabilises, and new backends can be
built against the complete contract from day one.

The cost of adding a method with a default impl is zero for existing consumers. The cost of
adding a method without a default to a trait already in use is a full-ecosystem breaking change.

### Why not put filter logic entirely in the runtime?

Runtime-side post-filtering (fetch top-K, then discard non-matching results) is correct for
`SqliteVecStore` today because it cannot push predicates into the vec0 virtual table efficiently.
But it wastes bandwidth. Backends that support native filter pushdown (e.g., RuVector's
`ruvector-filter` crate, which appears in the repo as a dedicated crate) should be able to
express that through the trait. The `search_with_filter` method gives them that path. Backends
that cannot push filters return `StorageError::Unsupported` from the default impl when
predicates are non-empty. Runtime retrieval code must check `capabilities().supports_filter`
and post-filter outside the store when native pushdown is absent.

### Why keep sqlite-vec as default?

Zero dependencies, zero config, single-file database. For the core "local research KG" use case,
the current brute-force scan at 100K entities and 384 dims takes roughly 40ms per query — well
within tolerable latency for an interactive research agent. Keeping sqlite-vec as default means
the happy path (install khive, run) works without any additional setup.

The production backend is opt-in. Researchers who need HNSW performance or quantization opt into
`khive-vec-hnsw` — the in-house production-grade backend.

### Why an in-house implementation rather than a dependency?

Three reasons, in order of weight:

1. **Quality-bar control.** External vector libraries iterate fast and frequently relax internal
   lint and audit discipline to do so. khive's other crates compile under `-D warnings`, run
   clippy lints to ground, and treat unsafe as an exceptional case. An in-house implementation
   keeps the production vector path on the same quality bar as the rest of the codebase.
2. **Release-surface independence.** Vector libraries in this space ship breaking changes on
   short cadences. Owning the implementation means the cadence is khive's, not upstream's; a
   khive minor release never has to ship a vector-library minor bump as a forcing function.
3. **Dependency-graph discipline.** Production-grade vector libraries pull in 50–100+ transitive
   crates (HNSW, persistence, SIMD, embedding, RPC layers). Keeping that out of `khive-mcp` and
   downstream consumers preserves the small-binary, low-supply-chain-surface property that the
   v0.1 single-binary release established.

Attribution to the projects that pioneered the algorithms we adopt — most directly RuVector —
is handled in the "Influences and Attribution" section below. The choice not to depend on a
project is independent of giving it credit for the design ideas it validated.

### Why capability introspection rather than trait method checking?

Runtime feature detection via `try_search_with_filter` calls and error-type matching is
error-prone and puts decision logic in the caller. A `VectorStoreCapabilities` struct makes
backend capabilities explicit at construction time, enables compile-time optimisation paths in
the retrieval policy, and matches how other capability-oriented systems (e.g., Vulkan
`PhysicalDeviceFeatures`) communicate optional support.

### Why one crate per backend (reaffirming ADR-009)?

Validated by the ADR-009 analysis (lines 69–84 of that document): separate crates mean separate
dep graphs, independent versioning, and no feature-flag maze. Even for an in-house backend,
keeping HNSW + quantization + REDB persistence in `khive-vec-hnsw` (not `khive-db`) preserves
the property that the default install ships only the SQLite vector path. Users who do not need
HNSW pay no compile-time or binary-size cost.

## Influences and Attribution: RuVector

The in-house implementation in `khive-vec-hnsw` draws explicit design influence from RuVector
(`https://github.com/ruvnet/RuVector`). This section documents what we adopted, what we
deliberately omitted, and how attribution is preserved in code.

The survey below is retained because the design choices RuVector made — HNSW parameter
defaults, scalar/PQ/binary quantization layout, REDB persistence pattern, filter API shape —
are the design choices `khive-vec-hnsw` reuses. Reimplementing without crediting prior art
would be both inaccurate and ungenerous.

### Repository profile (read 2026-05-19)

**Source**: GitHub API — README, `crates/ruvector-core/src/lib.rs`, `types.rs`, `vector_db.rs`,
`Cargo.toml`, and recent commit log. All findings are from primary source.

| Attribute    | Value                                                                                                                                                           |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Stars        | 4,100                                                                                                                                                           |
| Language     | Rust (primary)                                                                                                                                                  |
| Version      | 2.2.2                                                                                                                                                           |
| License      | MIT                                                                                                                                                             |
| Last commit  | 2026-05-19 (active as of this reading)                                                                                                                          |
| Created      | 2025-11-19 (six months old)                                                                                                                                     |
| Rust edition | 2021, `rust-version = "1.77"`                                                                                                                                   |
| Crate count  | 90+ (mono-repo: ruvector-core, ruvector-filter, ruvector-hnsw, ruvector-hyperbolic-hnsw, ruvector-postgres, ruvllm, ruvector-raft, ruvector-gnn, and many more) |

MIT license is compatible with khive's Apache-2.0 (MIT is permissive; Apache-2.0 can include MIT
works). No copyleft concern.

### Algorithms and indexing

From `crates/ruvector-core/src/lib.rs` (the module's own documentation block):

> **Working Features (Tested & Benchmarked)**
>
> - HNSW Indexing: Approximate nearest neighbor search with O(log n) complexity
> - SIMD Distance: SimSIMD-powered distance calculations (~16M ops/sec for 512-dim)
> - Quantization: Scalar (4x), Int4 (8x), Product (8-16x), and binary (32x) compression
> - Persistence: REDB-based storage with config persistence
> - Search: ~2.5K queries/sec on 10K vectors (benchmarked)

From `types.rs` (`HnswConfig` struct, lines 70–89): standard HNSW parameters (`m`,
`ef_construction`, `ef_search`, `max_elements` — default `m=32`, `ef_construction=200`,
`ef_search=100`, `max_elements=10_000_000`).

From `Cargo.toml` features: `hnsw = ["hnsw_rs"]` — HNSW is an optional feature backed by the
`hnsw_rs` crate, not a from-scratch implementation. `storage = ["redb", "memmap2"]` — disk
persistence via `redb` (an embedded key-value store). `simd = ["simsimd"]` — SIMD distance
acceleration. `parallel = ["rayon", "crossbeam"]` — parallel batch ops.

Additional algorithms listed in the README (v2.1.0 features):

- **DiskANN / Vamana** (`ruvector-core`): SSD-backed ANN with LRU page cache, claimed <10ms
  latency at billion scale.
- **OPQ** (Optimized Product Quantization): learned rotation for 10–30% error reduction vs
  standard PQ.
- **ColBERT multi-vector** (`ruvector-core`): per-token MaxSim scoring.
- **Matryoshka embeddings**: adaptive-dimension search.
- **Hybrid search / RRF** (`ruvector-core`): sparse + dense fusion with Reciprocal Rank Fusion.
- **Hyperbolic HNSW** (`ruvector-hyperbolic-hnsw`): Poincaré ball space search.
- **LSM compaction**: log-structured merge for write-heavy workloads.

### Storage model

From `vector_db.rs`: `VectorDB` wraps a storage backend (`redb`-based on disk, or
`MemoryStorage` when `storage` feature is off) and a `Box<dyn VectorIndex>` (HNSW or Flat). The
index is rebuilt from the storage on startup. This is a clean separation of storage and search
index layers.

The `DbOptions` struct (`types.rs`) exposes: `dimensions`, `distance_metric`, `storage_path`,
`hnsw_config: Option<HnswConfig>`, `quantization: Option<QuantizationConfig>`. Straightforward
builder-style configuration.

### Patterns adopted in `khive-vec-hnsw`

The in-house implementation reuses the design shape RuVector validated. Specifically:

- **`HnswConfig` parameter defaults**: `m=32`, `ef_construction=200`, `ef_search=100`,
  `max_elements=10_000_000` — same defaults, same tuning surface. Inline doc comment:
  `// HNSW parameter defaults follow the convention established in RuVector (ruvnet/RuVector).`
- **Storage/index separation**: index is rebuildable from durable storage on startup. We do
  not literally embed RuVector's `VectorDB`; we replicate the architectural separation in our
  own types.
- **Quantization API**: `None | Scalar | Product { subspaces } | Binary` — a closed enum with
  the same shape as RuVector's `QuantizationConfig`. The four quantization paths are textbook;
  RuVector's contribution here is the API shape, which we adopt.
- **Filter API**: a typed `VectorMetadataFilter` (namespaces, kinds, equality predicates) —
  similar in spirit to RuVector's `ruvector-filter::FilterExpression`, narrowed to khive's
  needs (we do not need range/compound predicates for v0.2). The `search_with_filter` method
  shape is independent of any specific implementation; the _idea_ of pushing filters into the
  index scan (vs post-filtering) is the RuVector contribution we adopt.
- **`search_batch` for HyDE / multi-query**: a single method that takes N query vectors and
  amortises the index-walk overhead. RuVector exposes this in its index layer; we add it to
  our trait surface for the same reason.

### Patterns deliberately _not_ adopted

- **DiskANN/Vamana, OPQ, ColBERT multi-vector, Hyperbolic HNSW**: powerful but well past the
  v0.2 scope. Tracked as candidate ideas for a future ANN-research ADR.
- **`ruvector-gnn` / `ruvector-raft` / `ruvllm`**: not in scope for a local-first KG.
- **AgenticDB / placeholder embeddings**: known-incomplete in upstream; embedding is handled
  by `lattice-embed` per ADR-012.
- **REDB as the persistence backing**: we will evaluate REDB vs sqlite-as-blob-store vs raw
  mmap in the implementation PR. REDB is one viable choice, not the only one.

### Maturity signals (why we do not adopt as a dependency)

**Positive**:

- Active: 5 commits on 2026-05-19 alone (supply-chain CI, NAPI binaries).
- Benchmarks exist: 8 named benchmark targets (distance metrics, HNSW search, quantization,
  batch ops, SIMD). Claimed ~2.5K queries/sec on 10K vectors is plausible for HNSW.
- Version 2.2.2 with `CHANGELOG.md` — versioned releases.

**Concerns**:

- **Young codebase**. First commit 2025-11-19. Vector stores typically need years of
  production burn-in to discover data-loss bugs at scale. Depending on a young one would put
  khive's correctness on a tighter clock than we want.
- **Lint posture**: the upstream `Cargo.toml` suppresses a wide set of clippy and rustc
  warnings — a reasonable choice for a project iterating rapidly, but not compatible with
  khive's `-D warnings` policy. An adapter that hides this from khive callers would still
  carry the discipline gap into transitive dependencies.
- **Broad mono-repo**: 90+ crates, many out of scope for a local-first KG (RPC, GNN, Raft).
  Even with feature gating, the dep tree and audit surface is larger than khive needs.
- **Self-reported performance**. Independent verification against khive's actual workload
  would be required before any production claim. Easier to benchmark our own implementation
  on our own data than to validate someone else's claims.

These signals are not criticisms of RuVector as a project; they are the signals that tell us
"adopt as reference, not as dependency." The same signals exist for almost every
fast-iterating OSS vector library; that is the nature of the space right now.

### Attribution mechanics

Where `khive-vec-hnsw` adopts a RuVector design choice, attribution lives in three places:

1. **Inline doc comments at the adoption site**. Example:
   `// Parameter defaults follow the convention established in RuVector (ruvnet/RuVector).`
2. **Crate README** (`crates/khive-vec-hnsw/README.md`): a brief "Influences" section naming
   RuVector and linking to its repository. License: Apache-2.0 (khive) + MIT (RuVector
   patterns) is non-conflicting; no MIT code is copied, only patterns.
3. **This ADR** as the canonical record of the influence relationship.

This pattern matches how `khive-query` documents its borrowings from RDF/SPARQL semantics
(ADR-008) and how `khive-storage` cites the storage capability tradition (ADR-005). Crediting
prior art is part of khive's documentation discipline.

### Optional RuVector bridge (out-of-tree)

Some downstream users may want to plug RuVector directly into khive at deployment time — for
example, to use a specific RuVector index variant we have not yet reimplemented in
`khive-vec-hnsw`. The trait-based architecture supports this _without_ khive taking on the
dependency: any community-maintained crate that implements `VectorStore` can be wired through
`RuntimeConfig.vector_store_kind` and an extension point on the runtime builder.

A reference adapter crate (`khive-vec-ruvector-bridge`, community-maintained, out of the
khive monorepo) can ship under the same ADR-009 sibling-crate pattern. It is explicitly NOT
shipped or maintained by khive core. This keeps the option open for users who want it without
moving the upstream variance into khive's quality-gated tree.

### Upstream contribution policy

When `khive-vec-hnsw` development uncovers a bug, ambiguity, or improvement in a RuVector
pattern we are reimplementing, the policy is:

1. File a focused upstream issue or PR at `ruvnet/RuVector` describing the finding.
2. Land the fix in our reimplementation regardless of upstream response time.
3. Cross-link the upstream issue from the relevant `khive-vec-hnsw` doc comment, so future
   readers can trace where the design refinement came from.

This is mutually beneficial: RuVector receives upstream contributions from a serious user; khive
gets to claim active engagement with the project we credit; and both communities see the
attribution path is bidirectional rather than one-way extraction.

## Alternatives Considered

| Alternative                                                               | Pros                                                                                                                           | Cons                                                                                                                                                | Why rejected                                                                       |
| ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Keep sqlite-vec only                                                      | Zero config, already works, single file                                                                                        | O(n) scan ceiling, no quantization, no filter pushdown, WAL lock contention                                                                         | Strategically wrong for 100K+ KGs                                                  |
| Hard-fork sqlite-vec for HNSW                                             | Stays in-process, familiar codebase                                                                                            | sqlite-vec is C extension code; HNSW graft is non-trivial; we'd own the fork                                                                        | Maintenance burden exceeds benefit                                                 |
| Multi-backend trait + in-house `khive-vec-hnsw` (this ADR)                | Clean abstraction, ADR-005/009 compliant, zero risk to existing users, full quality-bar control, explicit RuVector attribution | Requires implementation work (HNSW + quantization + persistence)                                                                                    | **Selected**                                                                       |
| Adopt RuVector as a Cargo dependency (`khive-vec-ruvector` adapter crate) | Less initial implementation work; access to advanced features (DiskANN, OPQ, ColBERT)                                          | Couples khive's quality bar to upstream lint posture; 90+ crate dep graph; release cadence we do not control; data-loss bug surface we cannot audit | Quality-bar and dependency-graph concerns outweigh the implementation-time savings |
| Full microservice / sidecar (Qdrant, Weaviate, Milvus)                    | Production-tested, feature-rich                                                                                                | Requires network hop, Docker dep, complicates local-first KG, breaks ADR-012 in-process model                                                       | Wrong deployment model for khive                                                   |
| pgvector (via future khive-db-postgres)                                   | Production-quality HNSW in Postgres, pgvector is mature                                                                        | Only relevant if Postgres backend (ADR-009 v0.3+) exists; doesn't help SQLite users                                                                 | Follow ADR-009 path; out of scope for v0.2                                         |

## Q5: VectorStoreCapabilities

Each backend declares a static capabilities struct that the retrieval policy (and future hybrid
retrieval ADR) can introspect without calling any method:

```rust
// In khive-storage::types (extend existing types.rs)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorStoreCapabilities {
    /// Supports metadata pre-filter pushdown into the index scan.
    pub supports_filter: bool,
    /// Supports batch search (multiple query vectors in one call).
    pub supports_batch_search: bool,
    /// Supports quantization (reduces memory and may trade recall).
    pub supports_quantization: bool,
    /// Supports in-place update without delete+insert.
    pub supports_update: bool,
    /// Maximum supported embedding dimension, or None if unbounded.
    pub max_dimensions: Option<u32>,
    /// Index algorithms available in this backend.
    pub index_kinds: &'static [VectorIndexKind],
}
```

Example backend declarations (illustrative — not binding on the implementation PR):

| Backend                | filter | batch | quant | max_dims | index_kinds    |
| ---------------------- | ------ | ----- | ----- | -------- | -------------- |
| `SqliteVecStore`       | false  | false | false | 4096     | `[SqliteVec]`  |
| `HnswStore` (in-house) | true   | true  | true  | None     | `[Hnsw, Flat]` |

## Q1: Expanded VectorStore Trait

The proposed trait expansion to `crates/khive-storage/src/vectors.rs`:

```rust
#[async_trait]
pub trait VectorStore: Send + Sync + 'static {
    // --- Existing methods (unchanged) ---
    async fn insert(&self, subject_id: Uuid, kind: SubstrateKind,
                    namespace: &str, embedding: Vec<f32>) -> StorageResult<()>;
    async fn insert_batch(&self, records: Vec<VectorRecord>) -> StorageResult<BatchWriteSummary>;
    async fn delete(&self, subject_id: Uuid) -> StorageResult<bool>;
    async fn count(&self) -> StorageResult<u64>;
    async fn search(&self, request: VectorSearchRequest) -> StorageResult<Vec<VectorSearchHit>>;
    async fn info(&self) -> StorageResult<VectorStoreInfo>;
    async fn rebuild(&self, scope: IndexRebuildScope) -> StorageResult<VectorStoreInfo>;

    // --- New methods (default impls; backends opt in by overriding) ---

    /// Search with metadata pre-filter.
    /// Default: returns `Err(StorageError::Unsupported("filter"))` when predicates
    /// are non-empty. Backends that support native filter pushdown override this.
    /// Callers MUST check `capabilities().supports_filter` before calling; the
    /// runtime post-filters when the backend lacks native support.
    async fn search_with_filter(
        &self,
        request: VectorSearchRequest,
        filter: VectorMetadataFilter,
    ) -> StorageResult<Vec<VectorSearchHit>> {
        if filter.is_empty() {
            return self.search(request).await;
        }
        Err(StorageError::Unsupported("filter pushdown"))
    }

    /// Search with N query vectors in one round-trip (HyDE fan-out).
    /// Default: sequential calls to `search`.
    async fn search_batch(
        &self,
        requests: Vec<VectorSearchRequest>,
    ) -> StorageResult<Vec<Vec<VectorSearchHit>>> {
        let mut out = Vec::with_capacity(requests.len());
        for req in requests { out.push(self.search(req).await?); }
        Ok(out)
    }

    /// Re-embed an existing entry. Default: delete then insert.
    async fn update(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        embedding: Vec<f32>,
    ) -> StorageResult<()> {
        self.delete(subject_id).await?;
        self.insert(subject_id, kind, namespace, embedding).await
    }

    /// Declare what this backend supports (called at runtime policy construction).
    /// Default returns a baseline capabilities struct with all optional features disabled,
    /// preserving backward compatibility for existing implementations.
    fn capabilities(&self) -> &'static VectorStoreCapabilities {
        static BASELINE: VectorStoreCapabilities = VectorStoreCapabilities {
            supports_filter: false,
            supports_batch_search: false,
            supports_quantization: false,
            supports_update: false,
            max_dimensions: Some(4096),
            index_kinds: &[VectorIndexKind::SqliteVec],
        };
        &BASELINE
    }
}
```

`VectorMetadataFilter` is a new type in `types.rs`:

```rust
/// A typed predicate for backend-pushable metadata filtering.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorMetadataFilter {
    /// Restrict to these namespaces.
    pub namespaces: Vec<String>,
    /// Restrict to these substrate kinds.
    pub kinds: Vec<SubstrateKind>,
    /// Arbitrary key=value metadata predicates (equality only).
    pub properties: Vec<(String, serde_json::Value)>,
}
```

This is intentionally minimal. Range predicates, compound logic, and full expression trees are
deferred to a future retrieval ADR. The current field set covers the cases khive actually needs
for v0.2 hybrid search (namespace isolation + kind scoping).

## Q3: Runtime Selection

`RuntimeConfig` (currently in `khive-runtime`) gains:

```rust
pub enum VectorStoreKind {
    /// sqlite-vec in the same SQLite file as notes/entities (default).
    Sqlite,
    /// In-house HNSW backend — requires khive-vec-hnsw crate in the binary's dep graph.
    Hnsw(HnswStoreConfig),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HnswStoreConfig {
    /// Path for the persistence file. Defaults to `<data_dir>/vectors.bin`.
    pub data_path: Option<PathBuf>,
    /// HNSW M parameter (connections per layer). Default: 32.
    pub hnsw_m: usize,
    /// HNSW ef_construction. Default: 200.
    pub hnsw_ef_construction: usize,
    /// HNSW ef_search. Default: 100.
    pub hnsw_ef_search: usize,
    /// Quantization level.
    pub quantization: VectorQuantization,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorQuantization { #[default] None, Scalar, Product { subspaces: usize }, Binary }

pub struct RuntimeConfig {
    // ... existing fields ...
    pub vector_store_kind: VectorStoreKind,  // default: VectorStoreKind::Sqlite
}
```

In `.khive/settings.json`:
`"vector_store": "sqlite"` (default) or `"vector_store": "hnsw"` with a `"hnsw": {...}` block
for `data_path`, `hnsw_m`, `hnsw_ef_construction`, `hnsw_ef_search`, and `quantization`.

The runtime constructs a `Box<dyn VectorStore>` from config and passes `Arc<dyn VectorStore>`
to services. Services never see the concrete type — the existing `Arc<dyn Trait>` pattern from
ADR-005 (lines 44–45 and 89–91).

## Q2: Crate Structure

Following ADR-009 (lines 28–39):

| Crate              | Backend                                            | Status                               |
| ------------------ | -------------------------------------------------- | ------------------------------------ |
| `khive-db`         | sqlite-vec (current)                               | v0.1 — ships by default              |
| `khive-vec-hnsw`   | In-house HNSW + quantization + persistence         | planned v0.2 — sibling crate, opt-in |
| Community adapters | Qdrant, pgvector, Weaviate, RuVector-bridged, etc. | out-of-tree                          |

`khive-vec-hnsw` skeleton:

```
crates/khive-vec-hnsw/
├── Cargo.toml         # deps: khive-storage, khive-types, khive-score
├── README.md          # includes "Influences" section crediting RuVector
├── src/
│   ├── lib.rs              # pub use store::HnswStore, store::HnswStoreBuilder
│   ├── store.rs            # VectorStore impl
│   ├── hnsw/               # HNSW index implementation
│   ├── quantization/       # Scalar, PQ, binary
│   ├── persistence.rs      # disk format + load/save
│   ├── filter.rs           # VectorMetadataFilter pushdown into the index scan
│   └── capabilities.rs     # static VectorStoreCapabilities
└── tests/
    └── conformance.rs  # run_conformance::<HnswStore>(...) — same harness as khive-db
```

`Cargo.toml` deps for `khive-vec-hnsw`:

```toml
[dependencies]
khive-storage = { path = "../khive-storage" }
khive-types    = { path = "../khive-types" }
khive-score    = { path = "../khive-score" }
uuid           = { version = "1", features = ["v4"] }
async-trait    = "0.1"
tokio          = { version = "1", features = ["rt"] }
serde         = { version = "1", features = ["derive"] }
serde_json     = "1"
# SIMD distance acceleration — to be evaluated in implementation PR:
# simsimd or hand-rolled portable_simd. No vector-DB library dependency.
```

The dependency set is intentionally minimal — only foundation crates plus standard utilities.
No external vector-database library appears here.

## Q7: SQLite coexistence

When `khive-vec-hnsw` is the active backend:

- **Note and entity data** remain in SQLite (`~/.khive/khive.db`). No change.
- **Vectors** live in a separate file (`~/.khive/vectors.bin` by default; the exact format is
  determined in the implementation PR).
- **Keying**: the vector store ID is the note/entity UUID (`subject_id`). UUID-keyed lookup
  is O(1) regardless of the on-disk layout. No synchronisation protocol needed beyond the
  UUID join.
- **Cascade on delete**: SQLite's trigger system cannot reach the external vector file. The
  runtime must delete from both stores explicitly. The `khive-runtime` operation for
  note/entity delete already calls `vector_store.delete(subject_id)` after the SQL delete for
  the SQLite backend. This becomes a cross-store delete rather than a same-file operation.
  Failure modes: if vector delete fails after SQL delete succeeds, the vector is orphaned
  (stale but not visible through normal search because its `subject_id` UUID no longer maps
  to an active entity). Acceptable for v0.2; a background orphan-sweep is a follow-up.
- **Transactions**: vectors and notes are not in the same transaction. This is already true
  today (sqlite-vec operates outside the WAL transaction for the notes table). The operational
  contract is best-effort consistency: notes win on conflict.

## Q6: Migration Between Backends

Switching from sqlite-vec to `khive-vec-hnsw` requires re-importing all vectors into the new
store. No data loss (vectors are derived from note/entity content which lives in SQL). The
migration sequence:

1. Read all note/entity UUIDs from SQL.
2. For each UUID, read the existing vector from the old store via `VectorStore::search` or a
   future `list_ids()` / `get_by_id()` extension.
3. Write to the new store via `insert_batch`.

This is a future CLI subcommand: `khive vec-migrate --from sqlite --to hnsw`. It does not
require downtime — the user runs the migration, then updates `.khive/settings.json`. Not in
this ADR's implementation scope.

## Q8: Performance Comparison

This ADR does not make performance claims. sqlite-vec's brute-force scan and HNSW have
well-understood theoretical complexity (O(n) vs O(log n) for recall-approximate search), but
real-world performance on khive's access patterns — mix of insert, delete, and search with
namespace+kind filters at 10K–100K entity scale — requires benchmarking against actual data.

A benchmark ADR is the right vehicle. This ADR enables the comparison by establishing the
contract under which both backends can be loaded and tested identically. The conformance test
harness in `khive-vec-hnsw/tests/conformance.rs` (see ADR-009 lines 154–162 for the pattern)
is the foundation for that benchmark suite.

## Consequences

### Positive

- `VectorStore` trait is now future-proof for v0.2 retrieval features without breaking
  existing sqlite-vec users.
- Backend capabilities are declared, not discovered. Higher-level retrieval code can make
  optimisation decisions at startup.
- The in-house `khive-vec-hnsw` crate is developed under khive's own quality bar — same
  clippy and audit policies as the rest of the codebase. No upstream variance.
- HNSW, quantization, and metadata filter pushdown become available for large-KG deployments
  without changing service-layer code.
- The migration path between backends is explicit, even if not yet implemented.
- RuVector attribution is explicit, which is both honest and useful — credits prior art and
  signals to the community where khive's design influences come from.

### Negative

- Trait expansion adds four methods. All four have default implementations, so existing
  `VectorStore` implementations compile without change. `capabilities()` defaults to a
  baseline struct with all optional features disabled; backends that support filter pushdown,
  batch search, quantization, or in-place update should override it.
- Building an in-house HNSW + quantization + persistence stack is real implementation work
  — multiple weeks of engineering. The implementation PR will need a focused design pass
  and a dedicated benchmark/correctness review.
- The coexistence model (vectors in a separate file, notes in SQLite) introduces two storage
  files to manage, back up, and migrate. Not complex, but not zero-config.

### Neutral

- This ADR does not change the embedding model or the retrieval fusion logic. Those are
  defined in ADR-012 and ADR-040. The vector store is a pure storage concern; what goes in
  it is determined upstream.
- The `VectorMetadataFilter` type is deliberately minimal. It will grow as retrieval ADRs
  add complexity. Adding fields is non-breaking (serde defaults); removing fields is not.

## Implementation

### Step 1: Trait expansion (khive-storage)

- Add `VectorMetadataFilter` and `VectorStoreCapabilities` to `crates/khive-storage/src/types.rs`.
- Verify `VectorIndexKind::Hnsw` is present in `VectorIndexKind` (already exists; add
  capability metadata if needed).
- Add four new methods to `VectorStore` in `crates/khive-storage/src/vectors.rs`, all with
  default impls: `search_with_filter` delegates to `search` when the filter is empty, returns
  `StorageError::Unsupported("filter pushdown")` otherwise; `search_batch` calls `search`
  sequentially; `update` performs delete+insert; `capabilities()` returns the baseline struct.
- Implement `capabilities()` on `SqliteVecStore` (returns the sqlite-vec static caps struct).

### Step 2: RuntimeConfig extension (khive-runtime)

- Add `VectorStoreKind`, `HnswStoreConfig`, `VectorQuantization` to the runtime config.
- Update the runtime init path to construct `Arc<dyn VectorStore>` from config.
- Default: `VectorStoreKind::Sqlite` — no behaviour change for existing users.

### Step 3: khive-vec-hnsw crate (follow-up PR — substantive implementation)

- Scaffold the crate per the directory layout above.
- Implement HNSW index (graph build, search, persistence).
- Implement scalar / PQ / binary quantization paths.
- Implement filter pushdown in the index scan (the `search_with_filter` method).
- Implement `VectorStore` trait on `HnswStore`.
- Add conformance test that runs the same cases as the sqlite-vec tests against `HnswStore`.
- Gate the crate behind the `hnsw` feature in `khive-mcp/Cargo.toml`:
  `hnsw = ["dep:khive-vec-hnsw"]` — default off.
- Inline doc comments at each pattern adopted from RuVector should reference the source.

### Step 4: Benchmark suite (separate ADR)

- Define the benchmark scenarios (insert 10K, 50K, 100K; search at each scale; filter by
  namespace; batch search with N=4 queries).
- Run against `SqliteVecStore` and `HnswStore`.
- Publish results; use to inform whether `khive-vec-hnsw` graduates from `experimental` to
  `recommended` for default production deployments.

## References

- ADR-005: Storage Capability Traits — the `VectorStore` trait contract this ADR extends
- ADR-009: Backend Portability — the one-crate-per-backend pattern applied here
- ADR-012: Retrieval Architecture — inference in lattice-embed, storage in khive; this ADR
  stays within the storage boundary
- ADR-040: Re-embed flows (parallel ADR) — the `update()` method is a hook for those flows
- `crates/khive-storage/src/vectors.rs` — current `VectorStore` trait (lines 14–28)
- `crates/khive-storage/src/types.rs` — `VectorRecord`, `VectorSearchRequest`, `VectorSearchHit`
- `crates/khive-db/src/stores/vectors.rs` — `SqliteVecStore` reference implementation
- `https://github.com/ruvnet/RuVector` — RuVector repository (design reference; surveyed
  2026-05-19). Not a dependency; algorithmic and API patterns are reimplemented in
  `khive-vec-hnsw` with attribution at each adoption site.
