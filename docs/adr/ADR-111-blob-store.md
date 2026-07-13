# ADR-111: BlobStore — Content-Addressed Binary Object Storage

**Status**: accepted
**Date**: 2026-07-12
**Authors**: khive maintainers
**Depends on**:

- [ADR-005](ADR-005-storage-capability-traits.md) — Storage Capability Traits (trait-only capability
  surface this ADR extends with a ninth capability)
- [ADR-015](ADR-015-schema-migrations.md) — Schema Migrations (the versioned migration this ADR uses
  to add `entities.content_ref`)
- [ADR-044](ADR-044-vector-store-extensions.md) — Vector Store Extensions (the `orphan_sweep`
  CLI-only precedent this ADR mirrors for `BlobStore`)
- [ADR-086](ADR-086-doc-file-pack.md) — Doc/File Pack (deferred `StorageCapability::Blob` to "a real
  consumer" — this ADR is that amendment)

---

## Context

khive's primary substrate (SQLite, via `khive-db`) is good at typed, queryable, small-to-medium
records. It is not the right place for opaque binary payloads: source PDFs, images, and other
large blobs that a downstream consumer (the planned doc/file pack, ADR-086) wants to store and
reference from the graph, without inflating `khive.db` itself or forcing every KG query to page
through blob bytes it never asked for.

ADR-005 defines eight storage capability traits (`Sql`, `Notes`, `Entities`, `Graph`, `Events`,
`Vectors`, `Sparse`, `Text`) under a "zero implementation, trait-only" constraint for
`khive-storage`. ADR-086 explicitly deferred adding a blob capability until a real consumer needed
it, and named `StorageCapability::Blob` as the natural v2 amendment. This ADR is that amendment
(khive#292): a `BlobStore` trait plus its first (filesystem) implementation, so the doc/file pack
and any future blob-shaped consumer have a typed, content-addressed storage seam to build on.

This ADR does not implement the doc/file pack itself — only the storage-layer capability it will
consume.

---

## Decision

### 1. `StorageCapability::Blob` — the ninth capability

`khive-storage::StorageCapability` gains a `Blob` variant, following the existing enum's 1:1
mapping to a capability trait (ADR-005 §2).

### 2. `ContentRef` — the opaque, content-addressed key

```rust
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentRef(String);
```

Backed by a lowercase-hex BLAKE3-256 digest (64 characters) of the blob's bytes. Identical content
always produces the same `ContentRef`; storing the same bytes twice is a no-op after the first
write. `ContentRef::from_hex` rejects anything that is not exactly 64 lowercase hex characters —
uppercase is rejected rather than normalized, because the value doubles as a filesystem path
component in the shipped backend, and accepting both cases would let two `ContentRef` values that
compare unequal as `String`s resolve to the same bytes.

`khive-storage` has zero heavy dependencies (ADR-005 constraint), so `ContentRef` does not depend
on the `blake3` crate itself — `from_digest_bytes(&[u8; 32])` accepts a digest computed by the
caller (the filesystem backend, which does depend on `blake3`), and the trait's own hex-encoder is
hand-rolled (7 lines, tested against BLAKE3's own published test vector for `BLAKE3("")`).

### 3. `BlobStore` trait

```rust
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef>;
    async fn get(&self, content_ref: &ContentRef) -> StorageResult<Vec<u8>>;
    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool>;
    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool>;
    async fn orphan_sweep(
        &self,
        config: &BlobOrphanSweepConfig,
    ) -> StorageResult<BlobOrphanSweepResult> { /* default: Unsupported */ }
}
```

`get` returns `StorageError::NotFound` (capability `Blob`) for an absent reference. `delete`
returns `Ok(false)` (not an error) when nothing existed to remove — deleting an absent object is
not a failure. `orphan_sweep` defaults to `StorageError::Unsupported`, following `VectorStore`'s
precedent (ADR-044): a backend opts in by overriding it.

### 4. `FsBlobStore` — the filesystem backend (`khive-db`)

The first (and, at time of writing, only) `BlobStore` implementation is a BLAKE3-sharded directory
tree: `<root>/<hex[0..2]>/<hex[2..4]>/<hex>` — two levels of shard directories, the same shape as
git's loose-object store, so a root holding millions of blobs never puts more than a few thousand
entries in one directory.

**Atomic publish.** `put` writes to a `tempfile::NamedTempFile` created in the _same_ shard
directory as the final path (guaranteeing a same-filesystem rename), flushes and `fsync`s it,
verifies the written length matches the input length, then calls `NamedTempFile::persist` to
rename it into place. A crash mid-write leaves an orphaned temp file — never a partially-committed
blob — and `orphan_sweep`'s directory walk only ever recognizes filenames that parse as a
64-character hex `ContentRef`, so stray `.tmp-*` files are silently skipped, never treated as
either live or orphaned data.

**Dedup.** `put` computes the BLAKE3 digest of the input bytes first (a pure in-memory operation),
then checks whether the target path already exists. If it does, `put` returns the existing
`ContentRef` immediately without touching the filesystem again — no free-space check, no write.

### 5. Free-space fail-closed floor

Before writing a new object (never on a dedup hit), `put` queries the target volume's available
space via the `fs4` crate and compares it against a configured floor. Below the floor, `put`
refuses with a new error variant:

```rust
#[error(
    "refusing write on {capability:?} at {volume}: {available_bytes} bytes available, \
     below the {floor_bytes}-byte floor"
)]
CapacityFloor {
    capability: StorageCapability,
    volume: String,
    available_bytes: u64,
    floor_bytes: u64,
},
```

This is a hard refusal: no silent degrade, no auto-spill to another volume (SPEC-gate ruling,
2026-07-12). The default floor is 100 GB (`FsBlobStore::DEFAULT_FLOOR_BYTES = 100_000_000_000`),
config-overridable via `StorageBackend::blob_store`'s `floor_bytes` parameter.

`fs4` (not a hand-rolled `libc::statvfs` call) was chosen specifically because khive's release
pipeline (`release.yml`) cross-builds for a `windows-latest` target; `fs4::available_space` is
unconditionally cross-platform (rustix on Unix, windows-sys on Windows), so the free-space check
does not need a maintainer-authored Windows FFI path.

### 6. Blob root resolution

`khive-db` cannot parse `khive.toml` itself without introducing an upward dependency (it sits
below `khive-runtime` in the crate chain). `StorageBackend::blob_store` therefore resolves the
root directory in this precedence order:

1. `KHIVE_BLOB_ROOT` environment variable (process-global, safe to read directly at any layer).
2. `config_root` — an explicit override the caller passes in, expected to be resolved from
   `khive.toml`'s `[storage.blob] root` by a layer above `khive-db` (e.g. `khive-runtime` or
   `kkernel`).
3. Default: beside the database file, at `<db_dir>/blobs`.

An in-memory backend with no `config_root` and no environment variable has no directory to default
beside, and `resolve_blob_root` returns an error rather than picking an arbitrary path.

### 7. `entities.content_ref` — the reference column

A new nullable, indexed column on `entities` (migration V10,
`crates/khive-db/sql/010-entities-content-ref.sql`):

```sql
ALTER TABLE entities ADD COLUMN content_ref TEXT;

CREATE INDEX IF NOT EXISTS idx_entities_content_ref
    ON entities(content_ref)
    WHERE content_ref IS NOT NULL;
```

`content_ref` is a first-class column, not a key buried inside `properties` — this lets orphan-GC
(deliverable 5, below) join against it cheaply instead of scanning and parsing JSON. Storage does
not validate that the referenced blob actually exists; publish-then-reference is the caller's
responsibility (an entity can legally reference a `content_ref` before, concurrently with, or
instead of an actual `BlobStore::put`, the same way `merged_into` can reference an entity ID with
no read-side existence check).

The same DDL is mirrored into `sql/entities-ddl.sql` (the non-versioned schema some callers apply
directly via `ensure_entities_schema`, e.g. `StorageBackend::memory()` test setups) — unlike V9's
index, which was not mirrored, a new _column_ referenced by `Entity`'s Rust struct fields and every
`SELECT`/`INSERT` in `khive-db`'s and `khive-runtime`'s entity code paths must exist under both
DDL sources, or any caller that never runs the versioned migration chain breaks with "no such
column: content_ref".

### 8. Orphan GC — the only deletion path besides explicit `delete`

`BlobStore::orphan_sweep` is the ninth capability's mirror of `VectorStore::orphan_sweep`
(ADR-044): an admin-side operation, not an MCP verb (adding one would be a wire-surface change
requiring its own ADR amendment, per ADR-023). The caller (an admin CLI, not a live consumer path)
assembles the set of live `content_ref`s — e.g. `SELECT DISTINCT content_ref FROM entities WHERE
content_ref IS NOT NULL AND deleted_at IS NULL` — and passes it in `BlobOrphanSweepConfig`;
`FsBlobStore` walks its shard tree and reports (`dry_run: true`) or deletes (`dry_run: false`)
everything not in that set.

This is deliberately the _only_ deletion path a consumer has besides an explicit
`BlobStore::delete(content_ref)` call (SPEC-gate ruling, 2026-07-12): a future doc/file pack never
deletes blob files directly, so a blob referenced from two places (or briefly orphaned mid-write by
some other in-flight operation) is never removed out from under a concurrent reader by a
consumer-side heuristic. `BlobStore` owns the deletion policy; consumers only ever add references
and let GC reconcile.

---

## Alternatives Considered

**`object_store` crate as the backend.** khive#292's issue text names the `object_store` crate
("Filesystem-first; S3-standard for cloud") as the intended backend. This ADR does not use it.
`BlobStore` (this ADR's own trait) is already the backend-swap seam `object_store` would provide —
introducing a second abstraction layer underneath a trait whose entire purpose is abstracting the
backend adds a dependency and an indirection with no current consumer that needs it. ADR-086's
"defer until a real consumer needs it" discipline, which produced this ADR's own trait in the first
place, applies again one layer down: an S3-compatible backend can be added as a second `BlobStore`
implementation (mirroring `FsBlobStore`) exactly when a consumer needs cloud storage, without
touching the trait or any existing caller. This is a known, deliberate delta from the issue's
literal text, flagged here per the issue's own "flag any place you diverge" instruction.

**Non-configurable 100 GB floor.** An earlier downstream design draft (the doc/file pack's ADR
draft, not yet accepted) describes "the non-configurable internal free-space floor" at 100 GB. This
ADR makes the floor config-overridable (default 100 GB) — the SPEC-gate ruling that produced §5
above did not revisit that point, and a hard-coded floor with no override would force every
deployment (including CI, sandboxes, and constrained environments) onto the same number with no
escape hatch. This is a known delta between this ADR and that unratified draft, to be reconciled
when the doc/file pack's own ADR is authored.

**Full re-hash verification after write.** Rather than re-reading and re-hashing the temp file
after writing it (double I/O per `put`), `FsBlobStore` verifies only the written byte length
against the input length. A length mismatch reliably catches truncated writes (disk full mid-write,
process killed mid-write); re-hashing bytes that are provably the same bytes the caller supplied
(safe Rust, no interior mutability) does not catch any additional failure mode a length check
misses.

---

## Consequences

- `khive-storage` grows one new module (`blob.rs`) and one new `StorageCapability` variant; no
  existing trait or type changes shape.
- `khive-db` grows one new store module (`stores/blob.rs`, `FsBlobStore`), one new
  `StorageBackend::blob_store` factory method, and one new migration (V10). No existing migration
  is edited.
- `Entity` (the `khive-storage` flat/SQL-facing struct, not `khive_types::entity::Entity`) grows a
  `content_ref: Option<String>` field. Every call site constructing an `Entity` literal
  (`khive-db`, `khive-runtime`, `khive-vcs`) needed updating; all currently set `content_ref: None`
  except the SQL-backed CRUD paths in `khive-db::stores::entity`, which thread the real value
  through.
- The pre-existing entity-merge SQL path in `khive-runtime::curation::merge_entity_sql`'s `INSERT
  OR REPLACE` already omits `entity_type` from its column list (a pre-existing gap, not introduced
  by this ADR) — merging an entity through that path resets `entity_type` to `NULL` in the stored
  row today. `content_ref` was deliberately left out of that same `INSERT OR REPLACE` for
  consistency with the existing (undocumented) behavior rather than silently fixing one field and
  not the other; the in-memory `MergeResult`'s returned `Entity` does still carry the "into"
  entity's `content_ref` forward, matching how it already carries `entity_type` forward in memory
  despite the DB row losing it. This existing gap should be fixed in its own change, not folded
  into this ADR's scope.
- No MCP wire-surface change: `blob_store` is reached only through `StorageBackend`, not through
  any pack verb. A future doc/file pack ADR will define what (if anything) becomes MCP-visible.

---

## Implementation Notes

- `crates/khive-storage/src/blob.rs` — `ContentRef`, `BlobOrphanSweepConfig`,
  `BlobOrphanSweepResult`, `BlobStore`.
- `crates/khive-storage/src/capability.rs` — `StorageCapability::Blob`.
- `crates/khive-storage/src/error.rs` — `StorageError::CapacityFloor`.
- `crates/khive-storage/src/entity.rs` — `Entity::content_ref`, `Entity::with_content_ref`.
- `crates/khive-db/src/stores/blob.rs` — `FsBlobStore`, `resolve_blob_root`.
- `crates/khive-db/src/backend.rs` — `StorageBackend::blob_store`.
- `crates/khive-db/sql/010-entities-content-ref.sql` — migration V10.
- `crates/khive-db/sql/entities-ddl.sql` — mirrored `content_ref` column + index.
- `crates/khive-db/src/stores/entity.rs` — `content_ref` threaded through
  `entity_upsert_statement`, `batch_upsert_entities`, `read_entity`, and all three `SELECT` column
  lists.

## References

- khive issue #292.
- [ADR-005](ADR-005-storage-capability-traits.md) — Storage Capability Traits.
- [ADR-044](ADR-044-vector-store-extensions.md) — `orphan_sweep` precedent.
- [ADR-086](ADR-086-doc-file-pack.md) — deferred `StorageCapability::Blob`.
- `fs4` crate (`https://crates.io/crates/fs4`) — cross-platform free-space query.
