# Blob store — content-addressed binary object CRUD

`BlobStore` (`src/blob.rs`) is the trait family added by khive#292 for bytes
that do not belong inside the primary SQLite database (source PDFs, images,
large opaque payloads). Per ADR-005's "zero implementations" constraint this
module defines the contract only — the first backend (filesystem,
BLAKE3-addressed) lives in `khive-db`.

## `ContentRef`

An opaque, content-addressed reference backed by a lowercase-hex BLAKE3
digest: identical content always produces the same `ContentRef`, so storing
the same bytes twice is a no-op after the first write. Callers must treat the
value as opaque — the backend, not the caller, decides how a `ContentRef`
maps to physical storage (a filesystem path, an object-store key, etc.).

`Deserialize` is hand-written, routing every input through `ContentRef::from_hex`
— deriving it under `#[serde(transparent)]` would construct a `ContentRef`
from any string, including one that is not 64 lowercase hex characters, and
an unvalidated value reaching `shard_path`'s `[0..2]`/`[2..4]` slices panics.
`deserialize_rejects_short_string` in the test module is the exact repro this
guards against.

`from_hex` rejects uppercase rather than lowercase-normalizing it, to keep a
single canonical string form per digest — the value doubles as a filesystem
path component in the shipped filesystem backend, so accepting both cases
would let two `ContentRef` values that compare unequal as `String`s resolve
to the same bytes.

`blake3_hash_of_empty` (test helper) hand-rolls the one known `BLAKE3("")`
vector instead of pulling in the `blake3` crate, since khive-storage has zero
heavy dependencies (ADR-005).

## `BlobStore::delete` — concurrency hazard

`delete` performs an unconditional physical removal with **no coordination
against any entity that might reference `content_ref`**. It is safe to call
only when the caller has independently ensured — outside this trait,
typically by quiescing whatever writer could attach a new `content_ref` to an
entity — that nothing live references `content_ref` for the duration of the
call. A caller that races an entity write against a `delete` can dangle a
live reference; this trait does not detect or prevent that.

## `BlobStore::orphan_sweep` — concurrency hazard

The operator-side GC path (khive#292 deliverable 5) — an admin-side
operation, not an MCP verb, mirroring `VectorStore::orphan_sweep`'s CLI-only
precedent (ADR-044). `BlobStore` has no visibility into SQL substrates
(ADR-005 constraint 4: a trait instance talks to exactly one backend), so it
cannot itself discover which content refs are still referenced by, e.g., the
`entities.content_ref` column — the caller assembles `BlobOrphanSweepConfig.live_refs`
and passes it in.

`live_refs` is a **snapshot** the caller assembled before the call.
`orphan_sweep` has no way to detect a `content_ref` that becomes newly live
between when that snapshot was taken and when the sweep runs; such a
reference is deleted anyway (see `khive-db`'s
`orphan_sweep_race_demonstrates_the_documented_quiescence_requirement` test,
which reproduces exactly this). This trait provides no transactional
coordination with an entity writer. **Callers MUST quiesce entity writes**
(nothing may create a new `content_ref` reference) for the duration of
snapshot-plus-sweep — a maintenance window, a single-writer admin CLI
invocation with no live traffic, or equivalent.

A DB-coordinated sweep is available separately as
`BlobStore::transactional_orphan_sweep`. The filesystem implementation acquires
the same process-local mutex and root-local advisory file lock as `put`, captures
a bounded candidate set, then selects every non-deleted entity's `content_ref`
and deletes only those candidates inside one `SqlAccess::atomic_unit` writer
transaction. Entity writes therefore cannot change liveness during the
anti-join. A filesystem publisher in another process that begins while the
sweep holds the advisory lock waits; after the sweep releases the lock, `put`
rechecks the target and republishes bytes removed as an orphan before returning
the `ContentRef`. This coordination applies to publishers using `FsBlobStore`;
direct filesystem mutation does not participate in the advisory-lock protocol.
Backends that cannot provide both coordination boundaries return `Unsupported`.

The original `orphan_sweep` remains an offline-maintenance API for callers that
already have a trusted `live_refs` snapshot. It intentionally retains its
quiescence requirement for compatibility; concurrent callers must use
`transactional_orphan_sweep`.

Default `orphan_sweep` implementation returns `StorageError::Unsupported`; the
filesystem backend overrides it with a real directory walk. No silent no-op.
