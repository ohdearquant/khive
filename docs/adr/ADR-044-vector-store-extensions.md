# ADR-044: Vector Store Extensions — Capabilities, Metadata Filter, Batched Search, Update, Orphan Sweep

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive
**Depends on**:

- [ADR-005](ADR-005-storage-capability-traits.md) — Storage Capability Traits (base `VectorStore`)
- [ADR-022](ADR-022-events-query-surface.md) — Events Query Surface (closed-enum predicate style)
- [ADR-016](ADR-016-request-dsl.md) — Request DSL (single-tool MCP envelope; rationale for CLI-only verbs)
- [ADR-031](ADR-031-multi-engine-retrieval.md) — Multi-Engine Retrieval (`RetrievalContext`, per-engine stores)
- [ADR-032](ADR-032-brain-profile-orchestration.md) — Brain Profile Orchestration (§6.1 profile-scoped recall filter)
- [ADR-033](ADR-033-recall-pipeline.md) — Recall Pipeline (`candidate_multiplier`, filter pushdown consumer)
- [ADR-043](ADR-043-embedding-model-migration.md) — Embedding Model Migration (`orphan_sweep` consumer, `capabilities()` consumer)

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
    /// Whether `max_delete` was reached before the full scan completed.
    pub max_delete_hit: bool,
}
```

```rust
// On VectorStore trait:
/// Find vector rows whose subject_id has no corresponding live record in the SQL
/// substrate (entities / notes / memories with `deleted_at IS NULL`).
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

The anti-join and `DELETE` are one statement under a `BEGIN IMMEDIATE` transaction,
held for the duration of the sweep. This eliminates the TOCTOU window between
"find orphans" and "delete orphans." The `LIMIT` ensures the writer lock is not held
for an unbounded time on large tables.

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

| Artifact                                                                             | Location                                                                                              |
| ------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| `VectorStore` trait (5 new defaults)                                                 | `crates/khive-storage/src/vectors.rs`                                                                 |
| New types (`PropertyFilter`, `PropertyOp`, `OrphanSweepConfig`, `OrphanSweepResult`) | `crates/khive-storage/src/types.rs`                                                                   |
| `VectorMetadataFilter` field rename                                                  | `crates/khive-storage/src/types.rs` (`properties` → `property_filters`, type → `Vec<PropertyFilter>`) |
| `VectorStoreCapabilities` new field                                                  | add `supports_orphan_sweep: bool` (default `false`)                                                   |
| `SqliteVecStore` overrides                                                           | `crates/khive-db/src/stores/vectors.rs`                                                               |
| CLI `vec-capabilities` / `vec-sweep`                                                 | `crates/khive-cli/src/vec.rs` (new)                                                                   |
| Compliance test harness                                                              | `crates/khive-storage/src/tests/compliance/vector_filter_suite.rs` (new)                              |

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
