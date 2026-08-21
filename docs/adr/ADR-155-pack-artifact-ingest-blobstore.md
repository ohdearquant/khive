# ADR-155: Pack Artifact Ingest over BlobStore

**Status**: proposed
**Date**: 2026-08-14
**Authors**: khive maintainers
**Superseded by**: proposed [ADR-160](ADR-160-shared-pack-infrastructure.md) in full on its
acceptance; ADR-160 converges on accepted ADR-121 attachments and closes the backend allocation
bound deferred here while retaining typed absent-store refusal and pack-owned self-description.
**Depends on**:

- ADR-111 (Blob Store) — the `BlobStore` content-addressed storage trait and the
  `blob.put` / `blob.get` / `blob.stat` verb surface
- ADR-017 (Pack Standard) — pack-owned entity subtypes
- ADR-148 (Moodboard Visual Retrieval Pack) — the first implementation of this contract

## Context

ADR-111 specifies the BlobStore trait and its generic verb surface, but it does not say how a
_pack_ should anchor its own typed artifact entities to stored bytes. The moodboard pack built
that pattern for visual assets: original bytes go into the content-addressed store, a typed
`artifact` entity carries the returned `ContentRef`, and every later read re-verifies the bytes
against that identity. The pattern is general — any pack that ingests external media (images,
documents, recordings, model bundles) faces the same four questions: what is the canonical
identity of the bytes, how are duplicates handled, what guarantees a read returns the bytes
that were written, and what happens when no store is installed.

Today the answers live only in `khive-pack-moodboard` handler code. A second media-ingesting
pack would either re-derive them or diverge silently. This record makes the contract normative
so future packs implement it rather than reinvent it.

## Decision

A pack that stores external bytes and registers typed artifact entities for them MUST follow
this contract:

**D1 — ContentRef is the canonical identity of the original bytes.** The pack stores the raw
decoded bytes via `BlobStore::put` and records the returned BLAKE3 `ContentRef` on the entity.
Derived representations (thumbnails, embeddings, normalized forms) never replace the original
bytes' identity; they are separate records that reference it.

**D2 — Deduplication is by ContentRef, before entity creation.** Ingest queries for an existing
entity carrying the same `content_ref` and returns it instead of creating a duplicate. This
carries the ADR-148 idempotence boundary unchanged: the lookup-before-create critical section
is process-wide, so the one-entity guarantee holds within a single Khive process; separate
processes can still race and create duplicates because `entities.content_ref` is indexed but
not unique. This record does not tighten that boundary. The
store's own idempotent put (identical content, same ref, no re-write) handles the byte layer;
this decision extends the same idempotence to the entity layer.

**D3 — Every read back is a verified read.** When a pack hydrates stored bytes to act on them,
it re-verifies the BLAKE3 digest against the recorded `ContentRef` before use. A mismatch is an
integrity error surfaced to the caller, never silently tolerated. Digest verification lives on
the read path by design: that is where the bytes are already in hand.

**D4 — Source reads are bounded.** Reads of stored source bytes carry an explicit size bound
chosen by the pack for its media class. Under the current `BlobStore` capability the bound is
enforced by a size preflight before the read and a length check after it; the read itself
returns a whole buffer, so a store whose object grows between preflight and read is detected
only after the allocation. An over-bound object is a typed refusal in either case. A
backend-enforced bounded or streaming read, which would close the preflight/read race and make
the refusal precede allocation, is a `khive-storage` capability extension deferred to its own
record.

**D5 — Absent store fails closed with a typed refusal.** A pack verb that requires the
BlobStore returns a typed unconfigured error naming the missing capability when no store is
installed. It does not degrade to storing bytes inline, writing to scratch paths, or silently
skipping persistence.

**D6 — The artifact entity is self-describing.** The entity's properties record at minimum a
`schema_version` for the pack's artifact shape and the media metadata needed to interpret the
bytes without fetching them (for visual assets: media type and pixel dimensions).

## Consequences

- Ingest is idempotent end to end within one Khive process: repeated ingest of identical bytes
  converges on one blob and one entity. Across processes the blob layer still converges (the
  store's put is content-addressed), but entity creation can race per the ADR-148 boundary.
- Corruption anywhere between write and read is detected at the read seam, the only place it
  can be detected cheaply.
- A pack's artifact entities remain interpretable from the graph alone (D6), while the bytes
  stay in the store.
- Packs pay a digest re-computation on every source read (D3). For the media sizes bounded by
  D4 this is negligible against the read itself.

## Alternatives considered

**Trust-on-read (no digest re-verification).** Rejected: the store is content-addressed at
write time only; filesystem corruption or an operator replacing files under the store root
would otherwise propagate silently into derived computations.

**Entity-layer dedup by perceptual or semantic similarity.** Rejected for the ingest contract:
identity here means byte identity. Near-duplicate handling is a retrieval-layer concern and
varies per pack.

**Inline bytes on the entity when the store is absent.** Rejected: it forks the persistence
model on a configuration accident and violates the graph/bytes separation ADR-111 establishes.

## Non-claims

This record does not govern retrieval quality, embedding identity, or vector storage — those
are ADR-148 (for moodboard) and the named-vector durability record (ADR-156). It does not add
verbs; the generic blob verb surface remains ADR-111's.

## References

- `crates/khive-pack-moodboard/src/handlers.rs` — reference implementation: dedup-by-ref entity
  creation, bounded verified source reads, typed unconfigured refusal
- ADR-111 (Blob Store) — trait, verbs, idempotent put
