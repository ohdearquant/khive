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

## `BlobStore::get_bounded_verified`

Whole-buffer reads declare an actual-byte maximum and use the required
`get_bounded_verified(content_ref, max_bytes)` backend primitive. The maximum
may be zero and cannot exceed `MAX_BLOB_WHOLE_BYTES` (64 MiB); larger objects
require a future streaming contract. There is deliberately no default built
from `size()` plus another read: metadata can provide an early refusal and an
integrity witness, but cannot bound or authenticate a later read.

A backend returns bytes only after the authoritative object reaches EOF at or
below the declared limit, its opened-handle/GET metadata agrees with the final
length, and BLAKE3 of those bytes equals the requested `ContentRef`. Failures
are ordered: invalid maximum, not found, `BlobTooLarge`,
`BlobSizeMismatch`, then `BlobDigestMismatch`. No partial or digest-mismatched
buffer reaches the caller.

Public unbounded whole-buffer reads do not exist. Production consumers enter
through the runtime `BlobHydrator`, which pairs this backend-enforced operation
with shared weighted admission and cancellation-safe supervision; pack-facing
raw store access is for metadata, mutation, and maintenance paths only.

## `BlobStore::delete` — concurrency hazard

`delete` performs an unconditional physical removal with **no coordination
against any record or attachment that might reference `content_ref`**. It is safe to call
only when the caller has independently ensured — outside this trait,
typically by quiescing every writer that could commit a new SQL liveness
reference — that nothing live references `content_ref` for the duration of the
call. A caller that races a reference write against a `delete` can dangle a
live reference; this trait does not detect or prevent that.

## `BlobStore::orphan_sweep` — concurrency hazard, disabled in this compatibility release

The operator-side GC path (khive#292 deliverable 5) — an admin-side
operation, not an MCP verb, mirroring `VectorStore::orphan_sweep`'s CLI-only
precedent (ADR-044). `BlobStore` has no visibility into SQL substrates
(ADR-005 constraint 4: a trait instance talks to exactly one backend), so it
cannot itself discover which content refs are still referenced by, e.g., the
`attachments.content_ref` column — the caller assembles `BlobOrphanSweepConfig.live_refs`
and passes it in.

`live_refs` is a **snapshot** the caller assembled before the call.
`orphan_sweep` has no way to detect a `content_ref` that becomes newly live
between when that snapshot was taken and when the sweep runs; such a
reference would be deleted anyway. This trait provides no transactional
coordination with an attachment writer, and — unlike
`transactional_orphan_sweep` — it has no `SqlAccess` capability with which to
prove a completed V21 attachment epoch either. **The filesystem
implementation therefore returns typed `StorageError::Unsupported` for every
call in this compatibility release, in both `dry_run` modes, regardless of
`live_refs`.** A caller-assembled snapshot could otherwise delete an object a
V20 SQL query cannot see as live (e.g. a moodboard FANN network), silently
bypassing the epoch gate `transactional_orphan_sweep` enforces. Use
`transactional_orphan_sweep` for all sweeps against a live database; there is
currently no supported destructive path through `orphan_sweep`.

A DB-coordinated sweep is available separately as
`BlobStore::transactional_orphan_sweep`. The Phase4a filesystem implementation
first performs a read-only schema-epoch gate. Both `dry_run` and destructive
calls are supported only when the database proves the named objects and epoch
markers of the exact completed V21 attachment cutover: the complete marker and
V21 ledger row, the attachment and claim tables/indexes, attachment
INSERT/UPDATE claim fences, and no legacy entity content-ref
column/index/triggers. V20, pending, incomplete, missing-required-object,
retained-legacy, and ahead-of-V21 epochs return typed
`StorageError::Unsupported` before waiting for the blob-root lock, walking
files, or recovering abandoned claims.

After that gate, the filesystem implementation acquires database ownership (the
process-local guard plus the cross-process advisory lock) and revalidates the
epoch immediately, before ever waiting on the root guard/lock or walking the
filesystem — closing the gap if external maintenance changed the schema
between the read-only preflight and ownership. Only once that recheck passes
does it acquire root ownership, capture the complete candidate set, and
classify file age outside SQLite's writer transaction, then validate every
attachment and claim reference before the functional fence probe. Malformed
stored evidence returns its validation error, and a malformed schema or
nonfunctional named fence returns its storage or typed `Unsupported` error;
every such path fails closed before claim recovery or deletion. The sweep then
removes validated abandoned rows in batches of at most 128. Each short
`SqlAccess::atomic_unit` anti-joins every `attachments.content_ref` role and
commits durable `blob_gc_claims`; attachment INSERT/UPDATE triggers reject a new
live reference to a claimed digest. Physical deletion runs outside SQLite while
ownership remains held, followed by bounded SQL-only cleanup. A crash after
claim commit leaves the fence durable and fail-closed; the next exclusive owner
rescans rather than resuming deletion blindly. Direct filesystem mutation does
not participate in the advisory-lock protocol. Backends that cannot provide the
coordination and epoch guarantees return `Unsupported`.

Phase4a changes only this GC reader/fence behavior. It does not create
attachments, register or run V21, backfill data, drop `entities.content_ref`, or
make Phase4a application serving compatible with a V21 database. The positive
mixed-version guarantee is deliberately narrower: an already-running Phase4a
GC implementation can interpret an exact completed V21 attachment epoch. The
Phase4b cutover still requires every Phase4a application reader and writer to be
quiesced first.

The original caller-snapshot `orphan_sweep` is disabled on the filesystem
backend in this compatibility release (see above); it is not an alternative
path for callers who want to avoid the epoch gate. All sweeps against a live
database must go through `transactional_orphan_sweep`.

Default `orphan_sweep` implementation returns `StorageError::Unsupported`; the
filesystem backend currently returns the same typed refusal rather than
performing a real directory walk. No silent no-op.
