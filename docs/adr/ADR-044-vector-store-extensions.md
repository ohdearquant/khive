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
