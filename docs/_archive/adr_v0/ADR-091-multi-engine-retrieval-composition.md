# ADR-091: Multi-Engine Retrieval Composition

**Status**: proposed\
**Date**: 2026-05-22 (drafted; renumbered from ADR-079 → ADR-091 on 2026-05-22)\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-005 (Storage Capability Traits), ADR-090 (khive-retrieval port)\
**Composed with**: ADR-092 (Brain profile orchestration), ADR-036 (Memory pack semantics)\
**Related**: ADR-078 + ADR-081/082/083/084 (multi-engine **embedding** — the layer this ADR
composes over for vector signal generation; this ADR is the **retrieval-composition** layer
above embedding)

## Context

khive's previous retrieval composed three signals: SQLite FTS5 (lexical),
sqlite-vec (dense semantic), custom RRF fusion. One embedding model, one
HNSW-less brute-force vector index, one fixed fusion strategy. khive's old
internal stack already proved that multilingual and paraphrase-heavy corpora
(Chinese / English / mixed) benefit substantially from running multiple
embedding engines in parallel and fusing their rankings — that was configured
per-deployment via TOML in the internal product.

ADR-090 ports the mature internal retrieval stack (`khive-retrieval`) into
khive-oss with `HnswIndex`, `Bm25Index`, multiple `FusionStrategy` variants,
HNSW checkpointing, and formal proofs of the core algorithms. This ADR builds
multi-engine composition on top of that foundation.

Brain (ADR-092) provides the feedback loop: per-engine weights, per-strategy
weights, and per-context buckets become tunable parameters whose posteriors
are learned from event streams over time.

## Decision

**Multi-engine retrieval is a runtime-layer composition, not a storage-layer concern.**
`khive-runtime` holds N independently-configured retrieval engines. At write time,
content is embedded by every active dense engine; at query time, the runtime fans out
the query, gathers per-engine rankings, and fuses them via a brain-tuned strategy.

### Engine model

```rust
pub struct EngineConfig {
    pub id: String,                      // "me5-small" / "bge-zh" / "splade-en"
    pub modality: Modality,              // Dense { dimensions } | Sparse | MultiVector
    pub model: String,                   // lattice-embed-resolvable model name
    pub weight_prior: f64,               // initial weight; brain refines via posteriors
    pub languages: Vec<String>,          // ["en", "multi"] | ["zh"] etc. — hints, not enforced
    pub active: bool,                    // enable/disable without removing from config
}
```

Engines are declared in TOML (`[[retrieval.engines]]`). Default OSS install ships
one dense engine. Multi-engine is opt-in by adding more engine blocks.

### Embedding surface — `lattice-embed`

`lattice-embed` is the single embedding API. khive-runtime never loads an embedding
model directly. lattice-embed exposes:

```rust
pub fn engines() -> Vec<EngineDescriptor>;
pub fn embed(engine_id: &str, text: &str) -> Result<Vec<f32>>;
pub fn embed_all(text: &str) -> Result<Vec<(String, Vec<f32>)>>;
pub fn embed_batch(engine_id: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
```

When khive's runtime config declares new engines, lattice-embed loads the
corresponding models on startup. Multi-model load = sum of model footprints
resident; acceptable on power-user configurations, undesirable on default.
Default config = one engine.

**`lattice-embed` also provides SIMD distance kernels and quantization
tiering** (`simd::cosine`, `simd::dot_product`, `simd::distance`,
`simd::tier::QuantizationTier`). These are reused across the stack — they're
not just for embedding inference. Distance kernels inside the vector store
adapters (cosine for HNSW scoring, dot for sparse retrieval) delegate to
`lattice-embed::simd` rather than RuVector's distance kernels, because
`lattice-embed` has wider SIMD coverage (AVX-512 VNNI on x86, multi-accumulator
NEON on aarch64) and we own the tuning. See ADR-090 §"Implementation source
allocation" for the full picture of which primitive comes from where.

### Per-engine vector store

One `RuvectorVectorStore` instance per engine (ADR-090 mandates one VectorDB per
engine since geometries differ). Namespace is a metadata filter on entries, not a
separate index — per-engine HNSW × per-namespace shard would be unbounded growth.

`khive-runtime` holds:

```rust
pub struct RetrievalContext {
    engines: Vec<EngineConfig>,
    dense_stores:  HashMap<String, Arc<dyn VectorStore>>,    // per dense engine
    sparse_stores: HashMap<String, Arc<dyn SparseStore>>,    // per sparse engine
    fts:           Arc<dyn TextSearch>,                       // keeps SQLite FTS5
}
```

`SparseStore` is a new trait in `khive-storage` (parallel to `VectorStore`) since
sparse vectors have a different shape (`SparseVector { indices, values }` not
`Vec<f32>`). Implemented in `khive-db-ruvector` via `ruvector_core::sparse_vector`.

### Write path

```
create_note(content, ...):
  for engine in active_dense_engines:
    vec = lattice_embed::embed(engine.id, content)
    dense_store(engine.id).insert(id, kind, namespace, vec)
  for engine in active_sparse_engines:
    sparse_vec = lattice_embed::sparse(engine.id, content)
    sparse_store(engine.id).insert(id, kind, namespace, sparse_vec)
  fts.upsert_document(id, content)  // optional, kept for exact-keyword
```

All N inserts are batched in a single transaction. If any fail, all roll back —
preserves the atomicity guarantee from ADR-024.

### Query path

```
recall(query, top_k):
  weights = brain.weights_for("recall", context={lang, kind, namespace})
  per_engine_rankings = []
  for engine in active_engines:
    if engine.modality == Dense:
      q_vec = lattice_embed::embed(engine.id, query)
      hits = dense_store(engine.id).search(q_vec, top_k * 3, filter=ns_filter)
    elif engine.modality == Sparse:
      q_sparse = lattice_embed::sparse(engine.id, query)
      hits = sparse_store(engine.id).search(q_sparse, top_k * 3, filter=ns_filter)
    per_engine_rankings.push((engine.id, hits))
  fts_hits = fts.search(query, top_k * 3)
  per_engine_rankings.push(("fts5", fts_hits))
  fused = ruvector_core::sparse_vector::fuse_rankings(per_engine_rankings, weights, top_k)
  brain.emit("recall_executed", {query, engines: ..., weights, results: fused})
  return fused
```

Fusion uses RuVector's `fuse_rankings` (RRF default, configurable to
`LinearCombination` or `DBSF`). Brain weights initialize from `weight_prior` and
update from feedback events (ADR-092).

### Verb surface — Shape C (verbs per mode)

Default `recall` is the brain-tuned multi-engine fusion. Specialized retrieval
modes get dedicated dotted verbs:

| Verb                                    | Behavior                                                                   |
| --------------------------------------- | -------------------------------------------------------------------------- |
| `recall(query)`                         | Default: hybrid dense+sparse+FTS, brain-tuned weights, no diversity rerank |
| `recall.diverse(query, lambda=0.5)`     | MMR diversity rerank over default recall results                           |
| `recall.colbert(query)`                 | ColBERT late interaction on entities with multi-field embeddings (Phase 2) |
| `recall.matryoshka(query, fast_dim=N)`  | Two-stage: fast retrieval at fast_dim, rerank at full dim                  |
| `recall.engine(query, engine="bge-zh")` | Force a single engine (no fusion, no brain)                                |
| `recall.candidates(query)`              | Existing debug verb — returns raw per-source rankings before fusion        |
| `recall.fuse(query)`                    | Existing debug verb — returns fusion output before final scoring           |

Per-call overrides on `recall` itself: `engines=["..."]`, `weights={"...": w}`,
`strategy="rrf"|"linear"|"dbsf"`. Brain learns from defaults; overrides are for
experimentation and deterministic reproduction.

## Rationale

### Why composition in runtime, not in storage?

Multi-engine is a query-time strategy, not a storage shape. Each engine's vector
store is an independent `VectorStore` impl. The fusion / orchestration is logic
that doesn't belong inside any single store. Runtime is the right layer.

### Why kill custom RRF in `khive-runtime::fusion`?

It's a hand-rolled implementation of a well-known algorithm that RuVector now
ships natively with multiple strategies and tested correctness. Deleting our
version reduces maintenance and gives users access to LinearCombination / DBSF
as alternatives without us writing them.

### Why keep FTS5 alongside RuVector sparse?

Different jobs. FTS5 is for exact-keyword queries (`list(contains="ADR-002")`)
and the `tokenize='trigram'` story for CJK substrings. RuVector sparse is for
semantic-with-lexical-bias retrieval — terms-near-meaning-anchors. Both
contribute to `recall`'s fusion. Neither replaces the other.

### Why `weight_prior` and not equal-weights default?

Equal weights assume engines are interchangeable. They're not — known model
differentials are operator knowledge. Encode them as priors; brain refines
from there.

### Why brain learns per-context (lang/kind/namespace), not just global weights?

Bayesian posteriors per `(parameter, context_bucket)` are cheap to maintain.
Without bucketing, brain converges to a global average that hides important
sub-population behavior (e.g., BGE-zh is great on `lang=zh` and useless on
`lang=en` — global average smears this).

### Why is `recall.matryoshka` a separate verb, not hidden inside `recall`?

It can be hidden. The reason for the verb: explicit control lets brain _measure_
when matryoshka helps. If matryoshka is always on, brain can't isolate its
contribution. As a verb, brain can compare `recall` (no matryoshka) vs
`recall.matryoshka` posteriors and learn when each wins.

## Alternatives Considered

| Alternative                                            | Pros                   | Cons                                                   | Why rejected                            |
| ------------------------------------------------------ | ---------------------- | ------------------------------------------------------ | --------------------------------------- |
| Single-engine + content-aware routing                  | Simpler, less memory   | Routing heuristic is fragile; can't fuse signals       | Misses fusion quality                   |
| Multi-engine but hidden behind `recall` only           | Smaller verb surface   | Brain can't isolate strategy contributions for tuning  | Defeats the tunability goal             |
| Per-engine separate verbs (`recall_me5`, `recall_bge`) | Explicit               | Combinatorial explosion; user has to know engine names | Verb surface becomes deployment-coupled |
| Keep custom RRF, just add multi-engine                 | Less RuVector coupling | Reinvents what RuVector already does well              | Wasted maintenance                      |

## Consequences

### Positive

- Multilingual and mixed-content corpora handled natively (the Chinese/English
  problem from khive-internal solves itself with engine-per-language config).
- Brain (ADR-092) has rich parameter space to learn over — per-engine, per-strategy,
  per-context. Quality improves with usage.
- Power users tune via verb arguments and TOML; OSS default stays single-engine simple.
- Hand-rolled fusion code goes away; one less thing to maintain.

### Negative

- Multi-engine startup memory cost = sum of model footprints. Mitigated: default
  ships one engine; multi-engine is explicit opt-in.
- Write latency increases with engine count (parallel embedding). Mitigated:
  batched embedding in lattice-embed, async fan-out.
- More verbs in the catalog. Mitigated: dotted-verb namespace (`recall.*`) groups
  them logically.

### Neutral

- Brain becomes responsible for more parameters. ADR-092 designs for this.

## Implementation

### Crate-level changes

- `khive-storage`: add `SparseStore` trait (parallel to `VectorStore`).
- `khive-db-ruvector`: implement `SparseStore` via `ruvector_core::sparse_vector`.
- `khive-runtime`: introduce `RetrievalContext`; refactor `search_notes` /
  `collect_recall_candidates` to walk the context's engines.
- `khive-runtime::fusion`: delete; replaced by `ruvector_core::sparse_vector::fuse_rankings`.
- `khive-pack-memory`: extend `recall` with `engines`/`weights`/`strategy` args;
  add `recall.diverse`, `recall.colbert`, `recall.matryoshka`, `recall.engine` verbs.
- `lattice-embed`: extend with named multi-engine API per the spec above.
- Config: `[[retrieval.engines]]` blocks in `~/.khive/khive.toml`.

### Migration

Existing `sqlite-vec` data has one embedding per entry under one (implicit)
engine. After backend swap to RuVector:

- Existing entries are migrated to a single default engine in the new RuVector
  store; their embeddings stay the same.
- If user later adds engines, missing-engine entries are lazily backfilled at
  next-read or via explicit `khive reindex --engine <id>`.

### Tests

- Parity: single-engine RuVector recall ≡ today's sqlite-vec recall on a frozen
  test set (same top-k order).
- Multi-engine: synthetic bilingual corpus, verify English-query→English-result
  and Chinese-query→Chinese-result with engine-per-language config.
- Brain-tuned: replay synthetic events under fixed profile, verify weights
  converge to ground truth.

## References

- ADR-005 — Storage capability traits
- ADR-090 — khive-retrieval port (the verified stack this ADR composes over)
- ADR-092 — Brain profile orchestration
- RuVector fusion: `ruvector-core/src/advanced_features/sparse_vector.rs::fuse_rankings`
- RuVector multi-vector: `ruvector-core/src/advanced_features/multi_vector.rs`
- Predecessor: khive-internal multi-embedding TOML configuration (proven on Chinese/English mixed corpora)
