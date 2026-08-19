# ADR-160: Shared Pack Infrastructure Program

**Status**: accepted\
**Date**: 2026-08-16\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-003](ADR-003-system-architecture.md) — dependency direction and runtime/pack separation
- [ADR-005](ADR-005-storage-capability-traits.md) — backend-neutral storage capabilities
- [ADR-006](ADR-006-deterministic-scoring.md) — deterministic ranking and score behavior
- [ADR-007](ADR-007-namespace.md) — namespace as attribution and read scope, not authorization
- [ADR-012](ADR-012-retrieval-composition.md) — high-level retrieval composition ownership
- [ADR-018](ADR-018-authorization-gate.md) — the Gate as the authorization seam
- [ADR-030](ADR-030-retrieval-stack-port.md) — public retrieval primitives and crate boundary
- [ADR-073](ADR-073-pack-core-backend-accessor.md) — canonical main/core routing from pack runtimes
- [ADR-111](ADR-111-blob-store.md) — content-addressed blob storage and the 64 MiB v1 envelope
- [ADR-119](ADR-119-daemon-component-supervision.md) — host-tracked background work and drain
- [ADR-129](ADR-129-fail-closed-gate-default.md) — Gate infrastructure failure refuses dispatch

**Amends**:

- [ADR-005](ADR-005-storage-capability-traits.md) — add a required actual-byte-bounded verified blob
  read and bind vector capability operations to complete embedding-space identity
- [ADR-111](ADR-111-blob-store.md) — require actual-byte-bounded, digest-verified blob reads and
  retire public unbounded hydration
- [ADR-030](ADR-030-retrieval-stack-port.md) — add policy-free ranked-prefix materialization and
  pure query-variant fusion primitives
- [ADR-031](ADR-031-multi-engine-retrieval.md) and
  [ADR-043](ADR-043-embedding-model-migration.md), plus
  [ADR-044](ADR-044-vector-store-extensions.md) — bind vector operations and spaces to complete
  immutable provider identity
- [ADR-047](ADR-047-knowledge-pack.md) — add an operator-opt-in intent-rephrase retrieval path
- [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) and
  [ADR-107](ADR-107-memory-ann-lifecycle.md) — key ANN lifecycle state by the same complete
  embedding-space identity
- [ADR-121](ADR-121-attachments-first-class.md) — place the one attachment/GC-liveness authority
  on the canonical main backend and make its coordinated cutover boot-gated and resumable
- [ADR-148](ADR-148-moodboard-visual-retrieval-pack.md) — consume shared hydration, attachment,
  identity, fusion, materialization, and checkpoint seams
- [ADR-149](ADR-149-moodboard-preference-learning.md) — move its deterministic numerical core to
  Lattice while preserving byte/event identities, and anchor the existing FANN object under
  ADR-121 role `"fann-network"` with authenticated cross-checks

**Supersedes**: [ADR-155](ADR-155-pack-artifact-ingest-blobstore.md) in full on acceptance.\
**Related**: proposed [ADR-156](ADR-156-named-vector-restart-durability.md), whose restart and
honest-underfill concerns remain useful but whose identity and materialization assumptions must be
rebased on this record before ratification.

---

## Context

The opt-in moodboard pack is the first consumer to combine binary artifacts, model-attested
embeddings, exact vector retrieval, and learned pairwise preference. To make those operations safe,
it implemented several mechanisms locally:

- source reads that preflight object size and verify BLAKE3 after hydration;
- a private blob-read semaphore independent of the blob pack's semaphore;
- a complete descriptor fingerprint used as a physical vector-space fence;
- per-namespace vector searches followed by deterministic best-hit merge;
- candidate materialization that skips stale rows, checks record and attachment eligibility, and
  returns an honest short result rather than padding;
- deterministic Bradley--Terry fitting, calibration, and FANN materialization; and
- opened-handle checkpoint snapshotting with a stronger whole-tree digest than the underlying model
  loader otherwise requires.

These are not all the same kind of reuse. Some are backend safety requirements, some are pure
retrieval mechanisms, some are runtime resource coordination, and some remain model- or pack-owned
policy. Moving the whole loops into one “shared helper” would collapse the architecture boundaries
established by ADR-003, ADR-005, ADR-007, ADR-012, and ADR-018.

The current implementation also exposes two correctness gaps that a mechanical extraction would
preserve:

1. `size()` followed by whole-buffer `get()` is not a hard allocation bound. The filesystem
   backend calls `fs::read`; an object that is already large, or grows after the preflight, is
   allocated in full before a caller can reject it. S3 bounds the streamed body, but the trait does
   not require every backend to do so.
2. A semaphore permit owned by the awaiting future is released when that future is canceled even
   though an already-started filesystem `spawn_blocking` read can continue. Admission therefore no
   longer represents live native work.

There is an additional desired-state inconsistency. Accepted ADR-121 replaces the single
`entities.content_ref` column with role-keyed attachments and backfills the legacy value under role
`"content"`. ADR-148 still describes ADR-121 as unratified, and proposed ADR-155 generalizes pack
artifact ingest around the legacy column while explicitly deferring backend-bounded reads. A new
shared contract must converge on ADR-121 rather than create a second legacy consumer.

Finally, a related missing capability is now practical: a small local model can generate bounded
intent-preserving query variants before embedding. The provider lifecycle and the pure fusion
mechanism belong at different layers, and the existing query IR is not an executor. This record
defines the boundary and the first opt-in knowledge consumer without placing model inference inside
the IR or making an experimental Lattice crate a mandatory runtime dependency.

## Decision

### D1 — One program, explicit ownership seams

This program extracts only mechanisms whose invariants can be stated independently of moodboard
domain policy. Ownership is fixed as follows:

| Layer             | Shared responsibility                                                                                                                                                 | Responsibility retained above it                                                         |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| `khive-storage`   | Required bounded-and-verified blob read; typed storage failures; immutable `EmbeddingSpaceIdentity`                                                                   | Admission, Gate decisions, visible read scope, model descriptor construction             |
| Concrete backends | Enforce actual bytes read and compute the content digest in the same operation; persist vectors under the complete space key                                          | Pack eligibility and provider selection                                                  |
| `khive-fusion`    | Existing deterministic union fusion                                                                                                                                   | Candidate hydration and eligibility                                                      |
| `khive-retrieval` | Policy-free ranked-prefix materialization; pure query-variant fusion and attribution                                                                                  | Runtime handles, Gate, namespace tokens, records, attachments, blobs, provider lifecycle |
| `khive-runtime`   | Gate enforcement before pack dispatch; shared blob admission/supervision; atomic provider-and-space binding; query-expander lifecycle, deadline, cache, and telemetry | Media validation, pack provenance, model-specific descriptor documents                   |
| Packs             | Visible read scope; record/subtype/attachment eligibility; error policy; public response and provenance                                                               | Gate enforcement, backend read mechanics, and generic numerical optimization             |
| Lattice           | Model-loader trust mechanics and the deterministic pairwise numerical artifact                                                                                        | Khive actor/board scope, events, attachment publication, and wire contracts              |

No shared API introduced by this record may import a higher layer merely to reuse its types. In
particular, `khive-retrieval` does not gain dependencies on `khive-runtime`, `khive-gate`,
`NamespaceToken`, `BlobStore`, `Entity`, or `Note` for the mechanisms in this record.

The implementation lands as the independently green phases in D10. A phase may not retain both
old and new public paths at program completion. Temporary branch-local comparison code and
feature-gated rollback are removed after parity; they are not compatibility surfaces.

### D2 — BlobStore requires an actual-byte-bounded verified read

`BlobStore` replaces whole-buffer `get` with this required method:

```rust
async fn get_bounded_verified(
    &self,
    content_ref: &ContentRef,
    max_bytes: u64,
) -> StorageResult<Vec<u8>>;
```

There is no default implementation composed from `size()` and `get()`. Such a default could check
metadata but could not prevent an implementation from allocating or transferring more than the
limit. `max_bytes` may be zero, allowing only an empty object, and may not exceed ADR-111's
64 MiB portable whole-buffer envelope. A larger request is `InvalidInput`; larger object support
still requires ADR-111's streaming amendment.

The method has one success condition: the backend obtained at most `max_bytes` actual bytes, the
complete object ended within that bound, and BLAKE3 of the returned bytes equals `content_ref`.
No bytes escape on a digest failure. Metadata size is an early-refusal hint and integrity witness,
not the hard bound.

`StorageError` gains typed blob variants carrying the content reference and measured values:

```text
BlobTooLarge { content_ref, max_bytes, observed_at_least }
BlobSizeMismatch { content_ref, metadata_bytes, actual_bytes }
BlobDigestMismatch { expected, actual }
```

Backends implement the same semantic operation:

- `FsBlobStore` opens the derived object path once, reads through that handle with a
  `max_bytes + 1` limit, detects a non-EOF byte as `BlobTooLarge`, compares opened-handle metadata
  with final length, and hashes the accepted bytes. It never calls `fs::read` for this path.
- `S3BlobStore` performs one GET, uses one deadline for request plus body consumption, rejects
  over-bound metadata early, checks cumulative body length before appending each chunk, compares
  metadata with final length, and hashes that same stream. It does not issue HEAD then GET as the
  integrity authority.
- Test and future backends pass the same conformance suite; “bounded” is not an advisory method
  name that an implementation may satisfy with a post-allocation check.

Failure precedence is deterministic across backends: argument validation, including a maximum above
the portable envelope, occurs before backend work; for a valid request, not-found wins before object
inspection; metadata or streamed bytes beyond the caller limit produce `BlobTooLarge`; after
bounded EOF, a metadata/final-length disagreement produces `BlobSizeMismatch`; only a
size-consistent bounded body is hashed, and a wrong hash produces `BlobDigestMismatch`.

`size()` remains a metadata/stat capability. It may be used for a cheap refusal or response
metadata, but callers cannot use it as proof that a subsequent read is bounded or unchanged.

After all consumers in this program migrate, public unbounded `BlobStore::get` is deleted. Khive
does not keep a compatibility alias or an unsafe escape hatch.

### D3 — Runtime owns shared byte admission and cancellation lifetime

Every installed blob-store service is paired with exactly one shared `Arc<BlobHydrator>` and that
same hydrator is supplied to all core and pack runtime handles sharing the store. It is not a
process-global static and is not recreated per pack. Different installed stores have independent
budgets.

The hydrator exposes the runtime operation conceptually as:

```rust
async fn hydrate_verified(
    &self,
    content_ref: &ContentRef,
    max_bytes: u64,
) -> RuntimeResult<VerifiedBlob>;
```

It reserves `max_bytes` from a weighted raw-byte budget before starting backend I/O. The default
budget is 256 MiB, preserving the current blob pack's four-by-64-MiB raw hydration envelope. The
`[runtime] blob_hydration_bytes` setting may lower or raise it, but startup rejects a configured
value below 64 MiB while the portable object envelope remains 64 MiB. This budget covers resident
verified raw buffers. Pack-specific decoded-raster, tensor, model, base64-response, and frame limits
remain separate because they account for different allocations.

The resolved hydration budget participates in runtime and warm-daemon configuration identity.
Clients that resolve different byte budgets cannot attach to the same incumbent daemon; equivalent
resolved values remain one identity regardless of whether they came from a config file or another
supported configuration source.

`VerifiedBlob` is non-`Clone`, retains the owned admission lease, and provides borrowed byte access.
It does not provide an owned-byte extraction that releases admission while the same allocation
survives. Dropping it releases the lease. This makes downstream parse, digest mirror, raster decode,
and FANN load time part of raw-buffer residency rather than releasing the gate immediately after
I/O.

Borrowed bytes can still be copied by a caller; Rust cannot prevent `&[u8]::to_vec()`. Such derived
allocations are charged to that caller's existing pack/operation budget and do not transfer or
release the original hydration lease. The shared contract accounts for the one verified buffer it
created, not arbitrary caller duplication.

The hydrator starts the backend future under an ADR-119-tracked supervisor and receives completion
through a one-shot channel. The supervisor, not the awaiting request future, owns the admission
lease until one of these outcomes:

- success transfers the lease into `VerifiedBlob`;
- backend failure drops the lease after native work has ended; or
- request cancellation drops only the waiter, while the tracked supervisor retains the lease and
  discards the eventual bytes after native work has ended.

The hydrator validates ADR-111's 64 MiB portable envelope before admission. A larger declared
maximum returns `InvalidInput` without queueing or backend work. Because startup requires an
aggregate budget of at least 64 MiB, every valid v1 maximum is reservable against an otherwise idle
hydrator. No backend operation starts until the acquired lease has been moved into the tracked
supervisor.

Daemon drain waits for these supervisors. Cancellation cannot make capacity available while an
uncancelable `spawn_blocking` read is still allocating.

The program migrates all current whole-buffer read families before deleting `get`:

1. `blob.get` hydration, including range responses whose v1 backend operation still verifies the
   complete object;
2. moodboard source-image hydration; and
3. moodboard preference bundle and FANN network hydration, each retaining its 1 MiB caller cap.

Moodboard candidate materialization continues to use metadata-only `exists`/stat checks. It does
not hydrate candidate bytes and therefore does not enter raw-byte admission merely because its
eligibility callback moves into the shared controller.

The blob pack's private GET semaphore and moodboard's pack-local `size`/`get`/digest helpers are then
deleted. Hydration-only preference paths stop borrowing the moodboard preprocessing permit. The
preprocessing gate itself remains and continues to cover base64 decode, raster decode/normalization,
and their derived allocations; search retains that gate around preprocessing after shared
hydration.

Production whole-buffer reads enter through the `BlobHydrator` paired with the installed store.
Pack-facing raw `BlobStore` access remains available for put, size/stat, exists, delete, and
maintenance operations, but a pack does not call `get_bounded_verified` directly and bypass shared
admission.

### D4 — Artifact consumers converge on ADR-121 attachments

The shared blob operation is independent of record modeling. Packs attach original bytes through
ADR-121's role-keyed attachment substrate rather than a new entity-specific content-ref helper.

For moodboard, the original visual asset and the preference model bundle use attachment role
`"content"` during migration, matching ADR-121's legacy backfill role. The separately stored FANN
network is attached to its `moodboard_model` under the pack-owned role `"fann-network"`; the bundle's
authenticated network reference and this attachment must match on publication and load. This makes
both CAS objects live under ADR-121's single GC reference source. Existing response fields named
`content_ref` remain unchanged; they are projections of the `"content"` attachment, not restoration
of an entity column. Additional renditions use distinct pack-governed roles and never replace the
original byte identity.

`ContentRef` is the canonical identity of a byte sequence, not the universal identity of an
entity. A pack owns the domain scope within which identical bytes reuse a record. Moodboard
preserves its current caller-namespace, live-`visual_asset`, same-original-ref reuse rule, expressed
as an indexed lookup over attachment role `"content"`.

This record does not strengthen that record-level deduplication into a cross-process uniqueness
guarantee. The current critical section may remain pack-local until a durable uniqueness contract
is separately justified. Content-hash stripe maps, model locks, judgment locks, and unrelated pack
locks are not extracted by this program.

Attachment migration is ADR-121's coordinated cutover, not a separately merged prerequisite. The
same merged change adds and backfills attachments, migrates every legacy reader/writer plus hard
delete and GC reference queries (including moodboard's `"content"` and `"fann-network"` roles), and
then drops `entities.content_ref`. There is no merged dual-source interval, no recreated
`create_entity_with_content_ref`, and no second legacy column.

The canonical main/core backend is the sole database that owns attachment rows and the sole SQL
liveness authority passed to ADR-111's transactional sweep. Record-plus-attachment APIs route
through `KhiveRuntime::core()` and reject a direct attachment mutation on a secondary pack backend;
secondary databases remain available for pack-auxiliary tables but cannot anchor objects in the
process-shared BlobStore. Before cutover, boot scans every configured secondary database and fails
with an actionable inventory if it finds a live legacy `entities.content_ref`; those records must be
relocated to the main graph or explicitly curated. This program does not pretend that one
single-database sweep can fence references spread across several databases.

Before attachment-only GC or column drop is enabled, every existing `moodboard_model` with a legacy
bundle reference is backfilled under `"fann-network"` from a fully verified bundle plus its matching
immutable `moodboard.model_record` event. The mutable display property is never migration authority.
The backfill verifies bundle/network references and both governed digests; missing, mismatched, or
corrupt evidence aborts the cutover for operator curation rather than guessing or silently
invalidating a recoverable model.

Because schema preparation currently precedes BlobStore installation, this cutover is a boot-gated,
resumable two-stage migration within that one merged implementation:

1. Before recording any incomplete state, boot acquires ADR-111's same canonical-database GC
   ownership (in-process plus advisory lock), waiting for any incumbent transactional sweep, and
   retains it through finalization. A versioned main-database migration then creates `attachments`,
   backfills legacy `"content"` rows, and records an incomplete migration state while retaining the
   old column and old GC fence.
2. Before packs, requests, or GC start, boot installs the one shared BlobStore and `BlobHydrator`,
   then a host-tracked application migrator verifies every legacy preference bundle/network and
   backfills `"fann-network"`. That migrator is part of core boot discovery rather than active
   moodboard-pack selection; if its code or store is unavailable while legacy models exist, startup
   fails actionably instead of finalizing around them. An exclusive final SQL transaction
   revalidates completeness, installs attachment-based liveness queries and `attachments`
   INSERT/UPDATE claim-fence triggers, removes the legacy entity claim triggers and
   `idx_entities_content_ref`, drops `entities.content_ref`, and marks the migration complete.

This cutover migration is registered in the versioned schema ledger but invoked by the boot
coordinator after GC ownership is acquired; it is not executed eagerly by the unconditional backend
constructor. Blob-independent schema work may still run at ordinary backend open.

A crash in either stage resumes from durable state; it never opens a serving or sweep window over
the intermediate dual representation. Every daemon and administrative sweep entrypoint refuses to
run while the durable migration marker is incomplete; restart reacquires GC ownership before
resuming. The final schema contains no trigger or query that names the dropped entity column.
Attachment publication racing an active `blob_gc_claims` row is rejected by the same durable fence
that ADR-111 currently applies to entity publication.

The migrated sweep validates every distinct `attachments.content_ref` and
`blob_gc_claims.content_ref` as canonical `ContentRef` values before it commits any new claim or
physical deletion. Corrupt liveness or claim evidence aborts the sweep with every blob preserved;
moving the anti-join must not drop ADR-111's fail-closed value validation.

ADR-155's two generic pack obligations remain in force after this record supersedes it: a verb that
requires an absent BlobStore fails closed with a typed unconfigured-capability error and never falls
back to inline or scratch persistence; and each artifact record remains self-describing through a
pack-owned schema revision plus the media metadata required to interpret it without hydration. The
shared attachment substrate does not prescribe those domain fields.

### D5 — Retrieval gets a policy-free ranked-prefix controller

`khive-retrieval` gains a backend-agnostic, policy-free controller over a bounded, already
total-ordered candidate sequence. It owns iteration, batch correlation, stable compaction, output
truncation, and diagnostics. The caller owns every semantic decision and any I/O performed by its
loader callback.

The logical contract is:

```text
materialize_ranked_prefix(
  candidates: unique Candidate<Key, Score>,
  output_limit: usize,
  loader_batch_size: NonZeroUsize,
  limits: MaterializationLimits,
  order_key: candidate -> OrderKey where OrderKey: Ord,
  candidate_validator: candidate -> Valid | Fatal(error),
  batch_loader: keys -> Result<rows in arbitrary order, error>,
  classifier: (candidate, optional row) -> Keep(output) | Drop(reason) | Fatal(error)
) -> MaterializedPrefix { accepted, drop_counts, diagnostic_details, diagnostics_truncated }
```

`MaterializationLimits` bounds candidates, loader batch size, output rows, and retained diagnostic
details. The library rejects a limit above the v1 absolute ceilings of 4,096 candidates, 256 rows
per loader batch, 4,096 output rows, or 4,096 diagnostic details; each consumer may lower them.
Diagnostic overflow never changes materialization: the controller retains the first bounded details
in candidate order, counts every typed drop reason, and sets `diagnostics_truncated=true`.
`DropReason` is a caller-declared closed ordinal taxonomy of at most 32 variants, not a free-form
string map, so aggregate counters are bounded too.

Normative behavior:

- candidate uniqueness and strict monotonicity of the caller-supplied total `order_key` are
  validated across the bounded input before I/O;
- the candidate validator then runs in candidate order immediately before each ordered loader batch
  of at most `loader_batch_size`; after the batch that supplies `output_limit` accepted rows, the
  validator still visits the remaining tail in order but no loader I/O occurs;
- classification stops immediately at the `output_limit`th `Keep`; any later rows already returned
  in that batch are ignored, and only candidate validation (not classification) continues over the
  remaining tail;
- batch rows are correlated by key, so storage return order cannot change output; an unexpected or
  duplicate returned key is a fatal structural error, and a loader error is propagated as fatal;
- a missing row is presented to the classifier rather than silently assigned policy;
- `Keep` preserves the candidate's score/order and receives the next compact one-based result rank;
- `Drop` records a typed diagnostic and continues; `Fatal` stops with the caller's error;
- the controller never re-scores, pads a page, or requests candidates beyond the supplied sequence;
  continuing through already supplied overfetch after a `Drop` is not a refill; and
- all input/output and retained-diagnostic allocations stay within the validated limits above.

Gate authorization occurs before this operation. Namespace membership supplied by
`NamespaceToken::visible_namespaces()` is an eligibility/read-scope predicate, not authorization.
By-ID record and attachment hydration remains namespace-agnostic per ADR-007; callers must not add
an entity-namespace equality check and call it a security boundary.

Before this controller serves a pack result, `VerbRegistry` dispatch conformance must prove that
both `GateDecision::Deny` and every Gate infrastructure error return without invoking the pack
handler, as required by ADR-018 and ADR-129. A fail-open Gate error cannot be compensated by a
pack-local check and is a prerequisite defect, not materialization policy.

Overfetch and refill are caller policies because they determine storage work and latency. The first
moodboard migration preserves its one-shot `4 * top_k + 1` candidate request and honest underfill.
It fixes `loader_batch_size=1`, uses namespace-agnostic by-ID `get_entity`, and preserves the exact
candidate I/O and error order of the current loop. It does not add repeated refill merely because
the controller could support another batch.

Moodboard's per-visible-namespace maximum-per-subject merge is replaced with the existing
`khive-fusion::union_fusion` primitive and its canonical descending-score, ascending-subject-ID tie
order. The program does not add a second merge implementation to `khive-retrieval`.

The moodboard callbacks retain pack policy: self-hit removal; live record lookup; `artifact /
visual_asset` validation; visible-scope eligibility; required attachment role and valid
`ContentRef`; metadata-only blob existence (never candidate hydration); stale-row drop;
backend/integrity failure classification; and the public JSON projection.

### D6 — EmbeddingSpaceIdentity is the only physical vector-space fence

`NamedVectorIdentity` is replaced by `khive_storage::EmbeddingSpaceIdentity`. The type is a
backend-neutral immutable fence, not a universal model descriptor and not a canonical-JSON
library.

Its public data is conceptually:

```rust
pub struct EmbeddingSpaceIdentity {
    space_key: EmbeddingSpaceKey,
    protocol: EmbeddingProtocol,
    fingerprint: [u8; 32],
    model_name: String,
    dimensions: NonZeroU32,
}
```

Construction accepts a non-empty ASCII-alphanumeric/underscore key prefix, a versioned protocol
identifier, a 32-byte fingerprint, a model label, and dimensions in `1..=8192`. It derives rather
than accepts the physical key:

```text
{key_prefix}_{lowercase_hex(fingerprint)}_{dimensions}
```

The resulting key retains `NamedVectorIdentity`'s 128-byte ASCII alphanumeric/underscore bound;
the model label retains its non-empty, no-surrounding-whitespace, 512-byte bound. The protocol is
1..=128 ASCII bytes from `[A-Za-z0-9._-]` and names the owner and canonicalization revision. Callers
cannot supply a physical key independently from the fingerprint and dimension.

The model or protocol owner constructs and golden-tests the canonical identity document. The
shared type neither serializes arbitrary JSON nor decides whether a prompt, tokenizer, checkpoint,
pooling method, normalization, adapter, query/document transform, or provider version belongs in a
particular protocol. Every input that can affect emitted vectors must appear in that protocol's
fingerprint preimage.

The protocol identifier itself is a governed field in the fingerprint preimage. A protocol change
therefore changes the fingerprint even if all other fields are textually equal. Moodboard already
satisfies this rule through its `schema_version` field; the shared constructor does not prepend a
second domain string and therefore does not change its golden bytes.

Moodboard retains its exact `moodboard.visual-descriptor.v1` compact canonical JSON, SHA-256
fingerprint, descriptor response, and
`moodboard_{fingerprint}_{dimensions}` key. Extraction changes only the validated shared fence.
The existing Rust and Python descriptor goldens must remain byte-identical.

Text embedder registration changes from a mutable name-to-factory association into one atomic
registration unit containing the service factory and the immutable space identity of its emitted
vectors. A service cannot become visible before the identity is installed, and replacing a
same-name/same-dimension service with a different fingerprint cannot reopen the prior space.
Composition providers derive and golden-test their own effective fingerprint from the ordered
child identities plus every vector-affecting transform; runtime does not infer identity from a
display name.

The complete `space_key` is used consistently for:

- physical sqlite-vec or other vector tables;
- `_embedding_models` lineage and active revision state;
- ANN segment identity and snapshots;
- embed/query caches;
- pending-vector write logs and replay watermarks; and
- every reopen, rebuild, and orphan-cleanup lookup.

Vector insert/search APIs either accept the complete identity or operate on a store handle already
bound to it. They do not select a physical space from a display `model_name` or sanitized engine
name. Migration resolves any legacy lineage row from its complete stored
`(engine_name, model_id, key_version, dim, output_dim)` tuple before rebuilding; it never guesses a
source space from one sanitized component.

ADR-043's registry is rebuilt to separate logical serving selection from physical vector identity.
The desired table retains `id`, lifecycle status/timestamps, supersession fields, and `created_at`,
and replaces `engine_name`, `model_id`, `key_version`, `dim`, `output_dim`, and `canonical_key`
with:

```text
lineage_slot          TEXT NOT NULL
space_key             TEXT NOT NULL UNIQUE
identity_protocol     TEXT NOT NULL
identity_fingerprint  BLOB NOT NULL CHECK(length(identity_fingerprint) = 32)
model_name            TEXT NOT NULL
dimensions            INTEGER NOT NULL CHECK(dimensions BETWEEN 1 AND 8192)
```

The one-active-row partial unique index is on `lineage_slot`. A configured text engine uses its
stable operator engine name as that slot. An immutable pack-owned space uses a slot containing its
pack/purpose plus `space_key`, so two moodboard descriptor revisions may coexist without contending
for one text-engine active slot. `space_key` alone chooses the physical table.

Regular vector rows and all ANN log/watermark/pending tables replace their ambiguous
`embedding_model` column with `embedding_space_key`. An agent-facing parameter that selects an
embedder continues to name the registry/lineage slot; runtime resolves its active row and passes the
resulting complete identity to storage. Stored rows never contain a display model name as their
space fence.

Historical registry rows are preserved as explicit legacy provenance under protocol
`khive.legacy-embedding-space.v1`. A companion `_embedding_model_legacy_provenance` table keyed by
the unchanged registry `id` stores the exact prior `engine_name`, `model_id`, `key_version`, `dim`,
nullable `output_dim`, opaque `canonical_key`, and pre-migration status; the irreversible fingerprint
is never the only surviving copy of the tuple.

For a valid legacy row, mapping is exact and restart-stable:

| New field              | Legacy mapping                                                                 |
| ---------------------- | ------------------------------------------------------------------------------ |
| `id`                   | unchanged                                                                      |
| `lineage_slot`         | exact prior `engine_name`                                                      |
| `identity_protocol`    | literal `khive.legacy-embedding-space.v1`                                      |
| `identity_fingerprint` | legacy digest below                                                            |
| `space_key`            | `legacy_{lowercase_hex(identity_fingerprint)}_{dimensions}`                    |
| `model_name`           | exact prior `model_id`                                                         |
| `dimensions`           | `output_dim` when present, otherwise `dim`                                     |
| lifecycle/timestamps   | unchanged during staging; transitioned only by the atomic cutover policy below |

The legacy digest is SHA-256 over the exact stored protocol bytes
`khive.legacy-embedding-space.v1`, one NUL, then each legacy UTF-8 value `engine_name`, `model_id`,
and `key_version` framed by a big-endian `u32` byte length in that order, followed by big-endian
`u32 dim`, one byte `0` for absent `output_dim` or `1` plus big-endian `u32 output_dim` when present.
Golden fixtures pin the complete preimage, every mapped registry column, preserved provenance row,
and derived `legacy_…` key.

The desired registry is built as a shadow table while the old serving path remains authoritative;
no runtime resolves a staged legacy `space_key`. At cutover every legacy `active` row belonging to
a configured text lineage must have one validated replacement in the same `lineage_slot`; it
becomes `superseded` and points to that replacement in the transaction that activates the new row
and publishes the new registry. Active non-text/pack-owned legacy rows and legacy `pending` rows
become `archived`; already `superseded` or `archived` rows and their links remain unchanged. A pack
later re-enables such a space only by supplying its authenticated owner identity and rebuilding from
source, never by blessing the legacy tuple. Ambiguous classification or a configured serving row
without a replacement aborts cutover.

The legacy identity names the complete stored tuple; it does not certify equivalence to any newly
registered provider. No old vector is copied or relabeled under a new space. New tables are rebuilt
from source records; old tables and operational logs remain isolated until the atomic cutover and
explicit curation. Any legacy row/log that cannot be mapped to one complete tuple fails migration
and forces rebuild rather than accepting a guessed identity.

The storage/runtime registry record and read/write APIs change atomically with this table. They
expose the new identity and lineage fields; legacy tuple fields remain available only through the
explicit provenance/admin surface, not as a second serving identity API.

Namespace is not part of `EmbeddingSpaceIdentity`. Memory continues to maintain global vector
spaces across namespaces and applies configured read scope at query time.

Existing vectors are never relabeled into a new identity. Text-provider migration creates new
physical spaces, re-embeds/reindexes from source records, validates parity and coverage, and makes
an atomic serving cutover. Old spaces remain isolated until explicit curation. Default migration
does not dual-read or fuse cosine scores across different identities.

`NamedVectorIdentity` and sanitized-name-derived physical keys are deleted in the same program.
There is no alias or compatibility constructor.

### D7 — Pairwise numerical fitting moves literally to lattice-tune

The deterministic numerical portion of ADR-149 moves to the existing `lattice-tune` crate under a
versioned `preference::PairwiseV1` module. No new Lattice crate is introduced. The move requires a
separate accepted Lattice ADR before a public Lattice interface changes.

Khive prepares already grouped and split numeric rows and passes only the numerical dataset to
Lattice. `PairwiseV1` owns its frozen algorithm parameters; it exposes no caller overrides for the
optimizer, calibration search, or float-materialization rules. Lattice owns:

- grouped weighted Bradley--Terry binary cross-entropy with L2 regularization;
- full-batch float64 optimization and the exact Armijo step sequence;
- float32 parameter materialization into a zero-intercept `N -> 1 Linear` FANN network;
- decisive calibration temperature and tie-band calibration;
- test metric computation through the materialized FANN forward path; and
- validation and deterministic serialization of the numerical result.

The first implementation is a literal extraction of ADR-149 v1 arithmetic. Iteration limits,
tolerances, constants, loop order, tie breaks, float conversions, error classification, and FANN
bytes do not change while moving ownership.

Khive's workspace and moodboard pack pin the exact accepted `lattice-tune` release, as ADR-148 does
for its Lattice inference dependencies. Updating that pin requires the same differential numerical
and serialized-artifact evidence.

Moodboard retains:

- the ten feature names, order, bounds, dtype, left-minus-right transform, and schema digest;
- actor, namespace, board, descriptor, and attachment scope;
- the unordered-pair split preimage and train/calibration/test buckets;
- support floors and event snapshot selection;
- serve, judgment, and model event wire identities;
- artifact publication, BLAKE3 references, SHA-256 mirrors, and the outer JSON bundle; and
- all user-facing verbs and response semantics.

Acceptance is differential: the pack-local oracle and Lattice implementation must produce exact
weights, float32 conversion, FANN bytes, artifact digests, calibration values, metrics,
predictions, and failure classes over permanent golden fixtures. The pack-local numerical code is
deleted after parity; event/provenance code is not moved.

### D8 — Checkpoint trust mechanics move without weakening attestation

Checkpoint extraction also requires a separate accepted Lattice ADR before changing public Lattice
interfaces. Ownership is split into four layers:

1. `lattice-inference` owns opened-handle path resolution, descriptor-relative safe reads, and
   low-level filesystem trust mechanics behind its existing mmap trust boundary.
2. The Lattice vision checkpoint module owns Qwen-family required-file inventory and shard/config
   closure validation.
3. `lattice-embed` owns the embedding-consumer facade that assembles a prepared snapshot and
   returns an attestation report while retaining every resource needed by model memory maps.
4. Khive/moodboard owns the operator's expected digest, its exact legacy whole-tree digest
   protocol, descriptor identity, and model publication.

The initial move preserves, byte for byte, ADR-148's
`khive-moodboard-checkpoint-v1` domain separator; big-endian file count, path length, and file
length framing; lexicographic relative-path order; complete auxiliary-file coverage; internal file
symlink acceptance; directory and escaping symlink rejection; opened-handle source-swap defense;
UTF-8/path/file-count bounds; and descriptor fingerprint. It does not substitute Lattice's current
per-tensor file-descriptor trust or base-model revision derivation for the stronger whole-directory
identity.

The public facade does not return a raw file descriptor, unguarded `File`, mutable path, or a second
“trusted mmap” token. Existing `mmap_trust` remains the sole memory-map authority. A prepared
checkpoint object remains alive for at least as long as any model or tensor mapping derived from
it.

The Lattice ADR defines a capability-limited, caller-supplied attestor seam rather than importing a
Khive digest protocol. Lattice owns enumeration, safe opened-handle reads, and ordered streaming
from the private snapshot. After it has bounded and lexicographically ordered the complete
inventory, it calls `begin(file_count: u64)`, then supplies each normalized logical relative-path
byte string, declared file length, and bounded byte chunks in order. The attestor receives no OS
path, `File`, file descriptor, mmap token, or mutable handle. Khive supplies the versioned framing
strategy and expected digest, and the resulting attestation is opaque to Lattice. Each pre-load and
post-load pass creates a fresh attestor and fresh opened-handle read sequence. Thus Lattice can
return a backend-neutral report without either baking `khive-moodboard-checkpoint-v1` into its model
identity or creating a second mapping authority.

The facade preserves ADR-148's complete preparation and publication sequence:

1. Copy the validated source into a private random snapshot using opened handles, materializing
   accepted internal file symlinks as regular files.
2. From that snapshot, validate the complete Qwen inventory and geometry and perform the first
   whole-tree attestation; then seal the tree read-only.
3. Construct weights and memory maps only from the sealed snapshot under existing `mmap_trust`.
4. After construction, re-read geometry, perform a second complete whole-tree attestation from the
   same sealed snapshot, and compare the complete descriptor before publication.
5. Retain the prepared snapshot through the stage, model, and tensor-mapping lifetimes; unseal it
   only for deletion after all of them release it.

This remains one full checkpoint copy plus two linear verification reads. Source-path replacement
after copying cannot affect the prepared model, and a mutation, addition, deletion, rename, or
replacement in the snapshot's load window fails before publication even when the first attestation
matched.

An aggregate checkpoint byte ceiling and new platform support are useful hardening, but they change
the accepted digest/load admission contract and require measured follow-up rather than being hidden
inside the ownership move.

### D9 — Intent-rephrase expansion is opt-in and bounded by admitted work

The unchanged original-only knowledge handler runs to completion first. Only while its parent
request remains active may runtime generate variants; generation for each generated leg completes
before that leg is embedded. Expansion is not added to `QueryNode`: the current IR contains
precomputed vector leaves, has no repository executor, and remains a pure plan representation.

Runtime owns a named lazy query-expander registry patterned after the embedder registry, with two
differences: registration atomically includes a complete generator identity, and initialization or
use appends no durable events. `original_query_utf8_bytes` means exactly the existing knowledge
handler's post-`str::trim()` `raw_query` bytes—the bytes already used by baseline retrieval and
scoring—with no additional case or Unicode normalization. The provider interface accepts those
bytes, the child absolute deadline/cancellation token, and `ExpansionLimits`, and returns only
candidate UTF-8 strings. One ADR-119-tracked expansion supervisor owns the child deadline and
max-inflight lease across initialization, generation, and generated retrieval. Canceling the waiter
requests cancellation but does not release capacity until provider/backend work has really ended
and the supervisor has discarded any late result.

The complete generator identity uses immutable digests or governed revisions—not paths or display
names—for provider implementation, model/checkpoint, ordered adapter composition, tokenizer,
prompt, grammar, decoding/sampling parameters, seed policy, thinking mode, normalization, and every
effective token, byte, and variant cap. The provider owner canonicalizes that document and the
identity fingerprint is its SHA-256, encoded as exactly 64 lowercase hexadecimal characters. The
registry `id` is 1..=64 bytes from `[A-Za-z0-9._-]`; its governed human revision is 1..=128 ASCII
graphic bytes. A cache key is exactly
`(generator_identity_fingerprint, original_query_utf8_bytes)`. Provider-internal normalization
remains governed by the identity but never collapses two different cache keys. A time-derived or
otherwise unrecorded seed is not cacheable, and every hit is revalidated against the effective
structural limits before use.

The pure `khive-retrieval` portion accepts the original query plus validated variants and owns only:

- `original_query_utf8_bytes` decoded as immutable variant `0`, also reused by final scoring;
- removal of leading/trailing ASCII SP, HT, CR, and LF followed by exact UTF-8-byte deduplication
  against the original and earlier variants, with internal bytes otherwise unchanged;
- at most three generated variants, preserving validated provider order;
- deterministic RRF-60 over already-ranked per-variant candidate lists, used only for bounded
  candidate admission rather than as a public relevance score;
- canonical hit-ID tie ordering; and
- one transient attribution occurrence per `(variant_id, lexical-or-ann source, source_rank)`.

It performs no inference, embedding, FTS/ANN request, Gate call, or durable write. V1 hard limits
are three generated variants, 256 UTF-8 bytes per variant, 1,024 generated UTF-8 bytes in aggregate,
and 192 generated output tokens; operator configuration may only lower them. Empty strings after
the specified trim, NUL bytes, an over-limit string/envelope, or a fourth item invalidate the
generated result and take the original-only path.

The first consumer is knowledge search, controlled by default-off operator configuration. Enabling
it requires explicit non-zero values for every non-cache field of `ExpansionLimits`; startup rejects
an incomplete configuration or a value above these immutable v1 ceilings:

```text
child_deadline_ms                 <= 10_000
return_reserve_ms                 <= 1_000
max_inflight                      <= 4
extra_fts_calls                   <= 3
extra_fts_rows                    <= 768
extra_embedding_calls             <= 4
extra_ann_calls                   <= 12
extra_ann_raw_candidates          <= 1_536
extra_row_hydrations              <= 1_536
extra_namespace_backend_calls     <= 192
max_fused_candidates              <= 127
cache_entries                     <= 4_096
cache_bytes                       <= 67_108_864
cache_ttl_seconds                 <= 86_400
```

The output token/byte/variant ceilings above are also required effective limits. Configuration may
lower but never raise any ceiling. The three cache fields are either all zero (cache disabled) or
all non-zero within their ceilings; byte accounting includes keys and values. No generated leg
consumes or reduces the original query's existing budget.

The resolved enable flag, generator identity, every effective limit, and cache configuration
participate in runtime/daemon configuration identity. Clients with different expansion behavior or
ceilings cannot silently share an incumbent warm daemon.

Each generated variant receives at most one lexical leg and one ANN leg and never recursively
invokes decomposition. Variants and their legs execute serially in provider order. For `n`
effective generated variants, each aggregate FTS-row and ANN-candidate quota `q` is allocated as
`q / n`, plus one for the first `q % n` variants; unused quota is never reassigned. The embedding
ledger reserves exactly one query-embedding call per generated variant plus one final rerank call.
That final call receives the original query plus at most 127 admitted candidate texts, respecting
the existing 128-text batch cap without introducing chunk-dependent arithmetic. Calls, rows, raw
candidates, embedding calls, refill calls, row hydrations, and namespace-scoped backend calls have
separate ledgers and are charged before work is admitted. An FTS call that returns zero rows still
consumes a call; an ANN refill's repeated prefix and duplicate hits consume call and raw-candidate
budget again. Serial scheduling makes global call/hydration/namespace exhaustion and fallback
independent of task races. Exhausting any dimension discards all expansion work and returns the
frozen baseline.

These counters bound observable admitted work. They cannot measure backend CPU or scan effort after
admission. The child deadline bounds request-visible waiting, not the lifetime of work that a native
provider cannot interrupt; such work remains admission-accounted and ADR-119 drain-visible until it
really finishes. A candidate-pool limit alone is not sufficient admission.

The original-only handler first freezes the exact canonical `serde_json::Value` and its bounded
internal lexical/ANN rank trace before provider initialization or any generated retrieval begins.
If the parent has no configured return reserve after that point, expansion is skipped. Disabled
configuration, missing provider, initialization or generation error, child timeout/cancellation,
invalid grammar output, cap or ledger exhaustion, empty output, or no eligible generated ranked
list returns that same value to the ordinary presentation layer. “Byte-identical” means the same
canonical handler value is serialized under the same presentation mode; no alternative fuser
reconstructs a purported equivalent response. Parent timeout/cancellation retains the pre-existing
request failure semantics rather than converting failure into a successful fallback.

Generated legs use the existing lexical and ANN channel implementations and the same structural
status, type, and namespace filters, but do not run final query scoring or `min_score` separately.
Each leg's existing inner lexical/ANN fusion produces its ranked candidate list. The outer RRF-60
combines those lists with variant `0`'s bounded trace only to select a bounded admission pool. The
pool contains at most `max_fused_candidates`. The existing original-query eligibility, scoring,
embedding rerank, `min_score`, and public score-band pipeline then runs exactly once over that pool,
using its existing single batch containing the original query plus candidate texts. Its score
remains `results[].score`; neither inner nor outer RRF score is exposed. Generated legs reuse
lifecycle state established by the baseline and must not start a second ANN warm, checkpoint,
watermark, or other durable maintenance path. All expanded final-pass row loads are charged as
extra hydrations.

Once at least one validated generated ranked list contributes, the expanded response adds exactly:

```text
query_expansion: {
  generator: { id, revision, identity_fingerprint },
  variants: [{ id, origin: "original" | "generated", text }]
}

results[].variant_attribution: [
  { variant_id, source: "lexical" | "ann", source_rank }
]
```

Variants are ordered by ID. A hit present in both channels has two attribution entries, never
`source:"both"`; entries are ordered by variant ID, lexical before ANN, then source rank. These are
candidate-origin facts, not score explanations; `variant_id` is `0..=3` and `source_rank` is the
one-based position in that channel's ranked list. Expansion-phase code writes no atom, note, event,
schema row, serving ledger, ANN checkpoint, or watermark; tests distinguish baseline-owned writes
from expansion-tagged work. Telemetry records identities, counts, latency, and fallback class but
not raw query or generated text by default.

The bounded in-memory cache stores only post-validated deterministic successes and valid empty
results. Errors, timeouts, cancellations, cap failures, and unrecorded seeds are never cached. Entry,
serialized-byte, and TTL limits are enforced simultaneously with deterministic eviction.

The child deadline is no later than both the configured 10-second ceiling and the parent's absolute
deadline minus its configured return reserve. Provider/child timeout or cancellation while the
parent remains active takes the frozen-baseline path; expiration or cancellation of the parent
propagates its existing error.

Strict Lattice HTTP v0 does not accept JSON Schema `minItems` or `maxItems`; an adapter using that
surface enforces cardinality with generation limits and strict post-validation. A feature-gated
Lattice adapter or binary-composition crate depends on `khive-runtime`'s provider interface;
`khive-runtime` and `khive-retrieval` do not depend upward on experimental `lattice-inference`.

The implementation PR records benchmarks for the selected local model, configured work envelope,
deadline behavior, and binary-size delta before recommending any non-zero production setting.
Rollback is configuration-only because the feature has no durable state.

### D10 — Non-extractions and phased migration

The following remain local in this program:

- moodboard raster preprocessing and decoded-allocation admission;
- moodboard serve/judgment/model event provenance and frozen UUID/digest framing;
- `BlockingStage` and model-specific warm-state lifecycle;
- content, judgment, model, Git repository, and Git cache lock maps; and
- attachment eligibility, visible namespace scope, and pack error policy.

A finite stripe map must not replace an exact-key lock when the existing contract requires
different keys never to block. In particular, Git cache `SLOT_LOCKS` remains exact-key. Map cleanup
or bounded lifetime is a separate concern and cannot justify weaker mutual-exclusion semantics.

Moodboard's detached `BlockingStage` model-load task, its per-request blocking inference closure,
and its blocking preference-fit closure must join ADR-119 tracking in focused fixes; cancellation
drops only the waiter, while the host retains and drains the native work and its owned permit. This
scope does not create a generic stage abstraction or silently expand into unrelated storage
maintenance work.

The implementation sequence is normative:

0. **Dispatch prerequisite.** Restore ADR-129 fail-closed behavior for Gate infrastructure errors
   in normal and intercepted `VerbRegistry` dispatch, with no handler invocation. No other program
   phase merges before this independently reviewable correction.
1. **Storage contract.** Add typed errors and required `get_bounded_verified`; implement Fs and S3
   under one conformance suite.
2. **Runtime lifetime.** Add the hydrator, weighted admission, lease, tracked supervisor, and
   cancellation/drain tests.
3. **Consumer closure.** Migrate blob GET, moodboard source-image reads, and preference
   bundle/network reads; leave candidate checks metadata-only; delete unbounded `get`, the blob GET
   semaphore, and pack-local size/read/digest helpers without removing moodboard's decoded/raster
   preprocessing gate.
4. **Attachment convergence.** Execute ADR-121's boot-gated coordinated cutover on the canonical
   main backend: add/backfill the substrate while serving stays closed; after shared hydration is
   installed, migrate every legacy reader, writer, hard-delete, GC query and claim fence plus
   moodboard roles `"content"` and `"fann-network"`; reconstruct existing network roles only from
   verified bundle/event evidence; cross-check the authenticated network reference; then finalize
   and drop `entities.content_ref` in the same merged change. Reject secondary-backend anchors.
5. **Pure retrieval.** Reuse `union_fusion`, add the ranked-prefix controller, and migrate moodboard
   with exact one-shot overfetch/underfill parity.
6. **Identity fence.** Introduce `EmbeddingSpaceIdentity`, preserve moodboard descriptor goldens,
   then remove `NamedVectorIdentity`.
7. **Text vector cutover.** Atomically bind providers and identity, widen vector/ANN/cache/log keys,
   rebuild from source, verify, and atomically cut over without relabeling old rows.
8. **Preference extraction.** After the Lattice ADR, land differential Lattice/Khive goldens,
   switch moodboard, and delete the local numerical oracle.
9. **Checkpoint extraction.** After the Lattice ADR, land cross-platform trust/digest parity,
   switch moodboard, and delete duplicated mechanics.
10. **Query expansion.** Land the optional adapter and knowledge pilot last, default off, after
    benchmark evidence fixes its first recommended operating envelope.

Each phase lands in a reviewable PR with its own rollback boundary. No later phase is required to
make an earlier merged phase safe. Removal of the old path occurs in the same phase that closes its
last consumer.

## Required verification

### Blob and runtime resource safety

- An object exactly at `max_bytes` succeeds; `max_bytes + 1` refuses without full allocation.
- False small metadata followed by a larger body is stopped at the actual-byte boundary.
- Metadata/final-length disagreement and digest mismatch return their typed failures before bytes
  reach a consumer.
- Canceling a waiter does not release admission while native I/O continues.
- A maximum above 64 MiB returns `InvalidInput` before queueing and a backend spy observes zero
  calls; every valid maximum can acquire against an idle minimum-size aggregate budget.
- API assertions prove `VerifiedBlob` is non-`Clone`, exposes no owned-byte extraction, and retains
  the original lease until wrapper drop; caller-created copies remain subject to caller allocation
  budgets.
- Daemon drain waits for hydration supervisors.
- Fs, S3, and fixture backends pass identical bound, integrity, and error-precedence cases. S3
  separately proves one provider deadline spans GET plus body consumption; hydrator tests prove a
  caller deadline/cancellation drops only the waiter while admission remains until native completion.
- Repository search proves no `BlobStore::get` call or implementation remains.
- Production `get_bounded_verified` calls exist only in `BlobHydrator` backend delegation (plus
  backend conformance tests), never in packs.
- Core, blob-pack, and moodboard runtime handles sharing one store contend on one byte budget rather
  than receiving independent hydrators.
- Configurations differing only in resolved `blob_hydration_bytes` have different daemon
  `config_id` values; equivalent resolved values have the same identity across config sources.
- Hydration-only paths no longer consume a preprocessing permit, while concurrent raster
  decode/normalization remains bounded exactly as before.

### Attachments and ranked materialization

- Original bytes publish and resolve through role `"content"`; existing wire content refs are
  unchanged.
- Preference bundle and FANN network attachments both remain live under GC, and a disagreement
  among the bundle reference, model event, and `"fann-network"` attachment fails closed.
- A pre-ADR-160 preference model migrates, survives restart and attachment-only GC, and produces its
  prior exact prediction; corrupt or incomplete bundle/event evidence aborts migration.
- A crash after either migration stage resumes before serving; no request or GC runs while the
  durable migration marker is incomplete. An upgrade with moodboard disabled still migrates legacy
  models, and enabling moodboard after restart loads them unchanged.
- With a transactional sweep paused after candidate/claim selection, migration waits on the shared
  database GC ownership before writing its marker; after both complete, the bundle and network
  objects survive.
- Multi-backend tests prove attachment publication is routed to main and rejected on a secondary;
  a main-backend attachment protects its object during a sweep, while a discovered legacy
  secondary reference blocks cutover rather than being ignored.
- An attachment INSERT/UPDATE racing an active GC claim is rejected or makes the object survive;
  schema inspection finds no post-cutover trigger, index, or query referring to
  `entities.content_ref`.
- Non-canonical attachment or GC-claim refs abort a sweep before any new claim or deletion; a corrupt
  liveness fixture proves every blob survives.
- Same-byte reuse preserves moodboard's current namespace/subtype scope without claiming durable
  global uniqueness.
- Arbitrarily ordered, missing, and duplicate loader rows produce deterministic diagnostics and
  output.
- In a multi-row loader batch, classification stops at the Kth `Keep`; later already-loaded rows
  cannot add diagnostics or surface a classifier failure, while tail candidate validation still
  runs without further I/O.
- Deleted, wrong-subtype, wrong-scope, missing-role, malformed-ref, and absent-blob candidates obey
  the pack's existing drop/fatal policy.
- Invalid scores anywhere in the bounded candidate tail fail even after enough valid hits exist.
- With moodboard's one-row batches, an earlier loader failure still wins over a later invalid tail
  score, preserving the current interleaved error order.
- Equal scores use canonical subject-ID order; retained scores are not recomputed; result ranks are
  compact.
- Existing one-shot overfetch, self-exclusion, and honest underfill are exact before and after.
- Gate allow precedes the operation; Gate deny or infrastructure error never invokes the handler;
  visible scope does not become a by-ID namespace wall.

### Embedding identity and migration

- Moodboard canonical descriptor bytes, fingerprint, model key, response, and table selection are
  exact goldens.
- Every registered identity protocol ships a field-mutation matrix proving that changing each of
  its declared vector-affecting inputs changes the space key; the shared type does not guess that
  field set.
- Replacing a same-name provider under a new fingerprint cannot read or write the old space.
- Namespace changes do not change the space.
- Table, lineage, ANN segment, cache, pending log, replay watermark, and reopen path agree on the
  same complete key.
- Restart resolves the same unchanged space.
- Registry tests distinguish one-active-per-text-lineage from coexisting immutable moodboard
  revisions; every stored row contains its resolved `space_key`, never a display model label.
- Golden migration tests pin the legacy tuple's length-framed fingerprint and fail closed on an
  incomplete or ambiguous tuple.
- Migration tests prove that old vectors are never served under a new identity and that cutover is
  atomic after complete rebuild.

### Preference and checkpoint parity

- Pairwise fixtures pin split membership, weights, float conversions, FANN bytes, BLAKE3/SHA-256,
  calibration, metrics, predictions, and failure classes across both repositories.
- Checkpoint fixtures cover ancestor and source symlink swaps, accepted internal file symlinks,
  escaping and directory symlinks, one-byte changes, file additions/removals/renames, tokenizer and
  shard changes, non-UTF-8 and count/length limits, resource lifetime, and platform path resolution.
- Whole-tree digest and descriptor identity are byte-identical across the move.
- A test hook changes, adds, deletes, renames, and replaces snapshot content after the first
  attestation but before publication; the second geometry read, whole-tree attestation, and complete
  descriptor comparison each run against the same sealed snapshot and fail before publication.
- No new public mapping API can bypass Lattice's existing mmap trust authority.
- Canceling the final `BlockingStage` waiter after checkpoint/model native work begins leaves that
  work drain-visible until completion; daemon drain waits within ADR-119's bound, and the completed
  stage remains reusable by a later caller.
- Canceling an in-flight moodboard inference or preference fit likewise leaves its native work and
  owned permit drain-visible until completion.

### Query expansion

- A spy proves the unchanged original handler value and bounded rank trace are frozen before provider
  initialization. Disabled, unavailable, child timeout/cancellation while the parent remains active,
  provider error, invalid/oversize output, empty output, ledger exhaustion, and no-generated-list
  cases hand that same value to presentation; parent timeout/cancellation preserves existing request
  failure semantics.
- Initialization, generation, and generated retrieval share one derived child
  cancellation/deadline and max-inflight lease. Canceling an uninterruptible expansion waiter
  retains its lease and drain-visible supervisor until real completion.
- Startup rejects every missing, zero, or above-ceiling enabled limit. Allocation goldens cover the
  quotient/remainder split; instrumented FTS, embedding, ANN, refill, row hydration, and namespace
  calls stay inside their separate ledgers, including zero-result FTS calls and repeated ANN
  prefixes/hits.
- Cache-key goldens cover every immutable identity/configuration field and exact original query
  bytes; entry, serialized-byte, and TTL ceilings hold simultaneously, and nondeterministic unkeyed
  seeds bypass cache.
- Whitespace goldens prove provider input, cache key, variant `0`, baseline trace, and final scoring
  all use the existing post-`str::trim()` `raw_query` bytes with no extra normalization.
- Configurations that differ only in resolved expansion identity or limits receive distinct daemon
  configuration IDs; equivalent resolved settings remain stable across configuration sources.
- Generated legs never invoke decomposition or lifecycle maintenance. Inner channel ordering, outer
  admission RRF, and per-source attribution are deterministic under shuffled backend return order,
  dual-channel hits, and equal scores.
- The expanded final pass uses the original query's existing eligibility, scoring, rerank,
  `min_score`, and public score bands exactly once; an RRF score never appears as `results[].score`.
- Expansion-tagged event/SQL spies observe zero durable writes, while ordinary baseline-owned ANN
  warm/checkpoint behavior remains unchanged.

## Consequences

### Positive

- Every blob consumer receives the same backend-enforced integrity and allocation contract.
- Admission represents live resource residency even under cancellation and native blocking work.
- Record attachment semantics converge on the already accepted substrate instead of extending a
  column scheduled for removal.
- Retrieval gains a reusable mechanism without importing authorization or domain policy.
- Model/provider replacement cannot silently reuse an incompatible vector space.
- Moodboard becomes the acceptance consumer for shared seams while preserving its frozen public and
  evidence identities.
- Lattice owns reusable model mechanics and numerical fitting without absorbing Khive event or
  graph semantics.
- Query expansion can be measured and rolled back without schema or event cleanup.

### Negative

- The program crosses storage, runtime, retrieval, DB/ANN lifecycle, two packs, and two repositories;
  it therefore requires more small PRs and longer parity operation than a local cleanup.
- Reserving a caller's maximum bytes is conservative and can reduce concurrency for objects much
  smaller than their declared cap.
- Holding the blob lease through downstream use lowers peak concurrency but is the cost of making
  admission describe resident buffers honestly.
- Text identity migration requires re-embedding and extra temporary disk; old vectors cannot be
  relabeled as a shortcut.
- Exact preference/checkpoint parity constrains cleanup during extraction; refactoring arithmetic or
  digest framing must wait for a separately reviewed revision.

### Neutral

- No entity kind, note kind, edge relation, or event kind changes.
- Moodboard remains opt-in and experimental.
- Query expansion is default off and produces no durable provenance.
- ADR-156 remains proposed and may be rebased or superseded separately.

## Alternatives considered

### Keep all mechanisms in moodboard

Rejected. The blob pack and preference loader already need the same safety contract, text vectors
need a complete space fence, and the current duplicate raw-read gates do not coordinate memory.

### Put a complete artifact materializer in khive-retrieval

Rejected. Entity liveness, namespace read scope, Gate policy, attachment roles, blob existence, and
pack error classification are application semantics. Moving them down would invert dependencies and
misstate visible scope as authorization.

### Implement bounded hydration only in runtime

Rejected. A runtime wrapper around filesystem whole-buffer `get` can reject after allocation but
cannot impose an actual-byte bound inside the backend operation.

### Put admission inside BlobStore

Rejected. The storage trait describes backend capability. Deployment-wide concurrent-memory policy,
request cancellation supervision, and result residency belong to runtime composition. Backends
still enforce each individual read's hard maximum.

### Keep both bounded and unbounded blob methods

Rejected. The unbounded path would remain the shortest implementation route and make safety depend
on caller discipline. All current consumers fit the 64 MiB v1 whole-buffer envelope.

### Make descriptor canonical JSON a shared utility

Rejected. Different model protocols govern different fields and encodings. The reusable contract is
the validated fingerprint fence; the protocol owner must retain and golden-test canonicalization.

### Generalize pairwise learning before moving it

Rejected. A literal extraction provides a stable cross-repository oracle. Changing optimizer,
schema abstraction, arithmetic, or artifact layout in the same step would make parity failures
unattributable.

### Replace keyed locks with fixed stripes

Rejected. Finite stripes make unrelated keys block on collision and violate existing exact-key
contracts such as the Git cache's different-key concurrency guarantee.

### Add query expansion to QueryNode or khive-retrieval provider traits

Rejected. Expansion precedes embedding and owns model lifecycle, deadlines, caches, and
cancellation. The current IR has no executor and the retrieval crate must remain provider-free.

## Non-claims

- This record does not introduce streaming blobs above 64 MiB.
- It does not make attachments an authorization boundary or namespace an isolation mechanism.
- It does not promise cross-process artifact-entity uniqueness.
- It does not improve moodboard model quality or alter its experimental label.
- It does not change preference feature semantics, event identities, or statistical claims.
- It does not make generated queries durable evidence or enable expansion by default.
- It does not authorize public Lattice interface changes before the required Lattice ADRs.
- It does not create a generic concurrency-stage or keyed-lock library.
- It does not define ADR lifecycle metadata or an ADR coherence CI guard. That governance work
  requires a separate decision because the current corpus has heterogeneous status, amendment,
  supersession, and PR-body-only ratification evidence.

## References

- `crates/khive-storage/src/blob.rs` — current blob capability
- `crates/khive-db/src/stores/blob.rs` — filesystem whole-buffer read and store implementation
- `crates/khive-db/src/stores/blob_s3.rs` — existing actual-byte-bounded S3 stream
- `crates/khive-pack-blob/src/handlers.rs` — private GET admission and verified read
- `crates/khive-pack-moodboard/src/handlers.rs` — source hydration, vector merge, and hit materialization
- `crates/khive-pack-moodboard/src/model.rs` — descriptor identity and checkpoint snapshot
- `crates/khive-pack-moodboard/src/preference.rs` — deterministic pairwise numerical core
- `crates/khive-pack-moodboard/src/preference_handlers.rs` — preference artifact hydration
- `crates/khive-runtime/src/embedder_registry.rs` — provider lifecycle precedent
- `crates/khive-retrieval/src/query_ir.rs` — pure post-embedding query IR
- `crates/khive-pack-knowledge/src/knowledge/search.rs` — first query-expansion consumer
