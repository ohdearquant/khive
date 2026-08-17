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

## `BlobStore::orphan_sweep` — concurrency hazard

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
reference is deleted anyway (see `khive-db`'s
`orphan_sweep_race_demonstrates_the_documented_quiescence_requirement` test,
which reproduces exactly this). This trait provides no transactional
coordination with an attachment writer. **Callers MUST quiesce attachment writes**
(nothing may create a new `content_ref` reference) for the duration of
snapshot-plus-sweep — a maintenance window, a single-writer admin CLI
invocation with no live traffic, or equivalent.

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

After that gate, the filesystem implementation acquires database/root ownership,
captures the complete candidate set, and classifies file age outside SQLite's
writer transaction. Ownership consists of a database-scoped process mutex,
`<database>.khive-blob-gc.lock`, and the same root-local locks as `put`. It
revalidates the epoch under ownership and validates every attachment and claim
reference before the functional fence probe. Malformed
stored evidence returns its validation error, and a malformed schema or
nonfunctional named fence returns its storage or typed `Unsupported` error;
every such path fails closed before claim recovery or deletion. The sweep then
removes validated abandoned rows—including claims copied by a backup or left
under a relocated root—in batches of at most 128. Each short
`SqlAccess::atomic_unit` anti-joins every `attachments.content_ref` role and
commits durable `blob_gc_claims`; attachment INSERT/UPDATE triggers reject a new
live reference to a claimed digest. Physical deletion runs outside SQLite while
database/root ownership remains held, followed by bounded SQL-only cleanup. A crash after
claim commit leaves the fence durable and fail-closed; the next exclusive owner
rescans rather than resuming deletion blindly. A filesystem publisher in
another process that begins while the sweep holds the advisory root lock waits;
after release, `put` rechecks the target and republishes bytes removed as an
orphan before returning the `ContentRef`. Direct filesystem mutation does not
participate in the advisory-lock protocol. Backends that cannot provide the
coordination and epoch guarantees return `Unsupported`.

### Schema epoch gate and the two-release V21 rollout

The shipped filesystem implementation selects SQL liveness only after proving
one exact completed V21 epoch: the V21 ledger row is uniquely present and latest,
the cutover marker is complete, the required attachment/claim objects and
attachment claim triggers are present, and the legacy entity
column/index/triggers are absent. Known non-admitted epochs—V20, pending,
incomplete, missing-required-object, retained-legacy, or ahead-of-V21—return
`StorageError::Unsupported` in both modes before root locking, filesystem
walking, or abandoned-claim cleanup. Malformed stored evidence or a
nonfunctional named fence may retain a more specific validation, storage, or
typed `Unsupported` error after ownership/candidate capture, but still fails
closed before claim recovery or deletion. Once admitted, every attachment role
is live and both attachment claim triggers are exercised before deletion.

This gate ships first as **Phase 4a**. That compatibility release leaves V20
schema and data untouched: no attachments, backfill, dual-read/write, or V21
ledger entry. Every process and scheduled job sharing the database/blob root
must converge on Phase 4a or newer, and every pre-Phase-4a process must be
drained and prevented from restarting, before **Phase 4b** may perform the
attachment backfill, fence swap, and legacy-column drop. Every Phase-4a
application-serving/read-write process must also be quiesced, or proven unable
to access the database, during cutover. A Phase-4a GC-only worker can safely
recognize exact completed V21, but that narrow property is not general entity
reader/writer compatibility. Start Phase-4b serving only after exact-current
topology validation. Transactional GC is intentionally unavailable while Phase
4a operates on V20; do not bypass the pause with caller-snapshot `orphan_sweep`
or unconditional `delete`.

The original `orphan_sweep` remains an offline-maintenance API for callers that
already have a trusted `live_refs` snapshot. It intentionally retains its
quiescence requirement for compatibility; concurrent callers must use
`transactional_orphan_sweep`.

Default `orphan_sweep` implementation returns `StorageError::Unsupported`; the
filesystem backend overrides it with a real directory walk. No silent no-op.
