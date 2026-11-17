# ADR-044: Vector Store Extensions — Capabilities, Metadata Filter, Batched Search, Update, Orphan Sweep

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
**Amended by**: proposed [ADR-160](ADR-160-shared-pack-infrastructure.md), which requires vector
insert/search handles to be bound to the complete immutable embedding-space identity on acceptance.
**Depends on**:

- [ADR-005](ADR-005-storage-capability-traits.md) — Storage Capability Traits (base `VectorStore`)
- [ADR-022](ADR-022-events-query-surface.md) — Events Query Surface (closed-enum predicate style)
- [ADR-016](ADR-016-request-dsl.md) — Request DSL (single-tool MCP envelope; rationale for CLI-only verbs)
- [ADR-031](ADR-031-multi-engine-retrieval.md) — Multi-Engine Retrieval (`RetrievalContext`, per-engine stores)
- [ADR-032](ADR-032-brain-profile-orchestration.md) — Brain Profile Orchestration (§6.1 profile-scoped recall filter)
- [ADR-033](ADR-033-recall-pipeline.md) — Recall Pipeline (`candidate_multiplier`, filter pushdown consumer)
  **Related**: [ADR-043](ADR-043-embedding-model-migration.md) — Embedding Model Migration
  consumes `orphan_sweep` and `capabilities()`; the extension contract does not depend on its
  consumer.

---

## Context

ADR-005 defines `VectorStore` with seven core methods: `insert`, `insert_batch`, `delete`,
`count`, `search`, `info`, `rebuild`. Four capabilities that were present in the old v0
`VectorStore` (ADR-041 §4–9) were not carried forward into v1:

1. **`capabilities()`** — runtime introspection of what a backend actually supports.
   Without it, pack handlers and retrieval pipelines must guess or probe by error,
   which couples call sites to error-type matching instead of declared intent.

2. **`search_with_filter`** — metadata predicate pushed into the vector index scan.
   The current workaround (`candidate_multiplier × limit` oversampling followed by
   post-hoc filtering) wastes candidates and forces inflated multiplier values on
   every filtered query, including namespace-scoped recall in ADR-033.

3. **`search_batch`** — N-query search in one call. HyDE (Hypothetical Document Embedding)
   fan-out and multi-anchor retrieval (ADR-031 §D4) both need this. Emulating it as N
   sequential `search()` calls incurs N transaction round-trips and prevents backends
   from making progress on real batch parallelism.

4. **`orphan_sweep`** — find and delete vector rows whose `subject_id` no longer exists
   in any live SQL substrate row. ADR-043's migration worker needs this to clean up
   `vec_<engine>_pending` rejects after `--abort`. There is no operator path for general
   housekeeping of vectors left behind by hard-delete cascades.

A fifth method, **`update`**, falls out cleanly as a named operation: re-embed an existing
entry. It is missing from ADR-005's `VectorStore` signature and is needed by ADR-043's
re-embed loop during migration.

This ADR amends ADR-005's `VectorStore` trait with five new methods and the companion
types required to use them. It does not add new backends, change the MCP wire protocol,
or introduce a new substrate.

---

## Decision

### 1. `VectorStoreCapabilities` — backend introspection

```rust
/// Backend capability declaration for VectorStore.
/// Returned by [`VectorStore::capabilities`] as a `&'static` reference.
/// Represents compile-time-static facts about the backend implementation,
/// NOT per-call configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorStoreCapabilities {
    /// Native metadata pre-filter pushdown into the index scan.
    pub supports_filter: bool,
    /// Native batch search (multiple query vectors, one round-trip).
    pub supports_batch_search: bool,
    /// Quantization support (scalar, product, binary).
    pub supports_quantization: bool,
    /// Atomic in-place update (no delete+insert round-trip).
    pub supports_update: bool,
    /// Orphan-sweep support.
    pub supports_orphan_sweep: bool,
    /// Maximum supported embedding dimension; `None` means unlimited.
    pub max_dimensions: Option<u32>,
    /// Index algorithms available in this backend.
    pub index_kinds: Vec<VectorIndexKind>,
}
```

`VectorStore::capabilities` returns `&'static VectorStoreCapabilities`:

```rust
fn capabilities(&self) -> &'static VectorStoreCapabilities {
    static BASELINE: OnceLock<VectorStoreCapabilities> = OnceLock::new();
    BASELINE.get_or_init(|| VectorStoreCapabilities {
        supports_filter:         false,
        supports_batch_search:   false,
        supports_quantization:   false,
        supports_update:         false,
        supports_orphan_sweep:   false,
        // sqlite-vec 0.1.9: SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192.
        max_dimensions:          Some(8192),
        index_kinds:             vec![VectorIndexKind::SqliteVec],
    })
}
```

The default `&'static` return avoids `Clone` overhead on the `Vec<VectorIndexKind>` field
while keeping the call-site ergonomics of `store.capabilities().supports_filter`. Backends
that override `capabilities()` use their own `OnceLock<VectorStoreCapabilities>`.

**`VectorIndexKind`** is a closed enum of vector index kinds. Variants represent v1
backends (`SqliteVec`) plus reserved discriminants for planned backends (`Hnsw` —
ruvector-core, not enabled in v1). Capability advertisements MUST NOT include `Hnsw`
until that backend ships.

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorIndexKind {
    /// Reserved discriminant for the future ruvector-core HNSW backend (ADR-005 §rationale).
    /// NOT available in v1 — capability advertisements MUST NOT include this variant
    /// until the ruvector-core backend ships.
    Hnsw,
    /// sqlite-vec vec0 virtual table — v1 production backend (brute-force cosine).
    SqliteVec,
    /// Explicit brute-force (alias for SqliteVec semantics; different backends).
    Flat,
}
```

`SqliteVec` is the correct label for the v1 backend. sqlite-vec uses brute-force
cosine, not HNSW. `Hnsw` is reserved for the future ruvector-core backend (ADR-005 §rationale).

---

### 2. `search_with_filter` — metadata predicate pushdown

**Signatures:**

```rust
/// Metadata filter for pre-scan pushdown.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VectorMetadataFilter {
    /// Restrict to records in these namespaces. Empty = no namespace filter.
    pub namespaces: Vec<String>,
    /// Restrict to records of these substrate kinds. Empty = no kind filter.
    pub kinds: Vec<SubstrateKind>,
    /// Arbitrary key/op/value predicates, ANDed. Empty = no property filter.
    pub property_filters: Vec<PropertyFilter>,
}

impl VectorMetadataFilter {
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty()
            && self.kinds.is_empty()
            && self.property_filters.is_empty()
    }
}

/// A single typed predicate on a metadata key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PropertyFilter {
    pub key:   String,
    pub op:    PropertyOp,
    pub value: serde_json::Value,
}

/// Closed set of comparison operators for v1.
/// Adding operators requires an ADR amendment (same discipline as ADR-002 edge relations).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyOp {
    Eq,
    Ne,
    In,
    Range,
    Exists,
}
```

```rust
// On VectorStore trait:
/// Search with metadata predicates pushed into the WHERE clause.
///
/// If `filter.is_empty()` the default impl delegates to [`search`].
/// If `capabilities().supports_filter == false` and the filter is non-empty,
/// the default impl returns `StorageError::Unsupported`.
///
/// Backends that override this method MUST set `supports_filter = true` in
/// their [`VectorStoreCapabilities`]. The inverse is also enforced: a backend
/// that claims `supports_filter = true` but does not override this method will
/// trigger a `debug_assert` in the default body.
async fn search_with_filter(
    &self,
    request: &VectorSearchRequest,
    filter:  &VectorMetadataFilter,
) -> StorageResult<Vec<VectorSearchHit>> {
    if filter.is_empty() {
        return self.search(request.clone()).await;
    }
    debug_assert!(
        !self.capabilities().supports_filter,
        "backend claims supports_filter=true but did not override search_with_filter"
    );
    Err(StorageError::Unsupported {
        capability: StorageCapability::Vectors,
        operation:  "search_with_filter".into(),
        message:    "filter pushdown not supported; set supports_filter=true only when overriding this method".into(),
    })
}
```

**Pushdown SQL shape (SQLite backend):**

The filter lowers to additional `WHERE` predicates on the `JOIN` against the substrate
tables (`entities`, `notes`, `memories`). The full SQL shape is:

```sql
SELECT v.subject_id, v.distance
FROM   vec_{engine}  v
JOIN   (
    SELECT id FROM entities WHERE namespace = ?  AND deleted_at IS NULL
    UNION ALL
    SELECT id FROM notes    WHERE namespace = ?  AND deleted_at IS NULL
    UNION ALL
    SELECT id FROM memories WHERE namespace = ?  AND deleted_at IS NULL
) live ON live.id = v.subject_id
WHERE  v.embedding MATCH ?
  AND  v.kind      IN (/* kinds */)
  AND  JSON_EXTRACT(live.properties, '$.key') = ?   -- Eq example
ORDER  BY v.distance
LIMIT  ?
```

Multiple `property_filters` are ANDed as additional `AND` clauses. The `IN` operator
uses a parameterized `IN (?, ?, ...)` clause. `Range` maps to `BETWEEN`. `Exists` maps
to `JSON_EXTRACT(...) IS NOT NULL`. No post-filter fallback mode exists — the pushdown
either succeeds or returns `Unsupported`.

**Compliance test harness:** `khive-storage::tests::compliance::vector_filter_suite`
provides a standard fixture set. Any backend that sets `supports_filter = true` in its
`VectorStoreCapabilities` MUST pass this suite. The suite covers: namespace isolation,
kind gating, single-property Eq, multi-property AND, empty filter delegates to `search`.

**Rationale for `property_filters` not `properties: Vec<(String, Value)>`** (change from
shipped code): the current shipped `VectorMetadataFilter.properties: Vec<(String, Value)>`
is equality-only with no named operator. The v1 ADR contract requires at least `In` and
`Range` for profile-scoped recall (ADR-032 §6.1) and namespace multi-select (ADR-033).
A named `PropertyOp` enum is the correct design; the shipped code is an implementation gap.

---

### 3. `search_batch` — N queries, one call

```rust
// On VectorStore trait:
/// Search N query vectors in one call. Returns one result list per input query,
/// in input order. Per-query failure is isolated: a failed query returns
/// `Err(StorageError)` in the inner Result, not an abort of the outer batch.
///
/// Default impl: sequential loop over [`search`]. Backends that support native
/// batch IO should override this and set `supports_batch_search = true`.
/// The default is NOT transactional.
async fn search_batch(
    &self,
    requests: &[VectorSearchRequest],
) -> StorageResult<Vec<StorageResult<Vec<VectorSearchHit>>>> {
    let mut out = Vec::with_capacity(requests.len());
    for req in requests {
        out.push(self.search(req.clone()).await);
    }
    Ok(out)
}
```

**Error semantics:** the outer `StorageResult` covers transport-level failure (pool
exhausted, connection dropped before any query started). Each inner `StorageResult` covers
per-query failure. A single malformed query vector does not abort the remaining queries.

HyDE fan-out and multi-anchor retrieval patterns both need the per-query isolation: they
collect all candidate sets and fuse even when some queries fail (degraded recall is better
than no recall). The caller decides whether to propagate, log, or ignore inner errors.

---

### 4. `update` — re-embed in place

```rust
// On VectorStore trait:
/// Re-embed an existing entry.
///
/// Default: delete then insert (non-atomic). `supports_update = true` only
/// when a backend overrides with a real atomic implementation.
/// Callers that need atomicity must use a backend that overrides this.
async fn update(
    &self,
    subject_id: Uuid,
    kind:       SubstrateKind,
    namespace:  &str,
    embedding:  &[f32],
) -> StorageResult<()> {
    self.delete(subject_id).await?;
    self.insert(subject_id, kind, namespace, embedding.to_vec()).await
}
```

**Consumers:** ADR-043 `EmbedMigrationWorker` uses `update` for incremental re-embed.
Any future LoRA-adapted re-embed flow (ADR-032 §5b) uses the same method.

---

### 5. `orphan_sweep` — find and delete stale vectors

```rust
/// Configuration for [`VectorStore::orphan_sweep`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrphanSweepConfig {
    /// Optional allowlist of subject IDs to check. None = scan all rows.
    /// Non-None restricts the sweep to only the listed IDs; rows not in the
    /// list are untouched even if orphaned.
    pub subject_id_allowlist: Option<Vec<Uuid>>,
    /// Restrict sweep to these namespaces. Empty = all namespaces.
    pub namespaces: Vec<String>,
    /// Restrict sweep to these substrate kinds. Empty = all kinds.
    pub substrate_kinds: Vec<SubstrateKind>,
    /// Maximum rows to delete in one call. Prevents runaway deletes on large
    /// stores. Required; callers must be explicit.
    pub max_delete: u32,
    /// When true, report what would be deleted without deleting anything.
    pub dry_run: bool,
}

/// Result of an [`VectorStore::orphan_sweep`] call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrphanSweepResult {
    /// Total vector rows examined (after allowlist/namespace/kind filter).
    pub scanned: u64,
    /// Rows deleted (0 when `dry_run = true`).
    pub deleted: u64,
    /// Rows that would be deleted (populated even when `dry_run = false`).
    pub would_delete: u64,
    /// Whether `would_delete` exceeded `max_delete`, meaning deletion was capped after full counting.
    pub max_delete_hit: bool,
}
```

```rust
// On VectorStore trait:
/// Find vector rows whose subject_id has no corresponding live record in the SQL
/// substrate (`entities` / `notes` with `deleted_at IS NULL`). Memory records
/// are stored as notes with `kind = 'memory'`, so a memory vector is protected
/// only by its row in `notes` -- there is no separate `memories` table.
///
/// A vector is orphaned when its subject_id is either absent from all substrate
/// tables, or present with `deleted_at IS NOT NULL`. Soft-deleted substrate rows
/// (`deleted_at IS NOT NULL`) do not protect their vectors -- only rows with
/// `deleted_at IS NULL` are treated as live.
///
/// The anti-join + DELETE execute in one statement under the writer lock,
/// preventing TOCTOU between the scan and the delete.
///
/// Default: returns `StorageError::Unsupported` when
/// `capabilities().supports_orphan_sweep == false`. No silent no-op.
async fn orphan_sweep(
    &self,
    config: &OrphanSweepConfig,
) -> StorageResult<OrphanSweepResult> {
    let _ = config;
    Err(StorageError::Unsupported {
        capability: StorageCapability::Vectors,
        operation:  "orphan_sweep".into(),
        message:    "this backend does not support orphan sweep".into(),
    })
}
```

**Anti-join SQL (SQLite backend, executed under writer lock):**

```sql
DELETE FROM vec_{engine}
WHERE subject_id NOT IN (
    SELECT id FROM entities  WHERE deleted_at IS NULL
    UNION ALL
    SELECT id FROM notes     WHERE deleted_at IS NULL
)
-- namespace filter:
AND  (?1 IS NULL OR namespace IN (SELECT value FROM json_each(?1)))
-- substrate_kinds filter:
AND  (?2 IS NULL OR kind IN (SELECT value FROM json_each(?2)))
-- allowlist filter:
AND  (?3 IS NULL OR subject_id IN (SELECT value FROM json_each(?3)))
-- portable capped delete (SQLITE_ENABLE_UPDATE_DELETE_LIMIT not compiled in bundled rusqlite):
-- wrap as: DELETE FROM t WHERE subject_id IN (SELECT subject_id FROM t WHERE [above] LIMIT :max_delete)
```

The sweep runs three SQL operations under one `BEGIN IMMEDIATE` transaction: a
filtered `COUNT` for `scanned`, an anti-join `COUNT` for `would_delete`, and the
capped `DELETE`. This eliminates the TOCTOU window between counting orphans and
deleting them.

`max_delete` bounds the number of rows **deleted** in a single run — it is a
blast-radius and safety cap, not a scan or lock-hold duration limit. The
`scanned` and `would_delete` counts require a full filtered table scan and an
anti-join against the substrate tables, so the writer lock is held for a duration
proportional to the filtered table size regardless of `max_delete`. This is
acceptable because `orphan_sweep` is a CLI-only maintenance operation run by an
operator, not a hot-path call.

**Naming:** `subject_id_allowlist` (this ADR) replaces `include_subjects` (original
draft). The rename makes the polarity explicit: allowlist means "only these are eligible
for sweep," not "sweep everything else." `substrate_kinds` replaces the original omission
(no kind filter existed in the draft) and is needed for ADR-043's per-kind cleanup.

---

## CLI Surface

Two operator commands are added (CLI only; no MCP exposure):

| Command                                                                                 | Purpose                                                        |
| --------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| `khive vec-capabilities`                                                                | Print `VectorStoreCapabilities` as JSON for the active backend |
| `khive vec-sweep --substrate=<kinds> [--namespace=<ns>] [--max-delete=<N>] [--dry-run]` | Run orphan sweep                                               |

**Why CLI-only (not MCP):** ADR-016 establishes the single-tool MCP surface (`request`) for pack verbs — speech
acts with agent-facing illocutionary force. Bulk-deleting retrieval vectors is operator
maintenance, not an agent speech act. An adversarial or misconfigured agent calling
`orphan_sweep(dry_run=false, max_delete=1_000_000)` could silently destroy retrieval
coverage. Same boundary as ADR-043 §6 for migration triggers.

---

## Rationale

### Why `VectorStoreCapabilities` over a parallel `ExtendedVectorStore` trait

A second trait splits the implementation surface and forces callers to check which they
have. `std::io::Seek` is the canonical Rust pattern: one trait, a static capability
descriptor, callers branch on the descriptor. Same pattern here.

### Why `&'static VectorStoreCapabilities` (not `Copy` by-value)

The shipped code uses `Vec<VectorIndexKind>` in `VectorStoreCapabilities`, which is not
`Copy`. Returning by `&'static` reference preserves OnceLock semantics (zero allocation on
repeat calls) without forcing `VectorIndexKind` into a static slice.

### Why `PropertyOp` is a closed enum

ADR-022 establishes the pattern: filter predicates in khive are closed enums at the
storage trait boundary. Adding a new operator (`Lt`, `Gt`, `Contains`) requires an ADR
amendment because each operator needs verified SQL lowering for every backend that claims
`supports_filter = true`. Ad-hoc operator strings would silently fall through to
`Unsupported` at runtime.

### Why `search_batch` returns per-query `StorageResult`

A failed query in a HyDE fan-out should not abort the remaining N-1 searches. Per-query
isolation lets the caller fuse degraded results instead of discarding all of them.

### Why `orphan_sweep` is not automatic on hard delete

Hard-delete cascade to vector rows requires a cross-backend transaction. If the vector
backend is temporarily unavailable, hard deletes would block. Decoupling sweep preserves
ADR-005 §constraint 4 (single-backend scope) and gives ADR-043's migration worker an
idempotent cleanup path.

### Why rename `include_subjects` to `subject_id_allowlist`

`include_subjects` was ambiguous about polarity. `subject_id_allowlist` states it
correctly: `None` = all rows eligible; non-None = only these IDs are eligible for sweep.

---

## Consumer Cross-References

| Consumer                                        | Method                           |
| ----------------------------------------------- | -------------------------------- |
| ADR-033 recall pipeline — namespace/kind gating | `search_with_filter`             |
| ADR-032 §6.1 profile-scoped recall              | `search_with_filter`             |
| ADR-043 migration worker `--abort` cleanup      | `orphan_sweep`, `capabilities()` |
| ADR-043 re-embed batch loop                     | `update`                         |
| HyDE / multi-anchor retrieval fan-out           | `search_batch`                   |

ADR-033, ADR-032, and ADR-043 will be amended to cite these methods explicitly (separate task).

---

## Alternatives Considered

| Alternative                                             | Why rejected                                                                                                                          |
| ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `Option<VectorMetadataFilter>` on `VectorSearchRequest` | Backends without pushdown silently drop it; method separation forces `Unsupported` to surface                                         |
| Auto-sweep on hard delete                               | Cross-backend transactional dependency; violates ADR-005 §single-backend scope                                                        |
| `Copy` capabilities type with `&'static [IndexKind]`    | Shipped code uses `Vec<VectorIndexKind>` — not `Copy`; `&'static` return achieves zero-alloc semantics without forcing a static slice |
| MCP verb for `orphan_sweep`                             | Breaks agent/operator boundary; same reasoning as ADR-043 §rationale                                                                  |
| Abort `search_batch` on first failure                   | Forces per-query retry loops, defeating the purpose of batch fan-out                                                                  |

---

## Consequences

### Positive

- `candidate_multiplier` in ADR-033 drops to 2–3 for filtered queries (from 20+ with
  post-hoc filtering).
- HyDE and multi-anchor retrieval patterns become tractable without N round-trips.
- ADR-043 migration cleanup has a first-class API with explicit max-delete safety.
- Callers branch on `capabilities()` descriptors, not on error-type matching.

### Negative

- `VectorMetadataFilter.property_filters: Vec<PropertyFilter>` is more complex than the
  shipped `properties: Vec<(String, Value)>`. The shipped code is an implementation gap;
  the ADR contract takes priority.
- The compliance test harness must stay in sync with new `PropertyOp` variants; each
  addition requires an ADR amendment (the gate is correct, not a burden).

### Neutral

- No DB migration required. This ADR is a pure trait surface change. ADR-043 owns the
  `_embedding_models` and `embedding_model_id` schema migrations.
- The shipped `vectors.rs` partially implements the new methods. Full alignment is a
  separate downstream task.

---

## Implementation Notes

### File locations

| Artifact                                                                             | Location                                                                                                     |
| ------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| `VectorStore` trait (5 new defaults)                                                 | `crates/khive-storage/src/vectors.rs`                                                                        |
| New types (`PropertyFilter`, `PropertyOp`, `OrphanSweepConfig`, `OrphanSweepResult`) | `crates/khive-storage/src/types/vector.rs`                                                                   |
| `VectorMetadataFilter` field rename                                                  | `crates/khive-storage/src/types/vector.rs` (`properties` → `property_filters`, type → `Vec<PropertyFilter>`) |
| `VectorStoreCapabilities` new field                                                  | add `supports_orphan_sweep: bool` (default `false`)                                                          |
| `SqliteVecStore` overrides                                                           | `crates/khive-db/src/stores/vectors.rs`                                                                      |
| CLI `vec-capabilities` / `vec-sweep`                                                 | `crates/khive-cli/src/vec.rs` (new)                                                                          |
| Compliance test harness                                                              | `crates/khive-storage/src/tests/compliance/vector_filter_suite.rs` (new)                                     |

No required-method additions to `VectorStore`. Default impls return `Unsupported` when
the capability flag is `false`. Existing backends continue to compile unchanged.

---

## Amendment A1: `supports_multi_field` and `batch_exists` (2026-06-06)

Two additional items were added to the `VectorStore` surface after the initial ADR was accepted.
This amendment brings them under formal ADR coverage.

### `supports_multi_field` — capability flag on `VectorStoreCapabilities`

```rust
/// Whether this backend stores multiple named fields per subject
/// (e.g. `entity.title` and `entity.body` as separate vectors).
/// sqlite-vec backends use `subject_id PRIMARY KEY` and therefore support
/// only one vector per subject per namespace; this flag is `false` for them.
#[serde(default)]
pub supports_multi_field: bool,
```

**Semantics**: When `false` (the default), backends silently collapse multi-field inserts
to the last vector written for a given `subject_id`. When `true`, backends must store each
`(subject_id, field)` pair independently. The retrieval layer uses this flag to decide
whether field-disambiguated recall is available without a runtime probe.

**Compliance**: No backend currently sets this to `true`. It is declared `#[serde(default)]`
so existing serialized capability blobs deserialize correctly.

### `batch_exists` — default method on `VectorStore`

```rust
/// Check which of the given subject IDs already have embeddings in this store
/// for the specified namespace.
///
/// Returns a [`HashSet`] of IDs that are present. IDs not in the returned set
/// have no embedding. Default returns [`StorageError::Unsupported`]; backends
/// that support fast bulk existence checks should override this method.
async fn batch_exists(
    &self,
    ids: &[Uuid],
    namespace: &str,
) -> StorageResult<HashSet<Uuid>>;
```

**Semantics**: Returns the subset of `ids` that have at least one embedding in the given
namespace. The default implementation returns `StorageError::Unsupported`. Backends that
implement it should do so as a single SQL `IN (...)` query for efficiency.

**Consumer**: `kkernel::reindex` uses `batch_exists` to skip re-embedding of entries that
already have up-to-date vectors, falling back gracefully when unsupported.

**No capability flag**: Unlike the methods in §1–5, `batch_exists` does not have a
corresponding `supports_batch_exists` capability flag. Callers must use the error-type
pattern (`StorageError::Unsupported`) to detect absence. A future ADR may add the flag
if widespread adoption justifies it.

---

## References

- [ADR-005](ADR-005-storage-capability-traits.md) — base `VectorStore` trait; this ADR amends it
- [ADR-022](ADR-022-events-query-surface.md) — `EventFilter` closed-enum predicate style (model for `PropertyOp`)
- [ADR-016](ADR-016-request-dsl.md) — single-tool `request` MCP envelope; agent/operator boundary for CLI-only verbs
- [ADR-031](ADR-031-multi-engine-retrieval.md) — per-engine `RetrievalContext`; `vec_<engine>` table naming
- [ADR-032](ADR-032-brain-profile-orchestration.md) — §6.1 profile-scoped recall filter consumer
- [ADR-033](ADR-033-recall-pipeline.md) — recall pipeline; `candidate_multiplier` reduction consumer
- [ADR-043](ADR-043-embedding-model-migration.md) — `EmbedMigrationWorker` (`orphan_sweep`, `update`, `capabilities()`)
- [ADR-071](ADR-071-backend-pluggable-runtime.md) — `capabilities()` default correction (see Amendment A2 below).

## Amendment A2: `capabilities()` default must not assume SQLite (ADR-071, 2026-06-25)

ADR-044 §1 specifies the default `capabilities()` implementation on `VectorStore`:

```rust
BASELINE.get_or_init(|| VectorStoreCapabilities {
    // ...
    max_dimensions: Some(8192),                   // sqlite-vec 0.1.9 limit
    index_kinds:    vec![VectorIndexKind::SqliteVec],
})
```

This default encodes two SQLite-specific values in a backend-neutral trait. ADR-005
requires the storage traits to be backend-neutral. A non-SQLite backend that does not
override `capabilities()` would incorrectly advertise SQLite constraints.

ADR-071 §7 corrects the default to:

```rust
BASELINE.get_or_init(|| VectorStoreCapabilities {
    supports_filter:       false,
    supports_batch_search: false,
    supports_quantization: false,
    supports_update:       false,
    supports_orphan_sweep: false,
    supports_multi_field:  false,
    max_dimensions:        None,   // no assumption; backend declares its own limit
    index_kinds:           vec![], // no assumption; backend declares its own kinds
})
```

`khive-db`'s `SqliteVecStore` overrides `capabilities()` and returns:

```rust
VectorStoreCapabilities {
    // ...
    max_dimensions: Some(8192),
    index_kinds:    vec![VectorIndexKind::SqliteVec],
}
```

`VectorIndexKind::SqliteVec` is NOT removed from the enum. It is the correct discriminant
for the sqlite-vec backend. The correction is confined to the trait default; the sqlite-vec
backend's override continues to advertise its capabilities correctly.

ADR-044 §1's commentary "SqliteVec is the correct label for the v1 backend" remains
accurate. The amendment removes only the claim that the trait-level default should carry
SQLite-specific values.

## Amendment A3: operator commands and compliance tests as built (2026-09-25)

**Status**: Accepted (2026-09-25)

**Context.** The CLI Surface section adds two operator-only commands, `khive vec-capabilities`
and `khive vec-sweep --substrate=<kinds> [--namespace=<ns>] [--max-delete=<N>] [--dry-run]`.
The Implementation Notes table places them in `crates/khive-cli/src/vec.rs` (new) and the
compliance test harness in `crates/khive-storage/src/tests/compliance/vector_filter_suite.rs`
(new). Neither path has any history in this repository, and `vec-capabilities`, `vec-sweep`
and `vector_filter_suite` have no match under `crates/`, `scripts/` or `tests/`. The trait and
type rows of the same table resolve.

What exists instead:

- The operator commands are subcommands of the `kkernel` binary, added in commit 3db44e9e8:
  `kkernel vector capabilities` and `kkernel vector sweep`, in `crates/kkernel/src/vector.rs`
  (`VectorCommand::Capabilities`, `VectorCommand::Sweep`) and wired as `Command::Vector` in
  `crates/kkernel/src/cli.rs`. Neither is an MCP verb.
- `kkernel vector capabilities [--human] [--engine <label>]` prints the capability set of the
  compiled sqlite-vec backend without opening a database. `--engine` only labels the report,
  and `--db` is accepted for compatibility and not used. The ADR's command reports "for the
  active backend".
- `kkernel vector sweep [--namespace <ns>...] [--max-delete <n>] [--dry-run] [--engine <name>]
  [--db <path>]` calls `VectorStore::orphan_sweep` once per selected model store.
  `--max-delete` defaults to 1000 and is one budget shared across the selected stores. There
  is no `--substrate` flag: the command passes an empty `substrate_kinds`, which Section 5
  defines as all kinds. `--engine` selects configured engines, which the ADR's command line
  does not have.
- The compliance tests live in `crates/khive-storage/tests/compliance.rs`, which describes
  itself as the "Vector filter compliance suite (ADR-044 §2)" and provides helpers for
  backends that set `supports_filter = true`, and in
  `crates/khive-db/tests/contract/vector_filter.rs`, which checks that the SQLite store
  returns `Unsupported` for a non-empty filter. `SqliteVecStore::capabilities()`
  (`crates/khive-db/src/stores/vectors.rs`) reports `supports_filter: false` and
  `supports_orphan_sweep: true`.

**Decision (accepted).** The CLI Surface decision, two operator commands with no MCP
exposure, stands and is implemented as `kkernel vector capabilities` and
`kkernel vector sweep`. The `khive vec-*` command names and the Implementation Notes rows for
the CLI file and the compliance harness are superseded by the locations above. The sweep
command has no substrate-kind filter; selecting model stores with `--engine` is the shipped
way to narrow it, and `OrphanSweepConfig.substrate_kinds` stays available to callers of the
trait.

**Alternatives considered.**

- Add `khive vec-capabilities` and `khive vec-sweep` as specified. The operator binary is
  `kkernel`, and ADR-043's engine commands already live there as `kkernel engine`. A second
  name for the same two operations would add surface without adding capability.
- Require a `--substrate` flag on `kkernel vector sweep`. The consumer this ADR names for the
  kind filter is ADR-043's migration worker, which is deferred and would call
  `orphan_sweep` directly with `substrate_kinds` set. No shipped operator workflow needs
  the flag, so requiring it is a new feature rather than a correction.

**Consequences.** `docs/operations.md` and `crates/kkernel/docs/design.md` already document the
shipped commands; this amendment brings the ADR in line with them. No shipped backend sets
`supports_filter = true`, so the pushdown half of the compliance suite applies to no backend
until one does.

**Refs.** Commit 3db44e9e8.

## Amendment A4: persisted vector text provenance (#2878, accepted 2026-09-27)

**Status**: Accepted (2026-09-27). The implementation takes the next free contiguous
migration number at merge time; this amendment does not reserve a number.

### Context and scope

`VectorRecord.updated_at` is supplied by record producers, but the SQLite
vector writer currently inserts only the six vec0 columns and discards that
timestamp. A vec0 row also carries no evidence of the text given to the
embedder. The presence of a row therefore cannot tell a reader whether the
subject was re-rendered after that vector was computed. `VectorStore` has no
per-subject provenance read. This amendment adds that contract to ADR-044's
`VectorStore` extension surface. ADR-005 defines the base trait; ADR-043 §1.1
records the existing `field` and `embedding_model` vec0 columns and the V17
rebuild precedent. This amendment does not alter that model registry or the
existing vec0 layout.

The new evidence is optional. It allows a caller that has the exact current
rendered text to compare it with a stored vector. It does not change search,
ranking, presence-based reuse, or automatic re-indexing behavior.

At A4 landing, provenance is populated only by the two producers that carry
the embedding service's prepared-input attestation into a `VectorRecord`.
Other routes keep the live vector or perform their existing purge, but do not
claim a fingerprint or a vector write time they did not capture:

| Route at landing                                                                                                          | Provenance after the route                                                                                                                                         |
| ------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Knowledge index and `kkernel reindex` record-based writes                                                                 | Persist `updated_at` and the exact prepared-text `text_fingerprint` when the embedding outcome supplies it; an unattested provider leaves the fingerprint unknown. |
| Raw note create, update, reindex, restore, and merge-survivor reindex through `atomic_message` (also comm message writes) | Clear the old sidecar in the same atomic unit as vec0 replacement; the present vector reads with both optional fields unknown.                                     |
| Entity reindex through `curation`                                                                                         | Clear the old sidecar with the raw vec0 replacement; the present vector reads with both optional fields unknown.                                                   |
| Lower-level `SqliteVecStore::insert`, `update`, and `insert_exact_only` without a record                                  | Write a sidecar with null fingerprint and null write time, including when the BLOB is unchanged.                                                                   |
| Purge and delete routes                                                                                                   | Remove the matching sidecar with the vec0 row; a deleted vector is absent, not a present vector with invented provenance.                                          |
| Namespace move                                                                                                            | Preserve the vector bytes and clear its sidecar in the move transaction; the moved vector reads with both optional fields unknown.                                 |

Capturing prepared input and attributable write time for the raw routes is a
separate, later amendment. ADR-189's namespace move keeps the vector rather
than re-embedding it; clearing this additional sidecar does not change that
vector move or its ANN write-log obligations.

### Decision

**A4.1: source fingerprint and interface.** Add
`VectorRecord.text_fingerprint: Option<ContentRef>` with a serde default of
`None`. When the producer can observe the exact document input sent to the
embedding provider, it sets the field to BLAKE3-256 of those UTF-8 bytes,
encoded by the existing lowercase-hex `ContentRef` convention. The input
includes the configured document-byte bound and the selected model's document
prefix, in their actual order. The producer captures the prepared input for
the same embedding request; it must not re-render the subject afterward to
obtain the hash. Whitespace, line endings, truncation, prefix, and other bytes
are significant. Do not hash the vector, subject ID, or an unbounded or
reconstructed source string. `Some(hash of empty text)` is distinct from
`None`. A producer that cannot observe the exact prepared input sets `None`
rather than inventing provenance. A change to the bound or prefix changes the
fingerprint when it changes the provider input.

There is no universal renderer for `note.content`: note creation may embed
`embedding_content.unwrap_or(content)`, while `kkernel reindex` renders the
stored `content`. A producer hashes the input it actually sent, and a strict
caller compares only when it can reproduce that same renderer and model
preparation. A model prefix supplied inside the embedding service must be
exposed to the producer by that service or captured at its request boundary;
hashing the pre-prefix argument is not comparable evidence.

The fingerprint covers text only. The row's existing `embedding_model` value,
its model table, and its `field` remain separate identity evidence that a
strict caller must also compare. A `ContentRef` used here is a digest value;
it does not claim the source text was stored in `BlobStore`.

Add a `VectorStore::provenance(subject_id)` read with a conservative default of
`StorageError::Unsupported` for backends without a provenance read. The
SQLite implementation is scoped to its configured model table and namespace.
It returns `None` only when that scope has no live vector row. A present row
returns `VectorProvenance` with its stored `embedding_model`, `field`, optional
`text_fingerprint`, and optional `updated_at: DateTime<Utc>`. Thus a caller
can distinguish absent vector, present vector with unknown source text, and
present vector with a comparable fingerprint. `Unsupported` is not an absent
vector. A caller claiming the vector matches current text must compare the
digest and embedding-space/model identity; a timestamp alone does not prove
freshness. `updated_at` is the vector record's write time, not the substrate
subject's edit time.

```rust
pub struct VectorProvenance {
    pub embedding_model: String,
    pub field: String,
    pub text_fingerprint: Option<ContentRef>,
    pub updated_at: Option<DateTime<Utc>>,
}

// Default on VectorStore: return StorageError::Unsupported.
async fn provenance(&self, subject_id: Uuid) -> StorageResult<Option<VectorProvenance>>;
```

**A4.2: SQLite sidecar schema.** The next free contiguous migration creates
`vector_provenance(model_key TEXT NOT NULL, subject_id TEXT NOT NULL, namespace
TEXT NOT NULL, embedding_digest TEXT NOT NULL, text_fingerprint TEXT NULL,
updated_at TEXT NULL, PRIMARY KEY (model_key, subject_id))`.
`embedding_digest` is BLAKE3-256, as 64 lowercase hexadecimal characters, of
the exact BLOB bytes SQLite returns for the live vec0 row's `embedding`
column. It is computed after the vec0 write in the same transaction, not from
the caller's in-memory float representation. `text_fingerprint`, when
present, must also be 64 lowercase hexadecimal characters. `updated_at`,
when present, is the `DateTime<Utc>` value serialized as RFC 3339 and parsed
back into UTC. Each `vec_{model_key}` table has one vec0 row per `subject_id`;
that table key plus
`subject_id` is the logical vector-row identity. The sidecar repeats
`namespace` so reads can require it to match the live vec0 row. The read
joins from the live model table on `(model_key, subject_id, namespace)`. A
sidecar row by itself never makes a deleted or differently scoped vector
appear present. The read also recomputes `embedding_digest` over the live
vec0 BLOB. A missing sidecar or a digest mismatch reports a present vector
with both optional provenance fields `None`; it never returns metadata from
an earlier vector incarnation. No foreign key to a dynamic vec0 virtual
table is assumed.

The sidecar is used instead of adding columns to each vec0 table: vec0
columns are fixed at `CREATE VIRTUAL TABLE` time, and ADR-043's V17 required
a table rebuild to add `field` and `embedding_model`. This migration must not
rebuild every named model table just to add nullable provenance.

**A4.3: one transaction per row lifecycle.** A successful vector insert,
replacement, or upsert through `SqliteVecStore` writes the vec0 row and its
sidecar value in the same transaction or savepoint. The sidecar binds its
metadata to the bytes read back from the new row. Replacing a row with a
record whose fingerprint is `None` must clear any previous fingerprint; it
must not retain a digest for
the old text. A new record-based write persists its supplied `updated_at`.
Low-level `insert` or `update` calls without a timestamp or exact text clear
both provenance fields to unknown. Deleting a vector, including a model-wide
subject deletion or orphan sweep, removes the matching sidecar row in the
same transaction. A failed write rolls back both rows. The read's live-row
join and embedding-digest check guard against stale sidecar data after an
out-of-band deletion or replacement; they do not substitute for transactional
cleanup by a writer that knows about the sidecar. If a bypass writer replaces
the row with different embedding bytes, the read reports unknown metadata
even within one version of khive. An identical replacement BLOB cannot be
distinguished by this digest; strict callers still compare the text
fingerprint against the current prepared input. If an unrecognized writer
replaces a row with identical bytes without clearing its sidecar, the old
`updated_at` can still appear to be the new row's write time.

At adoption of this amendment, the production vec0 writer inventory is:

- `khive-db::stores::vectors` (`SqliteVecStore` insert, replacement, update,
  delete, and orphan sweep), including the public free function
  `delete_subject_from_vector_tables` used by entity and note merge cleanup;
- `khive-runtime::atomic_message::vector_insert_statements`, a raw
  DELETE+INSERT used by note create, note update/reindex, note restore, note
  merge survivor reindex, and comm message writes;
- `khive-runtime::atomic_prepare::purge_index_row_statement`, a raw vector
  DELETE in atomic index-purge plans;
- `khive-runtime::curation::entity_vector_insert_statements`, a raw
  DELETE+INSERT run by `reindex_entity` in an `atomic_unit` after entity
  update, merge, restore, or import;
- `khive-runtime::note_write::NoteVectors::apply`, which enumerates existing
  `vec_*` virtual tables and runs raw `DELETE FROM main.{table}`
  by namespace and subject ID through its supplied `SqlWriter`. The note
  creation-compensation path in `note_index` and an atomic note update with
  `embed=false` call this purge. `NoteEmbeddingInheritance` also carries
  `NoteVectors`, but its current atomic-runner branch calls only `has_rows`;
  that read does not delete a vector or its provenance; and
- `khive-db::namespace_move::move_vectors`, which stages and re-inserts vec0
  rows when a namespace moves.

Both raw DELETE+INSERT routes (`atomic_message` and `curation`) and the raw
DELETE routes (`atomic_prepare` and `NoteVectors::apply`) must clear the
matching sidecar row in the same atomic unit as their vec0 change. A sidecar
clear must cover at least every row its vec0 DELETE can remove in that model
table. In particular, `atomic_message` and `curation` delete vec0 by
`subject_id` without a namespace predicate, so their sidecar clears cannot
add a namespace predicate. The `atomic_prepare` and `NoteVectors::apply`
vec0 DELETEs include namespace; their sidecar clears may use the same scope.
`NoteVectors::apply` derives `model_key` from each validated `vec_{model_key}`
table name and clears only the row matching that `model_key`, the note's
`namespace`, and its `subject_id`. The sidecar DELETE predicate is
`model_key=? AND namespace=? AND subject_id=?`; the vec0 DELETE and sidecar
DELETE use the same supplied writer transaction. A `has_rows`-only inheritance
check does not clear sidecar metadata. These paths cannot rely on the embedding
digest: a replacement can have identical BLOB bytes and otherwise revive the
old write time. For example, an entity-type-only update schedules
`reindex_entity` although `entity_embedding_text` uses only name and
description, so a deterministic provider can produce the same BLOB without
an explicit reindex call. After a tag-only or property-only entity update,
an explicit `reindex_entity` call can do the same; those patches do not
schedule vector reindexing themselves. Until a raw route captures the exact
prepared input and attributable write time required by A4.1, its new row
remains present with unknown provenance.
Namespace movement must clear the sidecar in the same transaction as the vec0
row move, even when the re-inserted embedding BLOB is byte-identical. The
moved vector remains present, but its optional provenance fields read as
unknown; the move does not capture a new prepared input or write time. Clear
by model and staged subject without a namespace predicate: a stale sidecar
already scoped to the target could otherwise revive when its digest equals
the moved BLOB.
The digest-bound read is a fail-closed guard for an unrecognized or older
bypass writer that changes embedding bytes, not a substitute for clearing
metadata on known paths. A source-site census scans production and
test-support Rust SQL construction sites for DELETE, INSERT/REPLACE, and
UPDATE with either a literal `vec_*` target or a dynamic table target. It
records each site's
file, enclosing function, operation, occurrence count, and evidence for the
target's producer. Here "production" means sites compiled without `cfg(test)`
or `khive-db/test-support`; a build enabling
`khive-runtime/test-internals` also enables that test-support feature, but
does not turn its fixture SQL into a production writer. Pin three
`khive-db::namespace_move_fixture` INSERT sites as a separate test-support
class: `index_row` writes an FTS row and its `{table}_rowids` map through
two dynamic SQL templates, while `add_vector_row` writes a vec0 row. Their
presence under that feature must neither fail a correct production inventory
nor silently add a production route. Every production dynamic-target DML
site outside `khive-db::stores::vectors` is classified as one of the five
named vec0 routes above or as an explicitly pinned non-vector site;
store-owned vec0 sites are pinned separately. New, missing, or unclassified
sites fail the census. Identical SQL templates do not imply identical target
classes:
`stores::text` and `atomic_prepare` use the same DELETE template for FTS and
vec0 targets respectively. `namespace_move::move_vectors` is an INSERT
candidate even though its column list comes from a separate variable. The
census pins every caller of the exported
`khive-db::stores::vectors::delete_vector_statement` builder, not only its
SQL construction site. Its sole caller at this baseline is
`SqliteVecStore::delete`; make the builder `pub(crate)` in the implementation
and reject any additional in-crate caller until its sidecar behavior is
classified. An out-of-store caller must not execute the returned vec0 DELETE
without an atomic sidecar clear. The census is a bounded source-change
detector, not proof that arbitrary generated SQL was discovered; changes to
target producers or SQL assembly
require code review. Re-derive this inventory at the implementation's merge
base; a new vec0 writer must maintain the sidecar or demonstrably leave
provenance unknown.

**A4.4: upgrade and interoperability.** The provenance migration takes the
next free version in the contiguous migration ledger when its code lands.
It creates only the sidecar and does not backfill guessed fingerprints or
timestamps. A vec0 row written before that migration has no sidecar row, so
the read returns a present vector with both optional fields `None`. This
remains true after restart until a provenance-aware
writer replaces it. The sidecar must be created through that migration, not
opportunistically by writable vector-store setup before the ledger records it.

An upgraded database should have all writers quiesced and restarted on a
provenance-aware binary before provenance is trusted. An already-running older
process can modify vec0 without updating or clearing the sidecar; a later
read detects changed embedding bytes and returns unknown, but an identical
BLOB replacement cannot be distinguished. An older binary that boots through
the canonical `run_migrations` path refuses a ledger version above its
`latest_schema_version`, both on its initial read and after acquiring the
migration write lock (`crates/khive-db/src/migrations.rs`, `run_migrations`;
see also ADR-015's post-consolidation guard). This is a boot guard, not a
SQLite connection-level fence: an older process that was already open, or
one that bypasses that boot path, can still write. Mixed-version writes to
one database are therefore unsupported during cutover. Read-only consumers
may inspect legacy rows through the new read seam after migration. Other `VectorStore`
backends may continue compiling through the default `Unsupported` read, but
Rust code constructing `VectorRecord` literals must add the new optional
field; the serde default only preserves deserialization of older payloads.
The earlier Consequences statement that no migration was required applies
to the original §1-5 trait extensions, not this A4 schema addition.

### Acceptance and gates

- Persist records from the knowledge index and `kkernel reindex` producers and
  read their provenance back from SQLite. Capture each producer's exact
  prepared embedding input, including bounding and model prefix, and compute
  the expected fingerprint independently. Test the distinct note-creation
  `embedding_content` and reindex `content` renderers. Re-embed unchanged
  prepared input and observe the same fingerprint; change the prepared input
  and observe a different one. Verify the supplied `updated_at` round-trips,
  including fractional seconds. Give each producer claim a failing control.
- Migrate a database with a vec0 row from the prior canonical ledger version.
  Its persisted read reports a present vector with null fingerprint and null
  `updated_at`, not a fabricated current value. A later attributed
  replacement populates both fields. Test the actual migration chain, not a
  recreated table that merely resembles the old schema.
- Replace an attributed row through each lower-level `insert`, `update`, and
  `insert_exact_only` path with no source text and **byte-identical** embedding
  BLOB bytes. Assert directly that the resulting sidecar's `text_fingerprint`
  and `updated_at` are SQL `NULL`, independently of the digest-bound read;
  also assert the public read returns a present vector with both fields
  unknown. Include `insert_exact_only`, used by moodboard's deterministic
  visual embedding path, in the low-level fixture. Name the test
  `low_level_same_blob_reinsert_clears_provenance`; retaining either old
  sidecar field must turn it red. Delete a row and verify neither the read nor
  the sidecar reports it. Inject a transactional failure and verify vec0 and
  sidecar roll back together. Give rollback a fault-injection test and a
  control that makes that test fail.
- Start with attributed rows and a deterministic test embedder, then replace a
  note through `atomic_message` with the **same** embedding BLOB. Separately,
  change only an entity's validated `entity_type`, retaining name and
  description. The normal update path must schedule `reindex_entity` without
  an explicit call; `entity_embedding_text` omits `entity_type`, so the
  prepared input and deterministic BLOB remain unchanged. A tag-only or
  property-only update, followed by an explicit `reindex_entity` call, is a
  separate same-BLOB case because those patches do not trigger reindexing.
  After each raw replacement, the vector stays present, both optional fields
  become unknown, and the old sidecar row is absent after commit. Name the
  automatic entity-type case
  `entity_type_update_same_blob_reindex_clears_provenance`; removing the
  curation route's sidecar clear must fail it. Removing the atomic-message
  route's clear must fail `atomic_message_same_blob_note_reindex_clears_provenance`:
  its direct sidecar-row assertion must find the old row absent even though
  the replacement BLOB is identical.
- For each subject-only DELETE+INSERT route, seed an attributed vec0 row and
  sidecar under namespace A, then invoke the raw insertion helper for the
  same model and subject under namespace B. Its vec0 DELETE removes A's row;
  assert directly that A's sidecar row is also absent after commit. Adding a
  namespace-B predicate to either sidecar clear must fail the corresponding
  `raw_vector_clear_covers_subject_only_delete` fixture.
- Delete an attributed vector through `atomic_prepare` and assert directly
  that the sidecar row is absent after commit. A sidecar-unaware fixture may
  then re-insert identical vec0 bytes to check that the old metadata cannot
  revive. Removing this route's clear must fail that test, independently of
  the live-row join.
- Attribute one note vector in each of two model tables, then update the note
  with `embed=false`. The `NoteVectors::apply` purge must remove both live vec0
  rows and their sidecars in the same atomic unit. Assert the sidecar DELETE
  uses `model_key`, `namespace`, and `subject_id`, and does not remove metadata
  for an unrelated model/subject. Put a sidecar and vec0 row for the same
  `subject_id` in a **third** model table under a foreign namespace: the
  sidecar primary key is `(model_key, subject_id)`, so two namespaces cannot
  hold that subject in one model table. The local purge must leave that third
  model's foreign vector and sidecar unchanged. Fault the second model's purge
  and verify both models' vec0 and sidecar rows roll back. Name the runtime
  test `note_vectors_embed_false_purge_clears_provenance_across_models`;
  removing only this route's sidecar DELETE must turn it red.
- Attribute a newly created note's vector, then run
  `note_index::compensate_note_creation` at the matching note revision. Its
  `NoteVectors::apply` purge and note deletion must leave no live vector or
  matching sidecar. Repeat after advancing the note revision: compensation
  declines and both vector and sidecar remain. Name the runtime test
  `note_creation_compensation_clears_provenance`; bypassing the compensation
  purge must turn it red. For the inheritance branch, start with an attributed
  vector and a registered embedder, then prepare a text-changing note update
  with `embed` omitted. Run its plan through `run_atomic_unit` and stop at the
  committed atomic-unit boundary, **before** consuming post-commit effects.
  `NoteEmbeddingInheritance::has_rows` is read-only: assert that the returned
  effect is `ReindexNote` and that the vec0 BLOB and every sidecar column are
  byte-identical to their pre-update values. Then consume that committed
  effect separately and assert the ensuing post-commit `reindex_note` clears
  the old sidecar through `atomic_message` when it replaces the vector. Name
  the commit-boundary control
  `note_embedding_inheritance_preserves_provenance_without_delete`; clearing
  sidecar metadata in `has_rows` must turn its pre-effect assertion red.
- Replace an attributed vec0 row with **different** embedding bytes through
  raw test SQL that does not know about or clear the sidecar, standing in for
  an older or unrecognized writer. The vector remains present, but both
  optional fields read as unknown because the live-BLOB digest differs.
  Removing only the digest comparison must fail this test; no known-path
  clear may mask it.
- Move an attributed vector between namespaces. Assert the vector remains
  present under the target with the same embedding BLOB, both optional
  provenance fields read unknown, and its old sidecar row is absent from
  both namespaces. Repeat with a target-scoped stale sidecar for another
  moved subject and an equal BLOB; that sidecar must be absent too. The
  vector move and sidecar clear must commit atomically.
  Removing that clear alone must fail a direct sidecar-row assertion, even
  when the live-row namespace join still rejects stale data.
- Run `raw_vec0_writer_census_includes_note_vectors_apply` over the production
  source sites described in A4.3. Pin the exact site and multiplicity
  inventory for store-owned vec0 DML, the five out-of-store vec0 routes, and
  non-vector dynamic-target DML; separately pin
  `namespace_move_fixture::index_row`'s FTS and `{table}_rowids` INSERT
  templates and `namespace_move_fixture::add_vector_row`'s vec0 INSERT in
  the test-support class. Do not classify a target from SQL text alone. Pin
  `delete_vector_statement`'s sole baseline caller,
  `SqliteVecStore::delete`, and require the builder to become `pub(crate)`.
  An added caller, even when it reuses the same SQL construction site, must
  turn the census red until its atomic sidecar behavior is classified.
  Omitting `NoteVectors::apply` from the vec0 allow-list must turn the test
  red. Inject an otherwise unlisted production-like function deriving
  `table = format!("vec_{model_key}")` and issuing both a dynamic-target
  `DELETE FROM {table}` and an `INSERT INTO {table} ({columns})` whose column
  list is supplied indirectly. Each injected statement, alone, must add an
  unclassified site and turn the census red. Removing a pinned dynamic-target
  non-vector FTS site must also turn it red. Name those controls
  `raw_vec0_writer_census_rejects_unlisted_dynamic_pair` and
  `raw_vec0_writer_census_pins_non_vector_dynamic_dml`.
- Verify two equal-dimension model tables cannot borrow each other's sidecar
  rows. In one namespace, insert byte-identical vector BLOBs for the same
  subject into both, but give their sidecars distinct fingerprints and
  timestamps. Query each model and assert it returns only its own metadata;
  also assert that the
  production read statement's join has exactly one matching sidecar row for
  each model. Deleting `model_key` from the join then creates two matches and
  must fail this named `model_scoped_identical_blob_provenance` control despite
  equal digests.
  Separately, in `namespace_scoped_identical_blob_provenance`, create an
  attributed vec0 row and sidecar for namespace A, then use raw test SQL to
  move only that live vec0 row to namespace B while retaining its
  byte-identical embedding BLOB and leaving A's sidecar untouched. Query B
  through the production read statement: the vector is present but both
  provenance fields are unknown, and its sidecar join has zero matches.
  Removing only the namespace predicate from that join must turn this test
  red; the equal digest and unchanged model key must not mask the mutation.
  Reserve the migration number from the next free ledger slot at merge time;
  do not gate code on a locally staged migration that has not landed in the
  canonical ledger.

### Alternatives considered

| Alternative                                             | Disposition                                                                                                                                                                                                                                                                                                                                                                                                              |
| ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Hash the vector bytes or subject ID                     | Rejected. Neither identifies the exact text submitted to the embedder.                                                                                                                                                                                                                                                                                                                                                   |
| Infer provenance from `updated_at` or backfill old rows | Rejected. A timestamp does not prove text equality, and legacy rows have no trustworthy vector write time.                                                                                                                                                                                                                                                                                                               |
| Add nullable columns to every vec0 table                | Deferred for this change. Dynamic virtual tables need a rebuild to change their declared columns; ADR-043 V17 shows that this is possible. A bypass DELETE+INSERT omitting the nullable columns would leave unknown provenance even with identical embedding bytes. The chosen digest-bound sidecar avoids the rebuild but needs a complete raw-writer inventory to protect write time on an identical-BLOB replacement. |
| Sidecar keyed only by model and subject                 | Rejected. A bypass writer can replace vec0 without touching that row and leave false provenance. Binding the chosen sidecar to the live embedding BLOB reports unknown when the bytes change; identical-byte replacements still require known writers to clear or maintain the sidecar.                                                                                                                                  |
| Return `None` for unsupported backends                  | Rejected. It would conflate an unsupported read with a proven absent vector.                                                                                                                                                                                                                                                                                                                                             |

## Amendment A5: rerank from stored vectors (#2378, accepted 2026-09-28)

**Status**: Accepted (2026-09-28; #2378). Builds on A4 (persisted vector text provenance).

### Context

`knowledge.search` reranks its fused lexical and ANN candidates by cosine similarity. Today it does
so by re-embedding every candidate at query time: `rerank_with_embeddings`
(`crates/khive-pack-knowledge/src/knowledge/search.rs:1805`) renders each hit as
`format!("{} {}", name, content)` (`:1817`) and `embed_cosine_scores` (`:1742`) sends those texts,
plus the query when no query vector is cached yet, through the generic `embed_batch` path. The
issue records a default `limit=5` search reporting `usage.embed_calls = 37`: one query and 36
candidate bodies re-embedded on a single call.

Those candidates already have vectors. Atom vectors are written at index time under the field
`knowledge.atom` (`knowledge/index_handler.rs:359`) through the document-intent embedding path,
from the text `atom_embed_text` renders (`knowledge/util.rs:313`: name, content and a
`Tags: ...` line, joined by blank lines). The query-time rendering differs from the indexed one
(a different separator, and no tags), and the query-time embedding is generic rather than
document intent. So the rerank signal is neither cheap nor the signal the ANN stage used.

Three facts constrain the change:

1. `VectorStore` (`crates/khive-storage/src/vectors.rs`) has no by-ID vector read. `batch_exists`
   (`:189`) answers presence and A4's `provenance` (`:59`) answers what produced a row; neither
   returns the embedding.
2. The ANN stage cannot supply the similarity instead: `fuse_ann_hits` (`search.rs:236`) fuses
   by reciprocal rank, so the raw cosine is discarded and a hit keeps only `provenance.ann`.
3. Lexical candidates can reach the rerank while the warm ANN serving index is unavailable (the
   warm wait times out, the persisted corpus is non-empty). A design that reads vectors out of
   the serving index fails in exactly that state.

### Decision

**A5.1: by-ID vector read.** `VectorStore` gains

```rust
/// Read the stored embeddings of `ids` under `field` in `namespace`, for this
/// store's embedding model. Absent IDs are absent from the map; the result
/// has no ordering. Default: StorageError::Unsupported.
async fn get_vectors(
    &self,
    ids: &[Uuid],
    namespace: &str,
    field: &str,
) -> StorageResult<HashMap<Uuid, Vec<f32>>>;
```

and `VectorStoreCapabilities` gains `supports_vector_read: bool`, default `false` (A2: the
default must not assume SQLite). The SQLite store implements the read as point lookups on the
requested IDs, bounded by `ids.len()`, never as a scan, and sets the capability. The read goes to
the persisted vector store, not to any in-memory serving index, so it works when the ANN index
is cold, rebuilding or unavailable. It returns the embedding bytes as stored; it does not
consult A4 provenance.

**A5.2: candidate vectors.** When the rerank runs, each candidate's vector, atom or domain, is
taken from `get_vectors` under `knowledge.atom`, from the store of the embedding model the request's
query vector belongs to. `get_vectors` takes one namespace, like `batch_exists`. The rerank issues one
read per namespace the request searched and merges the results; an ID absent from every read is a
candidate with no stored vector. Today `knowledge.search` searches exactly one namespace, the
caller's (`token.namespace()` in `search`, `search.rs:3296`), so this is one read bounded by the
candidate count; a request that searches several namespaces makes one read per namespace, each
bounded the same way. Domain candidates follow the same rule. `knowledge.upsert_domains` writes each
domain's FTS mirror into `knowledge_atoms` under the domain's own UUID, tagged `type:domain`
(`knowledge/crud.rs:684`). `knowledge.index` embeds every live atom of the namespace, mirrors
included (`knowledge/index_handler.rs:76`). So a domain's stored `knowledge.atom` vector is its
mirror's, the vector the ANN stage scored. A candidate with no stored vector, and every candidate
when the store does not support the read, is embedded at query time through the document-intent
path using `atom_embed_text`, the indexing renderer, over the candidate's name, content and tags,
so a fallback vector lands in the same space as a stored one. This includes a domain hydrated only
from `knowledge_domains`, with no indexed mirror. No candidate vector is ever produced by the
generic `embed_batch` path.

**A5.3: the query vector.** The rerank uses the request's one query-intent embedding attempt
(`embed_query`), attempting it if no earlier stage has. If that attempt failed, the rerank does not
run: it does not substitute a generic or document-intent vector for the query. The generic query
slot in `QueryEmbeddingCache` is no longer produced by the rerank.

**A5.4: result-level provenance.** Whenever the rerank runs, the `knowledge.search` result carries

```
rerank_provenance {
  stored_vector_lookup: "supported" | "unsupported",
  candidates: <n>,
  from_stored: <n>,
  embedded_fallback: <n>,
}
```

with `candidates = from_stored + embedded_fallback`. `unsupported` always reads `from_stored: 0`.
The object is present on every run of the rerank, including a run with no fallback, so its absence
means only that the rerank did not run. The fixed `usage` counter set is unchanged. A difference of
`usage.embed_calls` and 1 happens to equal `embedded_fallback` on a default search today; it is not
a contract.

**A5.5: what stays.** `rerank=false` skips the rerank as before. Each new read (the by-ID read and
any fallback embedding) runs as a request read phase: a spent deadline degrades to "rerank did not
run", the same contract as the existing `knowledge.embedding_rerank` phase, and the lexical
stage's own timeout (reported as `candidate_provenance.lexical: "timed_out"`) is untouched. ANN-degradation reporting is unchanged. The per-hit
`provenance.embedding_rerank` flag keeps its meaning.

**A5.6: staleness is inherited, not solved here.** A stored vector is used as stored. The ANN stage
already scores candidates from the same stored vectors, so the rerank inherits exactly the
staleness the ANN stage has. Comparing a stored vector's A4 `text_fingerprint` with the current
rendering, and re-embedding on mismatch (the stale-row fallback of #2378 step 3), is a later
amendment.

### Consequences

- Ranking changes by construction. Today's rerank compares generic query and candidate vectors of
  one rendering; A5 compares a query-intent vector with document-intent vectors of the indexing
  rendering, tags included. A test that asserts today's order would pin the old behaviour.
- A default search on a fully indexed corpus embeds only the query when the ANN stage has already
  embedded it, instead of the query plus every fused candidate body.
- Every `VectorStore` implementation outside this repository keeps compiling through the default
  and reports `unsupported`, which the result makes visible rather than silent.

### Alternatives considered

- **Carry the ANN similarity through fusion** and embed only lexical-only candidates. This removes
  the need for a read seam but leaves lexical-only candidates, and every candidate while the serving
  index is unavailable, on the query-time path, and it changes `fuse_ann_hits` and `ScoredHit` for
  a signal only the rerank consumes. Rejected: it does not cover the degraded-serving case.
- **Read vectors out of the ANN serving index.** Cheapest when the index is warm; unavailable in
  exactly the state that sends lexical candidates to the rerank. Rejected.
- **Report the fallback count as an eighth `usage` counter.** `embed_calls` cannot tell an
  unsupported backend from a corpus with no stored vectors, and neither could another counter.
  Rejected in favour of A5.4.

### Verification

1. Stored vectors are read: replace one candidate's stored vector with a known, different vector;
   its rank must move. A test that only reproduces an order proves nothing about the read.
2. Fallback: a candidate with no stored vector is embedded through the document-intent path from
   `atom_embed_text`, and `rerank_provenance` reports it in `embedded_fallback`. Domains: a domain
   whose mirror has a stored vector counts in `from_stored`; a domain hydrated only from
   `knowledge_domains` counts in `embedded_fallback`.
3. Unsupported store: a store without `supports_vector_read` reads `stored_vector_lookup:
   "unsupported"`, `from_stored: 0`, `embedded_fallback == candidates`.
4. Degraded serving: with the warm index unavailable and lexical candidates present, the rerank
   runs from stored vectors (`from_stored > 0`).
5. Failed query embedding: the rerank does not run and no substitute vector is used.
6. `rerank=false` and a spent request deadline behave as before.
7. The ordering change is characterised on a fixture that includes tagged atoms, where the indexed
   and query-time renderings differ, and the characterisation is recorded with the change.
8. Must-fail controls: a no-op `get_vectors` (always empty) must turn test 1 red; routing the
   fallback through the generic path must turn test 2 red; emitting `rerank_provenance` only on
   the fallback path must turn red a test that requires the object to be absent when
   `rerank=false` and present with `from_stored: 0` on an unsupported store.

**Refs.** #2378 (and its corrections), #2878 (A4), #2307 (query-intent vector must not be
substituted).

---

## Amendment 6 — paged enumeration of stored vectors (#3686)

**Status: Proposed.** This additive interface proposal does not change the accepted
status or decisions above. Source preparation is authorized by #3686; architectural
acceptance and executed native verification remain pending.

### Interface and compatibility

Add an object-safe, defaulted `VectorStore::scan_vectors(VectorScanRequest)` method
returning `StorageResult<VectorScanPage>`. The request has required exact string
selectors `namespace`, `embedding_model` and `field`, optional `SubstrateKind`
`kind`, optional UUID `after`, and a `NonZeroU32` `limit`. A page contains
`items: Vec<VectorScanEntry>` and `next_after: Option<Uuid>`; each entry contains
`subject_id: Uuid` and `vectors: Vec<Vec<f32>>`.

The default returns Vectors/`Unsupported` with operation `scan_vectors`; it neither
returns empty success nor falls back to similarity search. No capability flag is
added or reinterpreted. The requested-ID `get_vectors` read remains separate.

### Ordered interval contract

All string selectors are literal, including empty or punctuation-bearing values;
no trimming, implicit defaults or wildcard semantics apply. `kind=None` includes
all kinds. The handle chooses the physical store; the model selector filters that
store rather than constructing a different one. Supporting implementations return
one complete vector record per matching subject in ascending UUID order, strictly
after the supplied boundary. The boundary need not name an existing row. Reuse the
same selectors when continuing; changed selectors describe a new interval.

At most `limit` entries are returned. Only an observed extra matching record
causes `next_after=Some(last emitted subject_id)`; otherwise it is `None`. Empty
pages and exactly full terminal pages have no continuation. The nonzero limit
bounds materialized records, not total scan work or vector byte volume. The result
contains no count or hidden cross-request cursor state.

Each call is an independent read observation, not a snapshot spanning pages. With
unchanged data, continuation enumerates the scope once. Between calls, insertions
above the cursor can appear, insertions at or below it cannot, and future rows can
change or disappear. An advertised continuation can therefore return empty. No
watermark, restart lease or multi-page transaction is implied.

Rows are stored vectors, including those with missing or tombstoned owners. This
operation makes no lifecycle, orphan-cleanup or authorization decision. Namespace
is an exact metadata selector under the existing attribution contract.

### SQLite representation and errors

SQLite uses one admitted reader and one SELECT per page, binding selectors and
fetching `limit + 1` rows after filtering and binary UUID-text ordering. Only the
constructor-validated model-table identifier is interpolated. No MATCH, model
loading, ANN access or write runs. Existing request stop and driver classifications
are preserved under operation `vec_scan_vectors`.

Typed writers persist canonical lowercase dashed UUID text. Any encountered
malformed or noncanonical ID is a whole-page conversion error, so a normalized
cursor cannot disagree with the stored ordering. The public typed cursor keeps
the existing UUID serde grammar. Embeddings require the checked expected native
f32 byte length and use the shared native decoder without scoring or normalization.
SQLite returns one vector per subject; the capability keeps the multi-vector
shape. All returned rows, including lookahead, must decode successfully before a
page is returned. No nonfinite JSON round-trip guarantee is introduced.

### Scope and verification gates

Issue #3686 also permits making shared model-key derivation public instead of
migrating builders. The public `khive_runtime::config::sanitize_key` already serves
both memory and knowledge builders. Their existing corpus scans and same-statement
ANN-log watermark reads remain unchanged; ordinary pages cannot substitute for
that stronger contract.

Required native verification covers file reopen and memory vec0 reads with exact
float bits; every filter before LIMIT; boundary, lookahead and terminal cursors;
trait-object Unsupported and wire refusal; deterministic inter-page mutations;
raw owner/tombstone behavior; malformed ID/blob and overflow refusal; and request
cancellation/deadline precedence with subsequent healthy reads. The source change
and authored tests do not establish that those executions passed. Acceptance of
this amendment and native results are separate gates before claiming completion.

---

## Amendment 7: Entity-filter pushdown for exact vector search (#3687)

**Status**: Accepted 2026-11-16 (#6730). The accepted decisions and the separate Proposed status of Amendment 6 remain unchanged.

### Scope and capability

Entity hybrid search must apply its resolved canonical entity-kind and entity-type predicates before the exact SQLite vector arm chooses its bounded candidate window. This decision covers entity exact search; it does not extend generic ANN, note search or memory recall, add an MCP parameter, or add a candidate-ID field to the metadata filter.

The candidate-completeness concern in #3687 is already addressed by the separate exact candidate-scoring route described in [ADR-012](ADR-012-retrieval-composition.md). `rerank_in` validates the named embedding space, calls `score_candidates` with the entire supplied ID set, and only then truncates (`crates/khive-runtime/src/retrieval/single_engine.rs:174–195`). The storage contract expressly forbids namespace-wide top-K followed by candidate intersection (`crates/khive-storage/src/vectors.rs:91–112`). The stored regression `rerank_scores_every_candidate_before_cutting_and_breaks_ties_by_id` in `crates/khive-runtime/tests/candidate_rerank.rs` puts requested distant candidates behind closer outsiders and verifies their scores and final limit. This amendment preserves that mechanism rather than adding an unused hybrid candidate-ID selector. Amendment A5 remains unchanged: its distinct contract is `knowledge.search` reranking from stored `knowledge.atom` vectors with its specified fallback. It is not the authority for entity candidate scoring. The existing entity rerank regressions must still run with this change.

`VectorStoreCapabilities` gains `supports_entity_filter: bool`, with a serde default of `false`. It advertises only the entity subset defined below. The trait default advertises neither filtering capability. SQLite may advertise the new flag only when it implements that subset and passes its acceptance conditions. Its `supports_filter` remains false unless the complete existing generic namespace/kind/property compliance suite passes. The two flags are independent: neither implies the other, and the old generic suite does not establish support for the new entity fields. Entity-kind/type predicates require the entity-filter capability; the old generic flag alone is insufficient. The runtime entity-eligibility route checks `supports_entity_filter` for every nonempty supported filter, including namespace-only and `kinds=[Entity]` filters. It must not use `supports_filter || supports_entity_filter` as a substitute. A generic caller using only the old namespace/substrate/property contract checks `supports_filter` and retains those generic semantics; that flag does not promise authoritative live-entity eligibility or orphan removal. When both flags are present, the requested contract still determines the gate: runtime entity eligibility requires the entity flag. Both-empty filters retain the ordinary unfiltered contract.

For a filtered entity hybrid request, unavailable entity-filter capability or missing route authority returns `StorageError::Unsupported` with `StorageCapability::Vectors` and fails the whole request. This applies to the ordinary hybrid entrypoints, outcome-reporting entrypoints, and per-kind execution, even when lexical hits are available. These refusals must bypass vector-error tolerance: they cannot become an empty vector arm with successful lexical-only/degraded output. This governs a requested vector arm; it does not require an embedder for intentionally lexical-only calls or change unrelated text/query error precedence. No route may silently substitute global top-N followed by filtering.

Other vector-arm failures follow the existing entrypoint policy equally for filtered and unfiltered requests. For a capable, correctly bound store, an ordinary driver error, backend-local timeout or embedder failure returned inside the vector-error tolerance boundary has this determinate result:

| Entrypoint                                                                                                                             | Ordinary non-`Unsupported` vector-arm failure                                | Capability or authority `Unsupported` for the filtered vector arm |
| -------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| `hybrid_search` / `hybrid_search_with_text_mode`                                                                                       | Fail the request.                                                            | Fail the request.                                                 |
| `hybrid_search_each_kind`                                                                                                              | Fail the request; no successful partial kind lists.                          | Fail the request.                                                 |
| `hybrid_search_outcome` / `hybrid_search_outcome_with_text_mode`, with existing outcome tolerance enabled and a successful lexical arm | Retain the lexical hits and report the underlying failure in `vector_error`. | Fail the request, even with lexical hits.                         |

Text-arm failure, request cancellation and read-phase/outer deadline expiry remain fatal under their existing precedence; they are not reclassified as tolerable backend-local failures. This matrix preserves the strict ordinary/per-kind calls and outcome-only tolerance in `crates/khive-runtime/src/retrieval.rs:820–985`. It introduces neither universal filtered degradation nor a new failure policy for intentionally lexical-only calls.

### Required search and inherited methods

The existing `VectorSearchRequest.filter: Option<VectorMetadataFilter>` is a request carrier, not a new field introduced here. Section 2's method separation and the Alternatives table's rejection of this field do not describe the current source: the field was present in the initial Rust crate implementation, and no later authorizing decision was identified. This amendment explicitly supersedes that rejection and specifies the carrier's behavior. A required `search(request)` implementation must honor every nonempty supported request filter or return typed `Unsupported`; silently ignoring it is noncompliant. `None` and `Some(VectorMetadataFilter::default())` carry no predicates.

Advertising either filtering flag requires a corresponding `search_with_filter` override; the inherited method's diagnostic guard checks both flags on a filtered call. The inherited method inspects both `request.filter` and the explicit filter and delegates to `search` only when both are empty. Otherwise it returns typed `Unsupported`. An empty explicit filter must not create a bypass. Overrides preserve the combined-filter contract for the predicates they support.

Section 3's default batch body is replaced by this guarded delegation. It remains sequential and nontransactional, preserves input order, and isolates each query's error in its inner result; it introduces no cross-query or cross-backend transaction:

```rust
async fn search_batch(
    &self,
    requests: &[VectorSearchRequest],
) -> StorageResult<Vec<StorageResult<Vec<VectorSearchHit>>>> {
    let empty = VectorMetadataFilter::default();
    let mut out = Vec::with_capacity(requests.len());
    for request in requests {
        out.push(self.search_with_filter(request, &empty).await);
    }
    Ok(out)
}
```

A deliberately noncompliant test backend whose required `search` ignores filters is useful only as a tripwire: inherited filtered search and batch must refuse before reaching it. It is not an allowed backend implementation. Direct `search` compliance is tested separately. The operator capability report exposes both flags; Amendment A3's account of the then-shipped implementation stays historical.

### Filter fields and composition

Add these serde-defaulted fields to the existing `VectorMetadataFilter`, without adding mandatory fields to `VectorSearchRequest`:

| Field                       | Exact meaning                                                                                                                                                                                     |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `entity_kinds: Vec<String>` | Canonical `entities.kind` values, already resolved by the caller's vocabulary. Empty means unrestricted; a nonempty list is an OR. It is distinct from existing `kinds: Vec<SubstrateKind>`.      |
| `entity_types: Vec<String>` | Exact non-null `entities.entity_type` values. Empty means unrestricted; a nonempty list is an OR. No fallback to `properties.type`, case folding, alias resolution or trimming occurs in storage. |

Amend Section 2's `VectorMetadataFilter::is_empty()` to include both new lists:

```rust
self.namespaces.is_empty()
    && self.kinds.is_empty()
    && self.property_filters.is_empty()
    && self.entity_kinds.is_empty()
    && self.entity_types.is_empty()
```

An entity-kind-only or entity-type-only filter is nonempty in either carrier. This definition governs inherited delegation, default batch guards and SQLite eligibility activation; it is not merely a serialization detail. `None` and an all-default filter still carry no predicates.

All fields are ANDed. Empty kind/type lists contribute no restriction; nonempty lists select exact strings, with duplicates having no effect. `search_with_filter` conjunctively combines the request and explicit filters; neither replaces the other. Across sources, nonempty list restrictions intersect, with an empty list acting as the unrestricted identity. A disjoint intersection is represented internally as an empty selection, never as an empty unrestricted list.

The narrow capability applies to a request explicitly selecting `SubstrateKind::Entity`. Validate **each original filter source before intersection**: its existing `kinds` must be empty or contain only Entity, its property predicates must be empty, and a nonempty filter with an absent/non-Entity request kind is unsupported. Thus `[Entity, Note]` intersected with `[Entity]` refuses, as does `[Entity]` intersected with `[Note]`; normalization cannot erase an unsupported member. Unsupported predicates also refuse when another valid conjunct selects nothing. This does not remove predicates supported under the separate generic capability.

Each nonempty existing `filter.namespaces` list restricts the one effective request namespace (request value, otherwise the handle's existing default); it cannot widen it. A valid restriction excluding that namespace selects nothing. In SQLite's narrow entity-filter route, any nonempty original supported filter activates authoritative live-entity eligibility, including a filter containing only `kinds=[Entity]` or only a namespace list. The runtime requests this eligibility contract through `supports_entity_filter`, even when these are the only predicates. A generic-only backend remains usable through the old generic contract, but cannot satisfy that runtime eligibility request by advertising `supports_filter` alone. This does not impose authoritative entity joins on generic-only calls. Both original filters empty means ordinary unfiltered behavior, including its existing orphan visibility. The request's ordinary `kind` or `namespace` fields alone do not activate the new eligibility path.

For example, canonical kind restrictions `["concept"]` and `["project"]` select nothing; an absent kind restriction and `["concept"]` select concepts. An empty original request filter and a type restriction in the explicit filter preserve that type restriction. A namespace list containing only another namespace selects nothing; adding an unsupported property predicate still refuses.

### Population and execution

For an activated SQLite entity filter, the authoritative entity must be live under existing `EntityStore` exact-column semantics. Its namespace must equal the effective request namespace; a null type does not match a nonempty type restriction. Missing/deleted entity rows cannot consume the filtered window. The vector row must independently match the effective namespace, selected model/table, exact embedding-model predicate, substrate kind and existing field-eligibility rule. In particular runtime entity retrieval must continue excluding the pack-owned `knowledge.atom` field before its candidate window. These dimensions are distinct; no new field selector or model inference is introduced. Final live-row/visibility hydration remains a backstop against changes after selection.

The entire logical population formed by these entity and vector predicates constrains native MATCH before its limit. A JOIN around an already limited MATCH result is insufficient. Equal-distance order remains ascending subject ID with one-based ranks. Every tie-widening step and both existing exact scalar fallback causes use identical population and score semantics. Scalar fallback remains for its existing reasons; ordinary supported searches must not silently switch to scalar-only execution to claim native prefiltering.

A known-empty selection is not an escape from validation or admission. Runtime engine/model resolution and query validation still run. In the SQLite kernel, preparation first validates each original filter source against the supported subset, then checks the required same-pool authority binding for an activated entity route. Missing binding is typed `Unsupported` in that unsupported-filter phase, after source validation and before query arity or coordinate validation. Arity follows; with the expected dimension, nonfinite coordinates then produce pre-acquisition `InvalidInput`. A single search returns any preparation refusal before reader acquisition, including authority refusal combined with invalid arity, a nonfinite vector or an otherwise empty selection. If preparation succeeds, a wrong dimension retains its admitted `Driver` error and takes precedence over a nonfinite coordinate in that wrong-length vector. Preserve this ordering for disjoint restrictions, cancellation and deadlines. Old JSON validation still rejects zero `top_k`; the existing Rust zero-limit behavior is unchanged. Native undefined-cosine or overflow errors remain errors when scoring is evaluated, without requiring distance evaluation for a valid empty population.

The SQLite batch override retains its existing ordering: an empty request list returns an empty batch without a reader; a nonempty list prepares every slot, including source-filter validation and any missing-binding `Unsupported`, then acquires one reader and a shared deferred transaction even when every slot has a preparation error. Preparation before acquisition does not permit returning those errors early. If that acquisition fails, the outer admission/transport error wins even for an all-unbound or otherwise all-invalid batch; if it succeeds, each preparation error appears in its original inner slot. Errors arising during query execution remain inner results, while transaction/admission errors remain outer. This preserves the current behavior at `crates/khive-db/src/stores/vectors/vector_store_impl.rs:576–620`; it does not impose that transaction on the generic default.

Both existing public entity selectors reach the vector arm. `hybrid_search_each_kind` must equal separate hybrid searches for each requested kind: it may share the query embedding, but not a globally bounded unfiltered vector pool.

### Storage representation and route authority

Resolve eligibility from authoritative entity rows at read time, avoiding duplicated subtype metadata. Construction of the SQLite entity-capable route binds the vector handle to the authoritative entity source in the **same backend/pool**. This is an explicit trusted construction binding, not a check that some table named `entities` happens to exist. The current runtime's entity and vector accessors resolve through its selected backend; that backend constructs both accessors from its pool. Preserve configured secondary/core routing when establishing the binding.

The static `supports_entity_filter` flag describes an implemented backend capability, not proof that every instance has an authority binding. A standalone or differently routed SQLite vector handle without the required same-pool binding detects typed `Unsupported` during the unsupported-filter preparation phase specified above; it cannot consult a shadow entity table, read a different pool, or return a false empty success. A single search returns this refusal before acquisition; a nonempty SQLite batch still performs its shared acquisition before returning prepared slot errors. Unfiltered search and exact candidate scoring retain their current contracts. A correctly configured supported runtime route must receive the binding during construction; forgetting it is an implementation defect, not an allowed new route refusal.

Eligibility, native KNN and tie retries share one SQLite read snapshot. SQLite batch uses its existing shared deferred transaction. This does not make lexical search or later hydration part of that snapshot or promise cross-store atomicity.

Build one deduplicated ID restriction from authoritative live entity rows matching the canonical kind/type and effective entity namespace. Before native IN, intersect it only with IDs **present in the selected physical model table**, because sqlite-vec 0.1.9 fails text-ID-to-rowid mapping for absent IDs. Physical presence is not vector metadata eligibility: do not repeat the vector namespace predicate in this ID-presence stage. Apply authoritative entity namespace exactly once in entity eligibility, and vector namespace exactly once in each returned MATCH/scalar query, alongside its model/substrate/field predicates. Thus an entity in B with a stale vector in A fails the entity predicate for A; an entity in A with a vector in B fails the vector predicate for A. Neither independent control is masked by a second copy of the same namespace condition. Unfiltered query predicates remain unchanged.

sqlite-vec 0.1.9 accepts one primary-key IN constraint. All entity selectors therefore compose into that one ID restriction, which every native attempt and scalar fallback consumes without an earlier population cap. Eligible sets larger than the connection's host-parameter limit must work without one unbounded placeholder per ID; a set-valued binding or subquery can be considered, but stored-data acceptance must establish its native behavior. Deadline and cancellation checks cover eligibility construction, ID mapping, KNN and retries.

### Cost and representation trade-off

This decision fixes which rows can consume the candidate window; it does not promise to eliminate a whole-table/chunk scan or meet a latency threshold. The chosen read-time representation avoids subtype-copy synchronization, but constructing and transporting a large eligible set has a cost. For 100,000 supplied native IN IDs, the current sqlite-vec implementation performs 100,000 text-ID lookup preparations/executions/finalizations per native attempt, allocates at least 800,000 bytes for its 64-bit rowid array, and sorts that array. Its KNN path also walks selected chunks and reads vector blobs before applying rowid membership masking; filtering does not make that I/O independent of the scanned population. Tie retries can repeat this work. These are source-derived cost components, not measured elapsed times or an assertion of a speedup.

Acceptance records the physical vector population, entity/ID eligibility sizes, dimension, number of KNN attempts, and elapsed eligibility, native/scalar and total search time on the stored fixture, including an eligible population of 100,000. There is no timing threshold. The record makes the read-time trade-off against persisted metadata visible; it does not certify cost improvement from correct IDs alone.

A later persisted-metadata alternative must first specify and implement synchronization for single/batch/atomic insert and replacement, kind/type/namespace changes, delete/restore/merge, existing-model backfill, rollback, model isolation and reopen. Passing only new-row tests does not meet those obligations.

### Acceptance and causal controls

1. Real stored-vector entity searches find a rare canonical kind and separately a rare entity type despite more than `limit * 4` closer unrelated vectors. Expected IDs are literal and lexical hits cannot rescue the vector arm. Removing each prefilter fails its own result assertion. Record the population sizes, dimensions, attempts and elapsed components above; no performance threshold or completed measurement is implied.
2. Missing physical-model vector IDs are removed before native IN; duplicate entity selectors have no multiplicity. Disjoint restrictions return a validated empty result. A complete ID population exceeding a deliberately bounded connection host-parameter limit still returns literal expected IDs through native KNN, with room left for fixed query arguments. Truncating eligibility, treating a disjoint selection as unrestricted, or removing the ID restriction fails its own row assertion. Existing exact rerank tests retain distant requested candidates, missing/out-of-scope/duplicate handling, deterministic scores/order, named model selection and post-scoring truncation; no new `candidate_ids` metadata field is required.
3. Direct `search`, `search_with_filter` and default batch honor/refuse request filters. Independently test entity-kind-only and entity-type-only filters in each carrier with the other empty. On a backend advertising neither flag, inherited filtered search/batch must return typed `Unsupported` without reaching the deliberately ignoring required `search`; an unfiltered slot still delegates, and mixed slots preserve order. Removing either new `is_empty()` term fails the corresponding named nonempty/refusal assertion or the bound SQLite stored-ID assertion, not a debug guard or setup check. A direct-search backend ignoring its request predicate is noncompliant.

   Request-only, explicit-only, conjunction and disjoint selections retain their literal expected IDs. Source-drop controls use independently constraining kind/type lists: for example `request.filter.entity_kinds` containing `concept` and `project`, and explicit `entity_kinds` containing `concept` and `service`, select only concepts, with closer project/service decoys; dropping either source admits its decoy. Use an analogous overlapping type fixture. For namespace lists, test the single effective namespace's include/include, include/exclude, exclude/include and exclude/exclude cases, plus empty-list identity. Exclusion is absorbing. These namespace cases do not promise that dropping either source must change the result. Validate unsupported members in each original source, including `[Entity, Note]` against `[Entity]`, even when the other source excludes the namespace or selects nothing. Controls skipping source validation or turning an excluding restriction into unrestricted selection fail their own assertions; restoring direct default-batch delegation fails the inherited refusal assertion.
4. Closer decoys cover both namespace mismatches, model, field and live/deleted entity state with all other relevant dimensions eligible in each isolated fixture. The entity-namespace control removes its one eligibility predicate; the vector-namespace control removes its one predicate in the selected returned query. Wrong-type and NULL-type decoys are both excluded by the same nonempty type restriction; remove that restriction for their result control, not an independently removable null check. Their other entity/vector dimensions match.

   An absent entity is excluded by authoritative entity-row membership itself. Use separate namespace-only and `kinds=[Entity]` fixtures with a stored orphan vector and no additional entity-kind/type selector. Their control coherently bypasses the authoritative-row membership requirement while retaining the applicable vector predicates and valid SQL; assert the vector-arm IDs before later hydration can remove the orphan. Deleting a JOIN into invalid SQL or relying on a later hydrated result is not this control. Both-empty filters preserve ordinary orphan behavior.

   Two configured stores contain conflicting same-ID entity metadata: the selected route's explicit construction binding controls the result. An unbound vector handle with a convincing shadow entity table refuses; removing the binding check must fail that typed-refusal assertion. A bound secondary route must succeed from its own authority, not the core store's conflicting metadata.
5. Each execution-branch fixture forces the branch whose output is asserted. Initial MATCH uses `top_k=1` and strict distances with no boundary tie (one attempt). Widened MATCH uses `top_k=2`, three equal-distance eligible rows and a strictly farther fourth: initial `k=3` ties, `k=6` exhausts and returns (two attempts). Tie-overflow scalar uses more than 4096 equal-distance eligible rows and reaches `tie_exhausted_limit`. Direct scalar uses `top_k=4096`, whose initial `k=4097` exceeds the native cap, and reaches `direct_over_limit` with no KNN attempt. Bypass the composed ID restriction at only the selected branch's use; do not pretend separately composed predicates exist in every branch. Choose decoys so the mutant still returns through that branch, and assert its attempts/fallback reason in both pristine and mutant runs. An intact later retry must not mask the mutation. Check identical eligible IDs, cosine scores, tie order/ranks, insert-order independence and reopen.
6. Batch slots match individual searches after successful shared admission, retain the SQLite snapshot, isolate inner errors and release admission after error/cancellation. Combined-invalid cases pin runtime engine/query checks and storage source-filter validation, authority, arity, expected-length nonfinite, admission and wrong-dimension precedence, including empty selections. On a single unbound handle, unsupported source members take precedence over missing authority; with valid source predicates, missing authority takes precedence over invalid arity, expected-length NaN and wrong-length NaN, without acquiring a reader. Bound variants retain the existing arity/finite/dimension ordering.

   A nonempty all-unbound or otherwise all-invalid SQLite batch still acquires its shared reader once: with available admission its prepared errors occupy their original inner slots; with blocked or cancelled admission it returns the outer admission error. Mixed unfiltered-valid, activated-unbound and other-invalid slots retain input order. An empty batch alone bypasses acquisition. Moving authority checks after arity/finite or inside admitted execution fails its own precedence/acquisition assertion; returning prepared batch errors before acquisition fails the one-checkout/outer-error assertion. The generic default remains sequential and nontransactional. Retain exact rerank's farther-candidate regression: substituting global KNN plus intersection must fail its requested-candidate assertion.
7. Actual vector-backed `hybrid_search_each_kind` equals separate per-kind searches while query embedding remains shared. The existing lexical-only per-kind fixture is insufficient for this condition. With healthy lexical hits and entity selectors present, unavailable capability and unbound authority must fail ordinary, per-kind and outcome-reporting hybrid entrypoints with typed `Unsupported`; suppressing that error into a degraded vector arm fails the refusal assertion.

   A capable, bound filtered route also exercises an ordinary non-`Unsupported` driver/embedder failure and backend-local timeout within the existing tolerance boundary. Ordinary hybrid and per-kind calls must fail; outcome-reporting calls retain the literal healthy lexical hits and a nonempty `vector_error` identifying the underlying failure. Controls making strict calls swallow the error, or making the outcome call rethrow it, fail their respective own assertions. Preserve the existing unfiltered pair `hybrid_search_still_fails_loud_on_vector_arm_error` and `hybrid_search_outcome_preserves_text_hits_on_vector_arm_error`, plus fatal text-error, request-cancellation and read-phase/outer-deadline cases. The runtime currently owns a concrete SQLite backend; a narrow test-only failure seam can exercise propagation without inventing a configurable third-party runtime backend. Public selector normalization and final hydration remain intact.
8. Old capabilities/filters deserialize with false/unrestricted defaults; empty filters and nonempty/disjoint normalized selections remain distinct. Entity-kind-only and entity-type-only filters each make the amended five-term `is_empty()` false; old default JSON still makes it true. Operator output distinguishes the independent flags. The inherited-method guard detects either flag advertised without an override, and the old generic suite does not confer support for the new entity fields.

   Test neither/generic-only/entity-only/both capability combinations. Runtime entity-eligibility requests require `supports_entity_filter` for namespace-only and `kinds=[Entity]` filters as well as kind/type selectors; generic-only cannot satisfy that request. Changing the runtime gate to an OR of the two flags fails the generic-only refusal assertion. A generic caller on a generic-only backend retains its old generic semantics without acquiring an authoritative orphan-removal promise. Bound narrow-route orphan exclusion and both-empty compatibility remain distinct; treating namespace-only or kinds-only filters as inactive fails the vector-arm orphan assertion. Controls that misreport capability or delegate a nonempty request predicate must fail their own assertions.

For each causal control, the unchanged implementation passes its named test, the changed implementation builds and fails at the intended semantic assertion, and restoration rebuilds and passes the same test. Compilation or fixture failures, invalid SQL, timeouts, ignored/zero-selected tests and unrelated panics do not establish the behavior. All new acceptance conditions and timing records are prospective.

### Compatibility

The two new list fields and capability flag have serde defaults, so older valid JSON remains readable. Empty lists add no restriction; a normalized disjoint selection is internal state rather than an empty list written back into the public filter. Existing mandatory JSON fields and validation remain unchanged. No candidate-ID filter field or associated null/empty-list API is introduced.

Adding public struct fields is a Rust source-compatibility change for exhaustive `VectorMetadataFilter` and `VectorStoreCapabilities` literals. Callers must supply them or use an available default update expression; serde defaults do not make old Rust literals compile. No required `VectorStore` method, mandatory `VectorSearchRequest` field, MCP argument, vector-table migration or change to exact candidate-scoring rerank is introduced. The required search contract, inherited filtered-search guard and default batch delegation are strengthened as specified above.
