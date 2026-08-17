# ADR-148: Moodboard Visual Retrieval Pack

**Status**: accepted\
**Date**: 2026-08-08\
**Authors**: khive maintainers\
**Amended by**: [ADR-149](ADR-149-moodboard-preference-learning.md); proposed
[ADR-160](ADR-160-shared-pack-infrastructure.md) converges attachment publication on ADR-121 and
extracts shared hydration, complete embedding-space identity/lineage mapping, fusion,
materialization, and checkpoint seams on acceptance.

## Context

Graphic-media curation needs a durable asset identity, a model-identity-bound visual
descriptor, and similarity retrieval without pretending that one embedding is a complete
measure of aesthetic coherence. Khive already owns the required persistence boundaries:
artifact entities, `BlobStore` content-addressed bytes, token-scoped vector stores, and the
single MCP `request` surface. Lattice already exposes local Qwen3.5 vision-language pooled
embedding inference.

The missing layer is composition. Registering a vision checkpoint as a text
`EmbedderProvider` is incorrect: entity and note creation fans text into every registered
provider, while a visual descriptor must be derived from decoded raster bytes under an
explicit preprocessing contract. A separate application database would duplicate Khive's
asset, provenance, namespace, and retrieval responsibilities.

The Qwen3.5 pooled descriptor available in `lattice-embed` 0.9.0 is experimental retrieval
machinery. Its own contract says retrieval quality for the base instruct checkpoint is
unvalidated. This pack must expose that machinery honestly, not market it as a state-of-the-art
style model or collapse compatibility, cohesion, diversity, and uncertainty into one score.

## Decision

### D1 — An opt-in first-party `moodboard` pack

`khive-pack-moodboard` is linked into `khive-mcp` and `kkernel` inventory but is not part of
`RuntimeConfig::built_in_packs()`. Operators load it explicitly, for example with
`KHIVE_PACKS=kg,moodboard`. It declares `REQUIRES = ["kg"]` and contributes three additive
`artifact` entity subtypes:

- `visual_asset`
- `moodboard`
- `moodboard_model`

The pack requires no auxiliary SQL schema. It uses the shared entity substrate, the installed
`BlobStore` capability, and a model-identity-specific vector table. A missing blob store or model
configuration is an attributable `Unconfigured` error. `blob` is not a vocabulary dependency and
therefore is not in `REQUIRES`; the capability can be installed without exposing the blob verbs.

This ADR-148 visual slice exposes exactly three non-CRUD verbs; ADR-149 additively extends the
same pack with four preference-learning verbs:

| Verb               | Contract                                                                                                                   |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------- |
| `moodboard.model`  | Discover/validate the configured identity without constructing model weights and return its immutable descriptor.          |
| `moodboard.ingest` | Validate a raster, publish original bytes, attach/reuse a `visual_asset`, infer and persist its descriptor.                |
| `moodboard.search` | Re-derive one asset's descriptor and perform exact cosine nearest-neighbor search in the same identity-bound vector store. |

Boards and model artifacts otherwise use the generic KG verbs. The pack does not duplicate CRUD.

### D2 — Closed descriptor identity and wire shape

Every response contains a nested `descriptor` object with this closed v1 shape:

```json
{
  "schema_version": "moodboard.visual-descriptor.v1",
  "model_key": "moodboard_<fingerprint>_<dimensions>",
  "model_name": "qwen3.5-vlm-pooled-visual",
  "model_revision": "<operator-pinned revision>",
  "checkpoint_sha256": "<64 lowercase hexadecimal characters>",
  "inference": { "provider": "lattice-embed", "version": "0.9.0" },
  "preprocessing": {
    "revision": "moodboard-qwen35-srgb-pad32-max448-v1",
    "max_side": 448,
    "alignment": 32,
    "matte_rgb": [128, 128, 128],
    "resample": "lanczos3"
  },
  "prompt": {
    "revision": "moodboard-style-retrieval-v1",
    "sha256": "<64 lowercase hexadecimal characters>"
  },
  "pooling": "mean_visual_tokens",
  "dimensions": 2048,
  "normalization": "l2",
  "fingerprint": "<64 lowercase hexadecimal characters>"
}
```

`dimensions` is checkpoint-derived, positive, and at most 8192; 2048 above is illustrative.
`fingerprint` is SHA-256 over canonical compact UTF-8 JSON of the preceding descriptor identity
fields, excluding `model_key` and `fingerprint` to avoid recursion. Object keys are sorted
lexicographically at every depth, arrays retain their declared order, and no insignificant
whitespace is emitted; the cross-language golden fixture is enforced in Rust and Python tests.
`model_key` is the ASCII-safe
`moodboard_{fingerprint}_{dimensions}`. Any checkpoint, prompt, preprocessing, pooling, inference
version, dimension, or normalization change therefore selects a different vector table. The
operator supplies `KHIVE_MOODBOARD_MODEL_DIR` and `KHIVE_MOODBOARD_MODEL_REVISION`.
`KHIVE_MOODBOARD_CHECKPOINT_SHA256` is an optional expected attestation: the pack always computes
the canonical digest and uses that computed value in the descriptor; when the variable is present,
it must be exactly 64 lowercase hexadecimal characters and match or discovery/load fails closed.
The digest covers every regular file (including file symlink targets) below the model directory.
File symlinks are accepted only when their canonical targets remain inside the canonical model
directory; directory and escaping symlinks fail closed. Snapshot copy resolves the path of each
opened file handle and rechecks it against that root, so swapping a source entry or ancestor to an
escaping symlink between enumeration and open also fails closed.
The framing starts with
ASCII `khive-moodboard-checkpoint-v1` plus one NUL byte, then a big-endian `u64` file count and, for each lexicographically
sorted UTF-8 `/`-separated relative path: big-endian `u64` path length, path bytes, big-endian
`u64` file length, and streamed file bytes. Directory symlinks, non-UTF-8/over-4096-byte relative
paths, non-file entries, and more than 100000 files fail closed. This deliberately conservative
directory identity includes auxiliary files and layout as well as all configuration, tokenizer,
manifest, and resolved weight bytes. A one-byte mutation cannot reuse the vector table identity.

The published pack graph directly and exactly pins both `lattice-embed = "=0.9.0"` and its
inference engine `lattice-inference = "=0.9.0"`, matching `inference.version`; a lock refresh cannot
silently float the transitive math implementation under the same descriptor fingerprint.

Each response also carries top-level `experimental: true`. Exact result shapes are:

```text
moodboard.model
  { descriptor, experimental }

moodboard.ingest
  { asset_id, content_ref, created, indexed, descriptor, experimental, embedding }

moodboard.search
  { query_asset_id, descriptor, experimental, hits }
```

Identifiers are canonical bare UUID strings. `content_ref` is the raw 64-character lowercase
BLAKE3 hex used by `BlobStore`. `embedding` is the finite, unit-normalized `f32` row consumed by
downstream calibrated statistical models. Search hits contain
`{asset_id, score, rank, name, content_ref}`; `score` is canonical cosine similarity in `[-1,1]`,
and `rank` is one-based after self-exclusion.

### D3 — Two narrow runtime seams

The runtime adds two consumer seams rather than exposing its backend:

1. `create_entity_with_attachments(..., Vec<NewAttachment>)` is the typed publish-time path.
   Moodboard supplies the original under role `"content"`. The seam requires an installed
   `BlobStore`, verifies every `ContentRef` before mutation, and commits the entity plus roles in
   one transaction before the existing FTS/vector compensation path; compensation hard-deletes
   the entity and attachment rows together.
2. `vectors_for_named_identity(token, &NamedVectorIdentity)` returns a token-scoped vector store.
   `NamedVectorIdentity::new` rejects an empty/unsafe or over-128-byte model key, an empty or
   over-512-byte model name, zero dimension, and dimensions above 8192. The accessor validates the actual vec table's declared dimension
   and any stored `embedding_model` value before returning it. After validation it registers the
   identity in ADR-043's `_embedding_models` lineage with collision-safe engine name `model_key`,
   model id `model_name`, key version `model_key`, and the validated dimension. A provider-wide
   engine name such as `lattice-embed` would incorrectly make immutable descriptor revisions
   contend for ADR-043's one-active-model slot. Pack-owned visual spaces therefore remain visible
   to `engine list`, and old/new descriptor revisions can remain active together.

The vision model is never registered as a text `EmbedderProvider`.

In a multi-backend deployment, every `visual_asset` entity lookup, SQL reuse check, create, and
search-result materialization goes through `pack.runtime().core()` so graph identity remains in
the shared main backend. Descriptor vector tables deliberately remain on `pack.runtime()`, the
backend selected for the Moodboard pack. In a single-backend deployment `core()` is a cheap handle
to that same runtime, so the rule has no storage duplication. The installed `BlobStore` capability
is shared by the two runtime handles.

### D4 — Original bytes are canonical; derived state is repairable

`moodboard.ingest(image_base64, name?, media_type?, caption?)` follows publish-then-reference:

1. Bound and decode base64, identify an allowlisted raster media type, and decode under pixel and
   allocation limits.
2. Produce the governed normalized inference rendition in memory.
3. Publish the original, byte-exact payload through `BlobStore::put`.
4. Reuse a live `artifact/visual_asset` in the caller namespace with the same `content_ref`, or
   create one through D3. The original blob is the entity attachment.
5. Run Lattice inference, validate dimension/finiteness/norm, and replace the identity-specific
   visual vector row.

The normalized PNG is derived cache input, not a persisted attachment in the current
implementation. ADR-160 Phase 4 migrates the original visual and the preference bundle/network
blob anchors to ADR-121 roles without promoting this normalized cache input. A
failure after blob publication may leave an orphan for ADR-111 grace-period GC. A
failure after entity creation may leave an attached asset without the current descriptor row;
retrying the same bytes reuses the entity and heals the vector. `created` reports whether this call
created the entity; `indexed` is true only in a successful response.

The reused asset's properties contain only source-invariant media type, original byte count, and
original pixel dimensions. Governed rendition dimensions and descriptor/model identity are not
stored as mutable scalar asset properties; they belong to the immutable descriptor/vector space.

The lookup-before-create contract is retry-idempotent. A process-wide content-ref-striped critical
section performs the lookup again before create, preventing duplicate first ingests within one
Khive process without serializing unrelated content. Because attachment role `"content"` is indexed
but not uniquely constrained across records, separate Khive processes can still race and create
duplicates. V1 discloses that
cross-process boundary rather than adding a uniqueness rule that would incorrectly apply to every
entity kind or namespace.

### D5 — Governed raster preprocessing

V1 accepts PNG, JPEG, and WebP. Decode limits are 8192 pixels per side and 256 MiB of decoder
allocation; original encoded bytes are bounded by ADR-111's 64 MiB object ceiling. Decoded RGBA is
interpreted as sRGB, composited over `[128,128,128]`, resized without upscaling so the longest side
is at most 448 pixels using Lanczos3, then symmetrically padded to a multiple of 32 pixels and
encoded as RGB8 PNG. The 32-pixel alignment is the pinned Qwen3.5-0.8B checkpoint's 16-pixel
patch times two-patch spatial merge geometry. Model load fails unless `config.json` declares
`patch_size=16` and `spatial_merge_size=2`, so a checkpoint variant cannot silently violate the
preprocessing identity.

ICC transforms and EXIF orientation normalization are not performed in v1. That limitation is
part of the immutable preprocessing revision; changing it requires a new revision and therefore a
new descriptor fingerprint/table.

The fixed prompt revision is `moodboard-style-retrieval-v1`, and inference uses
`PoolingStrategy::MeanVisualTokens`. The response carries its SHA-256, not mutable prompt text.
Under the current causal decoder scaffold, the pooled image-pad positions precede the trailing
prompt and therefore cannot attend to it. V1 is consequently prompt-independent experimental
image geometry, not prompt-conditioned style retrieval. Keeping the actual prompt digest in the
fingerprint is deliberately conservative request-provenance: it fragments mathematically
equivalent spaces today, but prevents a silent identity collision if Lattice later changes request
placement or attention semantics under a new inference version. A gated real-checkpoint
characterization test verifies the present prompt-invariance expectation.

`moodboard.model` prepares the descriptor by copying the validated source tree into a private
random temporary directory, then derives config geometry, hidden size, canonical digest, and the
optional expected attestation from those copied bytes without constructing Qwen weights. File
symlinks are materialized as regular files in the snapshot. Descriptor preparation and model load
are two process-lifetime, cancellation-independent single-flight stages: cancelling the first
awaiting request does not cancel or duplicate an active blocking copy/hash/load, and all later
callers observe the same terminal result. Ingest/search load Lattice only from that prepared
snapshot, re-read geometry and re-hash it after weight construction, compare the complete
descriptor again at publication, and retain the snapshot for the loaded model's entire lifetime so
memory maps cannot outlive their source. Atomic replacement or restoration of the operator's
original path after preparation therefore cannot make a trusted digest name different loaded
bytes.

This binding costs one full checkpoint-sized temporary copy plus two linear snapshot verification
reads (before and after load). The private snapshot tree is sealed read-only after attestation, is
created once per pack process, and is unsealed for deletion only after both the model state and
loaded model release it; the system temporary filesystem must have corresponding free capacity.
Inference is guarded
by a pack-owned
semaphore: `KHIVE_MOODBOARD_INFERENCE_CONCURRENCY` defaults to 1 and must be an integer in 1–4,
preventing one parallel ops-file chunk from launching unbounded Qwen activation memory.
The owned semaphore permit is moved into the blocking inference closure, so cancellation of an
awaiting request cannot release capacity while native Lattice inference is still running.
Raster byte decode and governed preprocessing have a separate pack-owned single-permit gate. Ingest
acquires it before caller base64 decode; search first obtains a backend-verified source through the
shared runtime `BlobHydrator`, then acquires the preprocessing gate before raster decode and
normalization. Search retains the `VerifiedBlob` lease while it waits for and executes preprocessing,
and releases both the raw lease and preprocessing permit after the governed rendition is owned. This
keeps shared raw-byte admission separate from the decoded-raster allocation bound, while still
limiting the ordinary 100-op parallel chunk to one active decoder pipeline. Cold ingest completes
verified model loading before acquiring the gate or decoding caller bytes, preserving the
pre-publication identity fence without retaining a large raster across Qwen construction. Search
releases the gate before a possible cold model load.

### D6 — Exact local cosine retrieval

V1 uses Khive's sqlite-vec store, which performs a brute-force exact cosine scan. The by-ID query
asset lookup is namespace-agnostic after Gate authorization, as required by ADR-007 Rev 6. The
multi-record vector query fans out deterministically across every namespace in the authorized
token visibility set. The ordinary default token therefore searches its primary local namespace
plus configured `visible_namespaces`, while an explicit namespace token remains precisely scoped
to that namespace. Each exact query also filters by `SubstrateKind::Entity` and the descriptor's
exact `model_name`; results are deduplicated by subject, globally reranked by descending canonical
score with ascending UUID tie-break, and truncated to the shared candidate budget. Materialization
accepts only entities whose namespace belongs to that same authorized set. The query asset is
excluded. A
backend whose reported index kind is not
`SqliteVec`/exact is rejected rather than silently weakening the wire claim. `top_k` defaults to
20 and is bounded to 1–100. Search requests at most `4 * top_k + 1` candidates, then drops self,
stale, malformed/missing-BlobStore-reference, attribution-filter-mismatched, and
non-`visual_asset` rows. The
bounded overfetch avoids an unbounded refill loop, so enough stale vectors can still make the
response contain fewer than `top_k` hits; it never returns an unusable locator or fabricates a
replacement score. Candidate blob existence is checked without hydrating bytes; BLAKE3 integrity
is verified when an asset is fetched as a query. Only an exact entity-not-found or
namespace-mismatch condition is treated as a stale row; storage, query, authorization, and
internal failures propagate instead of being disguised as underfill. Every backend score is
checked as finite and in `[-1,1]` before filtering or serialization.

Moodboard writes use the storage layer's explicit permanently-exact insert seam. It preserves the
generic `VectorStore::insert` ANN-delta behavior for identities that may later acquire an ANN
consumer, but transactionally skips `ann_write_log` for this exact-only identity. Initial ingest,
replacement, and metadata repair therefore cannot accumulate deltas with no consumer watermark.

Vector validation is fail-closed: length must equal `dimensions`, every coordinate must be finite,
and L2 norm must be finite and within `1e-3` of 1.0. Khive persists the Lattice-produced normalized
row without renormalizing or coercing invalid output.

### D7 — Local slice only

Cloud/tenant blob placement is deferred until the tenant runtime has a governed blob capability.
Interaction learning, FANN heads, LoRA adapters, cross-model fusion, calibration, and a scalar
"vibe coherence" score are not part of this ADR. A later learning layer may consume the returned
frozen descriptor rows, but it must preserve descriptor identity and keep learned preference
probability separate from conformal/coherence statistics.

## Alternatives Considered

| Alternative                                          | Why rejected                                                                                                |
| ---------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| Keep assets and vectors in the Moodboard application | Duplicates Khive's CAS, artifact identity, attribution, and retrieval substrate.                            |
| Register Qwen vision as a text embedder              | Text entity/note creation would fan strings into an image contract and pollute the visual table.            |
| Store only normalized PNG bytes                      | Loses the original byte identity and makes preprocessing revisions destructive.                             |
| Claim one vector is aesthetic coherence              | Conflates retrieval compatibility with cohesion, diversity, uncertainty, and learned preference.            |
| Add FANN/LoRA in v1                                  | Couples an unvalidated base descriptor to training before the durable asset/retrieval contract is measured. |

## Consequences

### Positive

- Moodboard becomes a Khive consumer and pressure-tests BlobStore, pack composition, and named
  vector identities through the universal MCP surface.
- Original assets, derived vectors, and model provenance have stable, independently inspectable
  identities.
- Ordinary tests can cover preprocessing, attachment, vector identity, persistence, and search
  without shipping a real checkpoint.

### Negative

- Model descriptor preparation copies and verifies the full checkpoint in temporary storage;
  first ingest/search additionally pays a large synchronous Qwen cold load inside a blocking
  worker.
- Separate Khive processes can still race a duplicate first ingest; the in-process lock cannot
  provide a distributed uniqueness guarantee.
- Re-searching an existing asset rehydrates and re-embeds it because `VectorStore` has no get-row
  method.
- Qwen3.5 base pooled retrieval quality remains unvalidated and is marked experimental.

## Implementation Status

Implemented. The accepted first slice includes the opt-in local pack, both runtime seams,
governed-preprocessing goldens, checkpoint-free persistence/search tests, bounded bulk transport,
and ignored real-checkpoint characterization tests. `KHIVE_MOODBOARD_MODEL_DIR` and
`KHIVE_MOODBOARD_MODEL_REVISION` are required; `KHIVE_MOODBOARD_CHECKPOINT_SHA256` is an optional
expected attestation checked against the always-computed canonical digest.

The load-free characterization successfully derived a 2048-dimensional descriptor from a local
Qwen3.5-2B fixture. The historical 0.8.0 inference attempt did not start because that fixture
contains a single `model.safetensors`, while that release required
`model.safetensors.index.json` or `quantize_index.json` to locate `model.visual.*` tensors.
`lattice-embed` 0.9.0 now supports the unindexed single-file layout; the upstream friction in
[lattice#1381](https://github.com/ohdearquant/lattice/issues/1381) was resolved by the merged
[lattice#1385](https://github.com/ohdearquant/lattice/pull/1385).

The same ignored inference gate historically passed with `lattice-embed` 0.8.0 against the
materialized indexed Qwen3.5-0.8B fixture with operator revision
`hf-Qwen-Qwen3.5-0.8B-2fc06364715b967f1860aea9cf38778875588b17`. That run produced a
1024-dimensional descriptor with checkpoint digest
`6dca0d0e661696b36985cbce8f89e1a91377822065de31eac94e90a0e45d43d3`, fingerprint
`40be6f4ae97057e6a0b5c0d011db6e5a37f26c46b787df3e19ddf0fec1e3c9b9`, and model key
`moodboard_40be6f4ae97057e6a0b5c0d011db6e5a37f26c46b787df3e19ddf0fec1e3c9b9_1024`.
Under the current 0.9.0 contract, the unchanged checkpoint bytes map to fingerprint
`bd91f5bf961eb429a6f57b6c16bafde9eeea249d799b1ff0d31e32cf05e5bc8f` and model key
`moodboard_bd91f5bf961eb429a6f57b6c16bafde9eeea249d799b1ff0d31e32cf05e5bc8f_1024`; those values
are descriptor-identity goldens, not evidence of a fresh real-checkpoint inference run.
Before the private-snapshot hardening amendment, load-free descriptor discovery took 98,776 ms and
cold load plus post-load verification took 297,380 ms; those timings are a historical direct-source
baseline and do not include the new copy lifecycle. Three serialized inferences took 279,305 ms,
294,164 ms, and 170,919 ms. The output L2 norm was
`1.000000047`, repeat maximum coordinate delta `0`, and trailing-prompt maximum coordinate delta
`0`. These timings and numerical observations characterize that contended historical 0.8.0
debug-build run, not the current 0.9.0 math or a performance commitment. They are not
retrieval-quality evidence; the current inference identity requires a fresh characterization
before equivalent numerical claims may be made for 0.9.0.

## References

- [ADR-011](ADR-011-embedding-and-inference.md) — Lattice owns inference; Khive owns storage.
- [ADR-017](ADR-017-pack-standard.md) — pack vocabulary and dependency composition.
- [ADR-023](ADR-023-declarative-pack-format.md) — one request surface and non-CRUD verbs.
- [ADR-027](ADR-027-dynamic-pack-loading.md) — inventory registration and opt-in loading.
- [ADR-095](ADR-095-verb-surface-consolidation.md) — CRUD consolidation.
- [ADR-111](ADR-111-blob-store.md) — publish-then-reference CAS and orphan grace GC.
- [ADR-121](ADR-121-attachments-first-class.md) — accepted multi-rendition attachment model.
