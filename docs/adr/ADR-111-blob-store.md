# ADR-111: BlobStore — Content-Addressed Binary Object Storage

**Status**: accepted
**Date**: 2026-07-12 (amended 2026-07-13, PR #922; Amendment 2 accepted and implemented
2026-07-17, PR #1054; Amendment 3
accepted 2026-07-17; Amendment 4 accepted 2026-07-19; attachment-GC compatibility epoch proposed
2026-08-16 by ADR-160)
**Authors**: khive maintainers
**Amended by**: proposed [ADR-160](ADR-160-shared-pack-infrastructure.md), which requires
backend-enforced bounded and digest-verified reads, retires public unbounded `get`, and introduces
the two-release attachment-GC compatibility gate on acceptance.
**Depends on**:

- [ADR-005](ADR-005-storage-capability-traits.md) — Storage Capability Traits (trait-only capability
  surface this ADR extends with a ninth capability)
- [ADR-015](ADR-015-schema-migrations.md) — Schema Migrations (the versioned migration this ADR uses
  to add `entities.content_ref`)
- [ADR-044](ADR-044-vector-store-extensions.md) — Vector Store Extensions (the `orphan_sweep`
  CLI-only precedent this ADR mirrors for `BlobStore`)
  **Related**: [ADR-086](ADR-086-doc-file-pack.md) — proposed Doc/File Pack that deferred
  `StorageCapability::Blob` to a real consumer; it motivates this accepted storage capability
  but is not its prerequisite.

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
    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>>;
    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool>;
    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>>;
    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool>;
    async fn orphan_sweep(
        &self,
        config: &BlobOrphanSweepConfig,
    ) -> StorageResult<BlobOrphanSweepResult> { /* default: Unsupported */ }
}
```

As amended by ADR-160, `get_bounded_verified` is the only whole-buffer read and returns
`StorageError::NotFound` (capability `Blob`) for an absent reference. It enforces the caller's
actual-byte maximum and authenticates BLAKE3 before returning bytes. `delete` returns `Ok(false)`
(not an error) when nothing existed to remove — deleting an absent object is not a failure.
`orphan_sweep` defaults to `StorageError::Unsupported`, following `VectorStore`'s precedent
(ADR-044): a backend opts in by overriding it.

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

**Concurrency guarantee: choose the API deliberately (amended 2026-07-23, PR #1313).** The
paragraph above, as originally written, claimed this design "is never removed out from under a
concurrent reader". That remains false for `delete` and the caller-snapshot
`orphan_sweep(config)` API. Both are **offline-maintenance-only**, not safe to run against a live
reference writer:

- `orphan_sweep`'s `live_refs` set is a **snapshot** the caller assembles before the call. Nothing
  in `BlobStore` detects a `content_ref` that becomes newly live — a reference write lands
  referencing it — between when that snapshot was taken and when the sweep runs; such a blob would
  be deleted anyway. As of the amendment below, the filesystem backend closes this specific
  destructive path in this compatibility release by refusing every call outright, rather than
  narrowing the hazard.
- `delete` is an unconditional physical removal with the same class of hazard: any caller can
  delete a `content_ref` a reference write races into existence a moment later, with no coordination
  from this trait.

Run `delete` only when writes that could create a new `content_ref` reference are quiesced.
`BlobStore::transactional_orphan_sweep(sql, dry_run)`, added by PR #1313, is the live-traffic
alternative for backends that can coordinate both stores.

**Caller-snapshot `orphan_sweep` disabled on the filesystem backend (amended 2026-08-21).** This
API has no `SqlAccess` capability of its own, so — unlike `transactional_orphan_sweep` — it cannot
prove a completed V21 attachment epoch before deleting anything. A caller-assembled `live_refs`
snapshot could delete an object a V20 SQL query cannot see as live (e.g. a moodboard FANN network),
bypassing the epoch gate below entirely. `FsBlobStore::orphan_sweep` therefore returns typed
`StorageError::Unsupported` for every call in this release, in both `dry_run` modes, matching the
trait default; there is no destructive path through this method until a snapshot API can carry its
own epoch proof. This does not affect `S3BlobStore`, whose `orphan_sweep` remains the
offline-maintenance-only quiescence-required path described near the end of this section — the
disablement is filesystem-specific because only the filesystem backend has a competing,
epoch-gated `transactional_orphan_sweep` implementation to defer to.

**Attachment-cutover compatibility epoch (amended 2026-08-16, Phase4a).** The filesystem
implementation no longer sweeps against V20 entity liveness. Both report-only and destructive
calls first require the named objects and markers of the exact completed V21 attachment epoch: the
durable complete marker and V21 ledger row; attachment and claim tables/indexes; attachment
INSERT/UPDATE claim fences; and no legacy `entities.content_ref` column, index, or triggers. V20,
pending, incomplete, missing-required-object, retained-legacy, and ahead-of-V21 epochs return typed
`StorageError::Unsupported` before root locking, filesystem walking, or abandoned-claim cleanup.
Malformed table/evidence reads fail with their validation or storage error, and a nonfunctional
named fence returns typed `Unsupported`; all fail closed before claim cleanup or deletion.
`dry_run` does not weaken this contract.

Once admitted, the filesystem implementation:

1. acquires database ownership (the process-local guard plus the cross-process advisory lock) and
   immediately rechecks the completed epoch, before ever waiting on the root guard/lock or walking
   the filesystem — this closes the gap if external maintenance changed the schema between the
   read-only preflight above and ownership, and keeps the pre-lock refusal contract true for every
   listed epoch, not only the one the preflight observed;
2. only once that recheck passes, acquires root ownership, captures the complete blob candidate set
   while publishers are excluded, evaluates file age outside SQLite's writer transaction, validates
   every attachment and claim reference, and then proves the attachment INSERT/UPDATE fences
   function before any abandoned-claim cleanup;
3. removes validated claims abandoned by the previous database owner in SQL-only transactions of
   at most 128 rows; ownership, not the mutable path-derived `root_key`, makes root relocation and
   restored-backup recovery safe;
4. for each candidate batch of at most 128, enters a short, SQL-only `SqlAccess::atomic_unit`
   (`BEGIN IMMEDIATE` on SQLite), anti-joins every `attachments.content_ref` role, and durably
   claims only absent candidates in `blob_gc_claims`;
5. after that transaction commits, deletes only the claimed batch while retaining ownership;
   attachment INSERT/UPDATE triggers reject a newly live reference to any active claim; and
6. removes that bounded claim batch in a second short SQL-only atomic unit before advancing, then
   releases all locks after the final batch.

This yields two concurrency guarantees pinned by tests. A blob published after candidate capture
is not in the sweep set and survives. A committed attachment reference cannot appear between the
liveness query and physical deletion: SQLite's writer lock covers the anti-join plus claim commit,
then the durable trigger fence covers the external deletion phase without monopolizing SQLite's
single writer. Invalid stored attachment or claim refs fail closed before the functional probe,
claim recovery, or deletion. A crash after claim commit leaves a durable fail-closed row; the next
exclusive database owner rescans the current root and liveness state, clears abandoned claims in
bounded units, and freshly claims any still-eligible work. At no point does one writer transaction
bind, mutate, or return more than 128 candidates, bounding claim-table and WAL work per writer hold.

The guarantee still has a bounded publish gap. `put(bytes)` and the later attachment write that
stores its returned reference are separate client steps, outside one shared transaction. A
candidate with no committed reference is therefore protected by file age: `FsBlobStore` defaults
to a one-hour grace period, treats an unknown age as protected, and refreshes the mtime on a
deduplicated `put`. A client whose put-to-reference gap exceeds the configured grace remains
exposed to deletion. Tests cover a fresh unreferenced publish, deduplicated republication,
zero-grace behavior, and deletion after grace expiry; the warning is not weakened beyond that
evidence.

Phase4a is only the compatibility fence. It does not create `attachments`, register or execute
V21, backfill records, drop the legacy column, or make application serving V21-compatible. Before
a later Phase4b cutover, operators first drain and restart-fence every pre-Phase4a binary, deploy
Phase4a everywhere, and then quiesce every Phase4a application reader/writer. Only the GC method is
mixed-V21 compatible; the Phase4b migration and serving changes are a separate follow-up.

`S3BlobStore` does not override `transactional_orphan_sweep` and returns `Unsupported`; its
caller-snapshot `orphan_sweep` has no publish-grace accounting and remains offline-maintenance-
only. Unconditional `delete` remains offline-maintenance-only on both backends.

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
  offline-maintenance-only, requiring quiesced entity writes — a real concurrency hazard the
  original §8 text incorrectly described as absent. PR #1313 later added the distinct filesystem
  `transactional_orphan_sweep` path described in §8; it did not make these legacy methods safe.
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

**Status:** accepted and implemented. PR
[#1054](https://github.com/ohdearquant/khive/pull/1054) merged on 2026-07-17 as
`439b7d30561710c5272e76bd5b3e7e836caacca2`, adding `S3BlobStore`, strict
`[storage.blob]` selection, single- and multi-backend boot wiring, shared conformance tests,
fake-client failure tests, and the MinIO compatibility job. The filesystem decision and provider-
neutral `BlobStore` boundary remain in force.

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

| `BlobStore` method                             | S3-compatible operation                                   | Required behavior                                                                                                                                                                                                                                              |
| ---------------------------------------------- | --------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `put(bytes)`                                   | Client-side BLAKE3, `HEAD`, then conditional `PUT Object` | Compute `ContentRef` before network I/O. An existing key is the dedup no-op. A missing key is created with `If-None-Match: *`; a concurrent precondition failure means an identical writer won and is returned as success.                                     |
| `get_bounded_verified(content_ref, max_bytes)` | One `GET Object`                                          | Enforce the caller limit while streaming, require response metadata to match final length, and authenticate BLAKE3 before returning bytes. A missing key maps to `StorageError::NotFound` with capability `Blob`, resource `blob`, and the content ref as key. |
| `exists(content_ref)`                          | `HEAD Object`                                             | Success is `true`; not-found is `false`. Authorization, timeout, and transport failures remain errors and must not masquerade as absence.                                                                                                                      |
| `delete(content_ref)`                          | `HEAD Object`, then `DELETE Object`                       | Under the required quiescence, an absent HEAD returns `false`; a present HEAD followed by successful deletion returns `true`. The HEAD is necessary because S3 DELETE is idempotent and does not reliably report prior existence.                              |
| `size(content_ref)`                            | `HEAD Object`                                             | Returns `Some(size)` from the HEAD response's content length, or `None` when the HEAD reports the key absent. See Amendment 3.                                                                                                                                 |
| `orphan_sweep(config)`                         | Paginated `ListObjectsV2`, diff, bounded deletes          | List only the configured prefix, process no more than 1,000 keys per page, validate the exact shard/key form, compare to `live_refs`, and retain only page-sized remote state. Dry-run never deletes.                                                          |

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
write, potentially across processes and storage systems. `delete` and the caller-snapshot
`orphan_sweep` therefore remain offline-maintenance-only for S3 and require deployment-wide
quiescence of every writer that can create a new entity reference for the full snapshot-plus-sweep
interval. PR #1313 implemented `transactional_orphan_sweep` only for `FsBlobStore`;
`S3BlobStore` retains the trait default (`Unsupported`) and does not inherit the filesystem grace-
period guarantee.

Out-of-band lifecycle policies have the same limitation and must not delete objects from the live
BlobStore prefix.

### Capacity guard and failure mapping

The filesystem free-space floor is not applicable remotely. There is no portable S3 API for a
race-free “capacity remaining after this write” check, so `S3BlobStore` performs no capacity probe.
A provider quota or capacity refusal is surfaced from the failed PUT and is never mapped to
`StorageError::CapacityFloor`.

| HTTP/client failure                                                                                                                              | `StorageError` mapping                                                               |
| ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------ |
| GET or HEAD not-found                                                                                                                            | `NotFound` for `get_bounded_verified`; `false` for `exists` and the pre-delete check |
| Expected conditional-create already-exists/precondition failure                                                                                  | Successful dedup for `put`                                                           |
| Request deadline or transport timeout                                                                                                            | `Timeout { operation }`                                                              |
| Invalid bucket, endpoint, region, prefix, or incomplete credential environment                                                                   | Startup/config error; `InvalidInput` if discovered at method scope                   |
| Unexpected provider conflict                                                                                                                     | `Conflict { capability: Blob, operation, message }`                                  |
| Authorization/signature rejection, TLS/DNS/connect failure, exhausted transient response, quota/capacity refusal, or malformed protocol response | `Driver { capability: Blob, operation, source }`                                     |

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
stripped release-binary delta against a freshly measured `main` baseline; this draft does
not claim an unmeasured byte cost. Updating the pinned dependency is allowed only with the same
feature, compatibility, and size evidence.

**Baseline note (2026-07-16, #1055):** the "18 MB single-binary goal" cited in earlier drafts and
in other khive docs is stale — a stripped release `kkernel` at `main` (pre-#1054) measured
21,654,816 bytes (20.65 MiB); #1054 itself adds only +67,312 bytes (+0.3%), so the growth predates
and is unrelated to this ADR's S3 backend. There is no re-derived hard budget yet; until one is set,
size-delta reviews for this and future backends must compare against a freshly measured `main`
binary, not the old 18 MB figure.

The bounded whole-buffer trait is accepted for S3 v1 up to 64 MiB per object. `put` rejects a
larger buffer with `InvalidInput`; `get_bounded_verified` checks both returned metadata and actual
streamed bytes, then authenticates the complete body. Production consumers additionally enter
through runtime weighted admission. A streaming amendment covering upload and download, hash
finalization, replay, multipart abort, and retry semantics is required before khive supports larger
blobs or traffic whose measured peak memory violates a supported deployment envelope.

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
  unversioned bucket, and an offline maintenance window for deletion and caller-snapshot sweep.
- The later provider-neutral `transactional_orphan_sweep` trait method does not imply S3 support:
  the S3 backend returns `Unsupported`, while the filesystem backend implements the coordinated
  guarantee and grace-period boundary described in §8.

---

## Amendment 3 (2026-07-17): `size` accessor

**Status:** accepted.

### Context and decision

The blob verb surface's `blob.stat` verb answers "does this object exist, and how big is it"
without any need for the object's bytes. Before this amendment, `BlobStore` exposed no
size-only accessor: `exists` answers presence only, and the only way to learn an object's
length was `get`, which hydrates the full object into memory. `blob.stat` and `blob.get`'s
own pre-fetch bound check both needed a metadata-only answer, so this amendment adds it to
the trait rather than layering a second full-read workaround on top of `get`.

`BlobStore` gains:

```rust
async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>>;
```

`Ok(None)` means no object exists for this reference — this is the existence check and the
size read in one call, so a caller never pays for a full read just to answer "does this exist
and how big is it". `FsBlobStore` answers `size` from filesystem metadata (`stat`, not `read`).
`S3BlobStore` answers it from the same `HEAD Object` request `exists` already issues, reading
the response's content length instead of discarding it (see the trait method mapping table in
Amendment 2). Both implementations map a not-found response to `Ok(None)`, not an error.

`size` is a required trait method — this is a two-implementation trait (`FsBlobStore`,
`S3BlobStore`), and a metadata-only size accessor is meaningful for either backend, so there is
no principled default to fall back on.

Downstream, the `blob.stat` verb handler answers directly from `size` and never reads or
digest-verifies the object. As amended by ADR-160, `blob.get` performs its whole-buffer read through
the shared runtime hydrator and the backend verifies the digest before bytes reach the handler;
`size` remains a cheap preflight for response/range refusal, not the hydration authority.

### Consequences

- `BlobStore` implementers must provide a metadata-only size accessor; both implementations
  in this crate already have direct access to the required information (`stat`/`HEAD`).
- `blob.stat` no longer reports whether a stored object's bytes match its own `ContentRef`
  (the `corrupt` field is removed); the bounded verified read used by `blob.get` performs that
  check.
- No schema, wire-format, or `ContentRef` change; this amendment is additive to the trait
  surface only.

---

## Amendment 4 (2026-07-19): capability-by-hash authorization model

**Status:** accepted.

### Context

`blob.get` and `blob.stat` accept a `ContentRef` and resolve it against the global
content-addressed store; the handlers receive a `NamespaceToken` and do not consult it.
Review raised this as an authorization gap. This amendment records the intended model so
the behavior is normative rather than re-litigated per review.

### Decision: possession of a ref is the read capability

The blob surface uses **capability-by-hash**: a `ContentRef` is a 256-bit BLAKE3 digest of
the content it names, and possession of a valid ref is the authorization to read that
content. This is the access model of git object stores and comparable content-addressed
systems, and it is the deliberate composition of three accepted contracts:

1. **Namespace is attribution, not isolation** (ADR-007 Rev 6). The namespace stamp
   records who wrote a record; it is never a storage boundary, and by-ref reads are
   namespace-agnostic exactly as by-ID reads are.
2. **Authorization lives at the single dispatch gate** (ADR-053), never inside individual
   handlers. A per-handler namespace check on the blob path would re-introduce the
   handler-level authorization pattern this architecture removed.
3. **Global deduplication is a design goal of this store** (§2, §4). Identical content is
   one object regardless of who stored it; a namespace-partitioned blob keyspace would
   forfeit that property.

A caller cannot enumerate refs: learning a valid `ContentRef` requires having stored the
content, having been handed the ref, or possessing candidate bytes from which the ref is
computable. For **high-entropy content** this last path requires already possessing the
exact bytes, so `blob.get` reveals nothing its caller could not produce. For
**low-entropy content** the derivation path is weaker — a caller can hash a dictionary of
candidate payloads and probe each ref — and the model's guarantees are correspondingly
weaker; the residue analysis below treats this case explicitly rather than claiming
unguessability where none exists.

### The `blob.stat` existence residue

Under global dedup, `blob.stat` is an existence oracle. For **high-entropy content** the
residue is precisely bounded: a caller who already fully possesses the bytes can learn one
bit — whether the store also holds them (plus their size, which the caller already knows).
It confirms; it never discloses.

For **low-entropy content** the residue is materially larger, and this amendment names it
rather than defining it away: a caller holding a candidate dictionary (form letters, small
structured records, enumerable identifiers) can hash each candidate and probe `blob.stat`,
and confirmation across a candidate set _is_ disclosure — the caller learns which
candidate was stored. This is the confirmation-of-content attack known from cross-user
deduplication systems, and `blob.get` on a confirmed guessable ref retrieves the object.
Two rules follow:

1. **Single-user local deployment**: the residue is accepted — there is one principal, and
   every object in the store was placed there by that principal's own runtime.
2. **Multi-tenant hosted deployment**: the `(namespace, ContentRef)` put-ledger at the
   gate (ADR-053) is **mandatory at that tier, not optional**: `blob.get` and `blob.stat`
   answer only for refs the calling tenant has put (or been granted), which closes both
   the retrieval and the confirmation channel for guessable content while the storage
   layer keeps physical dedup. This remains gate policy — no handler-level check may be
   added in its place.

Additionally, callers storing **sensitive low-entropy content** in any deployment should
envelope it (client-side encryption or keyed salting) before `blob.put` — which also makes
the resulting ref high-entropy and restores the bounded residue. This is a documented
usage rule for consumers, not a runtime enforcement point.

### Ref-leak hygiene (binding)

Because a ref is a bearer capability for its content, a leaked `ContentRef` is leaked
content access. Refs are treated accordingly: they must not appear on public or
unauthenticated surfaces — public issue text, published logs, error messages returned to
unauthenticated callers, or telemetry that leaves the deployment boundary.

### Consequences

- `blob.get`/`blob.stat` remain namespace-agnostic by design in single-user local
  deployment; reviews should evaluate them against this model, not against a
  per-namespace isolation expectation.
- Multi-tenant hosted deployment REQUIRES the gate-level `(namespace, ContentRef)`
  put-ledger — the capability-by-hash model alone is insufficient there because
  low-entropy refs are derivable from candidate dictionaries. Storage-level dedup is
  preserved either way.
- Sensitive low-entropy content is enveloped client-side before `blob.put` (usage rule).
- No trait, schema, or wire change; this amendment is documentation of intent.

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
- `crates/khive-db/src/stores/blob_s3.rs` — shipped `S3BlobStore` implementation from PR #1054.
- `crates/khive-runtime/src/blob.rs`, `crates/khive-runtime/src/engine_config.rs`, and
  `crates/khive-mcp/src/serve.rs` — config-aware blob-store selection and boot wiring from PR #1054;
  no provider type enters `khive-storage`.
- `crates/khive-storage/src/blob.rs` and `crates/khive-db/src/stores/blob.rs` — provider-neutral
  transactional sweep contract and filesystem implementation from PR #1313.
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
- [khive#924](https://github.com/ohdearquant/khive/issues/924) — transactional,
  DB-coordinated `BlobStore` orphan sweep, closed by PR
  [#1313](https://github.com/ohdearquant/khive/pull/1313) on 2026-07-23
  (`c716e9821ba60f444d0400b3004893c2f4c175e5`).
- [khive#1145](https://github.com/ohdearquant/khive/issues/1145) — capability-by-hash
  authorization model and `blob.stat` oracle posture (Amendment 4).
- [`object_store` 0.13.2 S3 builder](https://docs.rs/object_store/0.13.2/object_store/aws/struct.AmazonS3Builder.html)
  and [feature model](https://docs.rs/crate/object_store/0.13.2/features) — selected S3 client
  surface and dependency boundary for Amendment 2.
- [AWS SDK for Rust endpoint configuration](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/endpoints.html)
  — comparison evidence for custom endpoints and path-style support in the rejected SDK fork.
- [Amazon S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)
  — `If-None-Match: *` create-if-absent behavior and concurrent response semantics.
- [Deleting objects from versioning-suspended buckets](https://docs.aws.amazon.com/AmazonS3/latest/userguide/DeletingObjectsfromVersioningSuspendedBuckets.html)
  — why suspended versioning does not satisfy physical deletion.
