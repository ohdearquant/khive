# ADR-111: BlobStore — Content-Addressed Binary Object Storage

**Status**: accepted
**Date**: 2026-07-12 (amended 2026-07-13, PR #922; Amendment 2 proposed 2026-07-16)
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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ContentRef(String);

impl<'de> Deserialize<'de> for ContentRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        ContentRef::from_hex(raw).map_err(serde::de::Error::custom)
    }
}
```

`ContentRef` derives only `Serialize` — under `#[serde(transparent)]` that just emits the inner hex
string, which is always already valid since the type's only constructors (`from_hex`,
`from_digest_bytes`) validate on the way in. `Deserialize` is implemented by hand, routing every
input through `ContentRef::from_hex`, so a malformed serialized value (wrong length, uppercase,
non-hex characters) is rejected at deserialization instead of silently constructing an invalid
`ContentRef` that would later panic in the filesystem backend's shard-path slicing. **This is
deliberate, not incidental: the former derived `Deserialize` in this ADR's Decision section
bypassed validation (PR #922).**
Do not "simplify" this back to a derive.

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
space via the `fs4` crate, subtracts the size of the pending write, and compares the result
against a configured floor — `remaining_after_write = available.saturating_sub(bytes.len())`,
refuse when `remaining_after_write < floor_bytes`. **Amended 2026-07-13:** the original
implementation compared `available` directly against the floor, with no
accounting for the write's own size — `available == floor_bytes + 1` admitted a write of any size,
including one that would leave the volume below the floor. The check is now write-size-aware.

`FsBlobStore` also serializes the whole check-then-publish critical section of `put` per
**canonical root** (a process-wide registry maps each canonicalized root path to one shared
`Arc<tokio::sync::Mutex<()>>`, held across the entire `spawn_blocking` call, from the availability
check through `persist`). **Amended 2026-07-13:** without this, two
concurrent puts could each observe the same pre-write availability snapshot, each individually pass
a write-size-aware check against it, and both proceed to write — jointly pushing the volume under
the floor even though neither write looked unsafe in isolation, since neither observed the other's
pending write. A per-root async mutex is adequate at `BlobStore`'s expected write rate; it defends
only against concurrent `FsBlobStore` callers within one process, not against another process
writing to the same volume.

**Amended 2026-07-13: the first fix was incomplete because it scoped the mutex
to one `FsBlobStore` instance** (`tokio::sync::Mutex<()>`
as a plain struct field) **and borrowed the guard across `put`'s own async stack frame.** Both were
insufficient: (a) `StorageBackend::blob_store` constructs a fresh `FsBlobStore` on every call, even
for the same root, so two independently obtained stores for one root had independent locks and
could still both pass the same snapshot; (b) cancelling or dropping the outer `put` future released
the borrowed guard immediately, while an already-started `spawn_blocking` write kept running
unprotected on its own thread — a second `put` could pass its check mid-persist. The fix now (1)
keys the shared `Arc<tokio::sync::Mutex<()>>` by the filesystem's own canonicalized root path in a
process-wide registry, so every `FsBlobStore` for the same root shares one lock regardless of how
many separate `new` calls constructed them, and (2) acquires an **owned** guard (`lock_owned`) that
is **moved into** the `spawn_blocking` closure rather than borrowed across its `.await`, so the
guard's lifetime is tied to the blocking work itself, not to whether the outer future is still being
polled. Below the floor, `put` refuses with a new error variant:

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
deletes blob files directly, so a blob referenced from two places is never removed out from under a
concurrent reader by a consumer-side heuristic. `BlobStore` owns the deletion policy; consumers only
ever add references and let GC reconcile.

**Concurrency guarantee: offline-maintenance-only (amended 2026-07-13).** The paragraph above, as
originally written, claimed this design "is never removed out from under a concurrent reader".
That claim was not true of the shipped implementation and has
been corrected here rather than left standing. Both `delete` and `orphan_sweep` are
**offline-maintenance-only** APIs, not safe to run against a live entity writer:

- `orphan_sweep`'s `live_refs` set is a **snapshot** the caller assembles before the call. Nothing
  in `BlobStore` detects a `content_ref` that becomes newly live — an entity write lands
  referencing it — between when that snapshot was taken and when the sweep runs; such a blob is
  deleted anyway. `khive-db`'s
  `orphan_sweep_race_demonstrates_the_documented_quiescence_requirement` test reproduces this
  exactly, so the hazard is pinned in code, not just prose.
- `delete` is an unconditional physical removal with the same class of hazard: any caller can
  delete a `content_ref` an entity write races into existence a moment later, with no coordination
  from this trait.

The actual guarantee is narrower than the original text implied: **run `orphan_sweep` and `delete`
only when writes that could create a new `content_ref` reference are quiesced** — a maintenance
window, a single-writer admin CLI invocation with no live traffic, or equivalent. `BlobStore` has no
visibility into the entity substrate (ADR-005 constraint 4) and therefore cannot enforce this
itself; it is a caller obligation, now stated explicitly on `BlobStore::delete` and
`BlobStore::orphan_sweep`'s doc comments.

A transactional, DB-coordinated sweep — selecting and deleting live/orphaned blobs under the entity
writer's own transactional boundary, so the sweep is safe to run concurrently with normal traffic —
would close this hazard properly. That is a larger design (does it live in `khive-storage` as a new
capability, or in `khive-runtime` orchestrating `BlobStore` + `SqlAccess`/`GraphStore` together?)
left to a follow-up: [khive#924](https://github.com/ohdearquant/khive/issues/924). It is
**deliberately not built as part of this fix**. The scoped correction makes the existing hazard
explicit and tested without expanding into a larger coordination design.

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
- **Amendments (2026-07-13, PR #922):** `ContentRef` no longer derives
  `Deserialize` — it is hand-implemented to route every input through `from_hex`, so a malformed
  serialized value is rejected at deserialization instead of later panicking in `shard_path`.
  `FsBlobStore::put`'s floor check now accounts for the pending write's own size. `delete` and
  `orphan_sweep` are now explicitly documented (trait doc comments, §8 above) as
  offline-maintenance-only, requiring quiesced entity writes — a real, undefended concurrency hazard
  the original §8 text incorrectly described as absent. A DB-coordinated transactional sweep that
  would close that hazard is tracked as a follow-up, not built here:
  [khive#924](https://github.com/ohdearquant/khive/issues/924).
- **Further amendment (same date):** the first fix for serializing `put` scoped
  its `tokio::sync::Mutex` to one
  `FsBlobStore` instance and borrowed the guard across the async fn's own frame — insufficient,
  because `StorageBackend::blob_store` builds a fresh `FsBlobStore` per call (so independently
  constructed stores for the same root had independent locks) and because cancelling the outer
  `put` future released a merely-borrowed guard while an already-dispatched blocking write kept
  running unprotected. §5 now describes the corrected design: one shared, canonical-root-keyed
  `Arc<tokio::sync::Mutex<()>>` per root, with an **owned** guard moved into the `spawn_blocking`
  closure rather than borrowed across it.

---

## Amendment 2 (2026-07-16): S3-compatible backend

**Status:** proposed amendment. The accepted filesystem decision and the existing `BlobStore`
trait remain unchanged while this amendment passes the implementation specification gate.

### Context and decision

ADR-111 originally deferred an object-store dependency until a real consumer needed remote blob
storage. That consumer now exists: deployments must be able to keep blob bytes in S3-compatible
object storage instead of on the database host's filesystem. Under the last-in-time rule, this
requirement ends the earlier deferral without reversing the trait boundary that made the backend
replaceable.

This amendment supersedes the earlier “`object_store` crate as the backend” alternative for the
new S3 implementation only. It does not move `FsBlobStore` onto `object_store` or change the
filesystem behavior accepted above.

`khive-db` gains `S3BlobStore`, a second implementation of the existing `BlobStore` trait.
`ContentRef`, `BlobOrphanSweepConfig`, `BlobOrphanSweepResult`, and all five trait method
signatures remain unchanged. Callers continue to receive `Arc<dyn BlobStore>` and cannot observe
provider-specific types.

Blob backend selection is a process-level storage-capability setting, separate from ADR-028's
pack-to-SQLite `[[backends]]` assignment:

```toml
# Existing behavior remains the default when [storage.blob] is absent.
[storage.blob]
backend = "fs"
root = "/var/lib/khive/blobs"
floor_bytes = 100000000000
```

```toml
[storage.blob]
backend = "s3"
bucket = "khive-blobs"
endpoint = "https://objects.example.invalid"
region = "us-east-1"
prefix = "blobs"
```

`backend` is a closed `fs | s3` enum. For `s3`, `bucket` and `region` are required,
`endpoint` is optional, and `prefix` defaults to `blobs`. Omitting `endpoint` uses the normal AWS
S3 regional endpoint; setting it is the compatibility knob for Cloudflare R2, MinIO, Tigris, and
other S3-compatible services. V1 uses virtual-hosted-style requests when `endpoint` is omitted
(real AWS, which has deprecated path-style for new buckets) and path-style requests when
`endpoint` is set (the S3-compatible services above). A separately typed `allow_http = true`
escape hatch may be used for a trusted local test endpoint; it defaults to `false`, and an
`http://` endpoint without it is rejected.

The nested blob config is strict even though `KhiveConfig` is forward-compatible at its top level:
unknown fields, fields for the other backend, and attempted credential fields are startup errors.
For `fs`, existing root precedence is unchanged: `KHIVE_BLOB_ROOT` environment variable,
configured `root`, then `<db_dir>/blobs`. `KHIVE_BLOB_ROOT` has no effect when `backend = "s3"`.

S3 credentials are never accepted in TOML. V1 reads `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY` as an all-or-nothing pair, plus optional `AWS_SESSION_TOKEN`, from the
process environment. Startup errors may name a missing variable but must never include a value.
Bucket, endpoint, and region come from the explicit non-secret config above and are not silently
replaced by process-global AWS endpoint variables.

`prefix` must be non-empty and canonical: no leading or trailing slash, empty segment, `.` segment,
or `..` segment. For a content hash `h`, the object key is:

```text
{prefix}/{h[0..2]}/{h[2..4]}/{h}
```

The shard shape preserves the deterministic CAS mapping used by `FsBlobStore`; it remains a backend
detail and is never stored in entity properties. The bucket used for this prefix must be
unversioned in v1. Versioning-enabled and versioning-suspended buckets are unsupported: a simple
`DELETE Object` can create a delete marker while retaining prior bytes, which does not meet this
ADR's physical-deletion and orphan-reclamation contract.

### Trait method mapping

| `BlobStore` method     | S3-compatible operation                                   | Required behavior                                                                                                                                                                                                                 |
| ---------------------- | --------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `put(bytes)`           | Client-side BLAKE3, `HEAD`, then conditional `PUT Object` | Compute `ContentRef` before network I/O. An existing key is the dedup no-op. A missing key is created with `If-None-Match: *`; a concurrent precondition failure means an identical writer won and is returned as success.        |
| `get(content_ref)`     | `GET Object`                                              | Return exact bytes. A missing key maps to `StorageError::NotFound` with capability `Blob`, resource `blob`, and the content ref as key.                                                                                           |
| `exists(content_ref)`  | `HEAD Object`                                             | Success is `true`; not-found is `false`. Authorization, timeout, and transport failures remain errors and must not masquerade as absence.                                                                                         |
| `delete(content_ref)`  | `HEAD Object`, then `DELETE Object`                       | Under the required quiescence, an absent HEAD returns `false`; a present HEAD followed by successful deletion returns `true`. The HEAD is necessary because S3 DELETE is idempotent and does not reliably report prior existence. |
| `orphan_sweep(config)` | Paginated `ListObjectsV2`, diff, bounded deletes          | List only the configured prefix, process no more than 1,000 keys per page, validate the exact shard/key form, compare to `live_refs`, and retain only page-sized remote state. Dry-run never deletes.                             |

`orphan_sweep` continues until the provider returns no continuation token. Delete request size and
concurrency are bounded; the implementation does not materialize the full remote listing. A normal
result means the complete prefix was visited. On a page or delete failure the method returns an
error rather than reporting a partial scan as complete. Keys under the prefix that do not parse as
the exact CAS shard shape are ignored, matching `FsBlobStore`'s refusal to sweep temporary or
foreign filenames.

### Concurrency and the §8 deletion hazard

`S3BlobStore` has no equivalent of `FsBlobStore`'s canonical-root write mutex. That mutex exists to
serialize a local free-space sample with a filesystem publish. S3 provides no portable
available-space gauge, and individual object PUTs are atomically published by the service.
Content-addressed keys plus conditional create replace the filesystem publish critical section for
concurrent `put` calls.

No object-store mutex replaces the §8 safety requirement because an in-process lock would not solve
it. The hazardous race is between a database snapshot of live `content_ref`s and a later entity
write, potentially across processes and storage systems. `delete` and `orphan_sweep` therefore
remain offline-maintenance-only for S3 and require deployment-wide quiescence of every writer that
can create a new entity reference for the full snapshot-plus-sweep interval. The transactional,
DB-coordinated design remains khive#924.

Out-of-band lifecycle policies have the same limitation and must not delete objects from the live
BlobStore prefix.

### Capacity guard and failure mapping

The filesystem free-space floor is not applicable remotely. There is no portable S3 API for a
race-free “capacity remaining after this write” check, so `S3BlobStore` performs no capacity probe.
A provider quota or capacity refusal is surfaced from the failed PUT and is never mapped to
`StorageError::CapacityFloor`.

| HTTP/client failure                                                                                                                              | `StorageError` mapping                                              |
| ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------- |
| GET or HEAD not-found                                                                                                                            | `NotFound` for `get`; `false` for `exists` and the pre-delete check |
| Expected conditional-create already-exists/precondition failure                                                                                  | Successful dedup for `put`                                          |
| Request deadline or transport timeout                                                                                                            | `Timeout { operation }`                                             |
| Invalid bucket, endpoint, region, prefix, or incomplete credential environment                                                                   | Startup/config error; `InvalidInput` if discovered at method scope  |
| Unexpected provider conflict                                                                                                                     | `Conflict { capability: Blob, operation, message }`                 |
| Authorization/signature rejection, TLS/DNS/connect failure, exhausted transient response, quota/capacity refusal, or malformed protocol response | `Driver { capability: Blob, operation, source }`                    |

The S3 client applies bounded exponential backoff with jitter to replay-safe requests and the
idempotent content-addressed PUT. Transient `429` and `5xx` responses are retried within that budget;
credential and authorization failures are not. After the budget, the original source remains in
`Driver` unless the overall operation deadline elapsed, which maps to `Timeout`.

### Dependency, buffering, and test decisions

The implementation uses Apache Arrow's `object_store` crate rather than `aws-sdk-s3` or a local
SigV4 client. The dependency baseline is `object_store = 0.13.2` with default features disabled and
only its `aws` feature enabled. That release's `aws` feature uses reqwest 0.12 and ring, matching
versions already present in the workspace dependency graph while excluding its filesystem and
other cloud backends. This provides a Tokio-native client, signing, retries,
conditional PUT support, custom endpoints, and streaming LIST pagination without importing its
other provider implementations. The implementation gate must record the normal dependency-tree and
stripped release-binary delta against khive's established 18 MB single-binary goal; this draft does
not claim an unmeasured byte cost. Updating the pinned dependency is allowed only with the same
feature, compatibility, and size evidence.

The existing whole-buffer trait is accepted for S3 v1 up to 64 MiB per object. `put` rejects a
larger buffer with `InvalidInput`; `get` checks returned object metadata before collecting a larger
response. Consumers must enforce the same ceiling before reading a source into memory. A streaming
amendment covering both upload and download, hash finalization, replay, multipart abort, and retry
semantics is required before khive supports larger blobs or concurrent traffic whose measured peak
memory violates a supported deployment envelope.

CI uses three layers:

1. one shared `BlobStore` conformance suite for filesystem and S3 implementations;
2. a pinned MinIO container as the required compatibility test for custom endpoint, SigV4,
   path-style addressing, conditional create, CRUD, and multi-page LIST behavior;
3. fake-client unit tests for timeout, authorization, quota, exhausted retry, and partial-page error
   mapping.

Live-provider tests are explicit, secret-gated, non-required smoke tests. A mock alone cannot prove
wire compatibility; a live service alone is too dependent on credentials and network health to be
the merge gate.

### Alternatives considered

**`aws-sdk-s3`.** It has the strongest AWS-specific service surface and supports custom endpoint and
path-style configuration. It also brings a broader generated service and Smithy runtime surface
than these five methods need. Rejected for v1 in favor of the narrower client, subject to the
required binary measurement and compatibility gate. It remains the fallback if the selected
`object_store` version cannot satisfy conditional create or endpoint compatibility.

**Minimal SigV4 client over `reqwest`.** This reduces the named dependency count but leaves khive
responsible for canonical request signing, clock skew, XML errors, pagination, retry
classification, conditional requests, and protocol security maintenance. Rejected: the apparent
dependency saving does not justify owning an S3 client.

**Streaming trait now.** This would remove the known memory ceiling, but it changes both trait
directions and introduces hash-finalization, replay, and multipart-cleanup decisions before a
consumer exceeds the 64 MiB v1 envelope. Deferred behind the explicit threshold above.

**Mock-only or live-only CI.** Mock-only does not exercise signing or endpoint behavior; live-only
is not hermetic. Rejected in favor of required MinIO plus focused fakes and optional live smoke
tests.

### Consequences

- `khive-storage` remains dependency-light and unchanged; provider types do not cross its trait
  boundary.
- `khive-db` owns both concrete blob backends and the S3 client dependency.
- The config/boot layer gains one typed blob selector and must wire the same selection through
  single- and multi-backend startup paths.
- Existing configurations continue to select `FsBlobStore` with the current root and floor
  behavior.
- S3 deployments gain off-host blob storage but must provide environment credentials, an
  unversioned bucket, and an offline maintenance window for deletion and sweep.
- `BlobStore` still does not provide transactional reference GC; this amendment does not close or
  weaken khive#924.

---

## Implementation Notes

- `crates/khive-storage/src/blob.rs` — `ContentRef`, `BlobOrphanSweepConfig`,
  `BlobOrphanSweepResult`, `BlobStore`.
- `crates/khive-storage/src/capability.rs` — `StorageCapability::Blob`.
- `crates/khive-storage/src/error.rs` — `StorageError::CapacityFloor`.
- `crates/khive-storage/src/entity.rs` — `Entity::content_ref`, `Entity::with_content_ref`.
- `crates/khive-db/src/stores/blob.rs` — `FsBlobStore`, `resolve_blob_root`,
  `write_lock_for_root`/`root_write_locks` (the canonical-root-keyed shared-lock registry),
  `crosses_floor` (the pure write-size-aware floor comparison).
- `crates/khive-db/src/backend.rs` — `StorageBackend::blob_store`.
- Amendment 2 proposes `S3BlobStore` beside `FsBlobStore` plus one config-aware boot-layer selector;
  no provider type enters `khive-storage`.
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
- PR #922, source of the three corrections this ADR was amended to address.
- PR #922 follow-up confirmed the deserialization fix and found the floor-guard fix
  incomplete (not actually per-root, not cancellation-safe) and this ADR's `ContentRef` example
  stale.
- [khive#924](https://github.com/ohdearquant/khive/issues/924) — follow-up: transactional,
  DB-coordinated `BlobStore` orphan sweep.
- [`object_store` 0.13.2 S3 builder](https://docs.rs/object_store/0.13.2/object_store/aws/struct.AmazonS3Builder.html)
  and [feature model](https://docs.rs/crate/object_store/0.13.2/features) — selected S3 client
  surface and dependency boundary for Amendment 2.
- [AWS SDK for Rust endpoint configuration](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/endpoints.html)
  — comparison evidence for custom endpoints and path-style support in the rejected SDK fork.
- [Amazon S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)
  — `If-None-Match: *` create-if-absent behavior and concurrent response semantics.
- [Deleting objects from versioning-suspended buckets](https://docs.aws.amazon.com/AmazonS3/latest/userguide/DeletingObjectsfromVersioningSuspendedBuckets.html)
  — why suspended versioning does not satisfy physical deletion.
