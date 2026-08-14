# ADR-156: Named-Vector Search Restart and Durability Semantics

**Status**: proposed
**Date**: 2026-08-14
**Authors**: khive maintainers
**Depends on**:

- ADR-148 (Moodboard Visual Retrieval Pack) — descriptor identity and the first named-vector
  consumer
- ADR-155 (Pack Artifact Ingest over BlobStore) — the artifact identity the read path
  cross-checks
- ADR-005 (Storage Capability Traits) — the `VectorStore` seam

## Context

Named vector spaces bind a physical sqlite-vec table (`vec_{model_key}`) to an immutable
descriptor identity: a fingerprint over model name, revision, checkpoint digest, preprocessing
configuration, prompt, pooling strategy, dimensions, and normalization. The moodboard pack
introduced the mechanism; nothing yet states what a process restart means for it. The
questions that matter operationally: which parts survive a restart, which are re-derived, what
guarantees results are identical before and after, and what the read path does when a stored
hit's backing artifact has since disappeared.

These semantics were exercised end to end (a serving process was restarted between indexing
and search, and between training and inference, with exact-equality checks on the results) but
the contract lives only in test code. This record states it normatively.

## Decision

**D1 — Vector tables are durable; identity binding is re-derived, never stored as mutable
configuration.** The `vec_{model_key}` tables and their rows persist in the database across
restarts. The mapping from descriptor identity to table name is a pure function of the
identity's canonical form (fingerprint plus dimensions). A restarted process recomputes the
same `model_key` from the same configured identity and lands on the same table. There is no
stored pointer that can drift from the identity it names.

**D2 — A changed identity selects a new space; it never mutates an old one.** Any change to an
identity field (a new checkpoint digest, a different pooling strategy, changed dimensions)
produces a different fingerprint, hence a different `model_key`, hence a different physical
table. Existing vectors are untouched and remain queryable under their original identity.
Upgrades are therefore additive: old and new spaces coexist, and retiring an old space is an
explicit curation act, not a side effect of reconfiguration.

**D3 — Model state is process-local and reloads from configuration at boot.** Inference
weights are not stored in the database. A restarted process reloads the checkpoint from its
configured path, and the identity fields that participate in the fingerprint (revision,
checkpoint digest) guarantee that a _different_ checkpoint at the same path cannot silently
write into the old space — it lands in a new one per D2.

**D4 — Restart exactness for search.** For an unchanged store and an unchanged descriptor
identity, a search issued after a restart returns the same hits with the same scores as before
it. This follows from D1–D3 plus the retrieval design: exact brute-force cosine over the named
table with deterministic score conversion, no approximate index whose in-memory build state
could differ across processes. Consumers may rely on it; a violation is a defect, not drift to
be tolerated.

**D5 — Read-path orphan policy: skip, do not error, never re-rank.** A hit whose backing
artifact bytes are no longer resolvable (blob removed, entity soft-deleted) is dropped during
result materialization. The remaining hits keep their store-assigned order; materialization
never re-scores or re-ranks. Orphaned rows are a curation concern; the read path's only duty
is to not present results it cannot back with bytes.

## Consequences

- Operators may restart serving processes freely between indexing and query; no re-indexing or
  warm-up invariant is implied for exact-search named spaces.
- Rolling a model forward is safe by construction (D2) at the cost of disk growth per retained
  space; reclaiming old spaces needs an explicit curation path.
- The orphan policy (D5) means result counts can shrink below the requested `top_k` when the
  store has been curated; callers must not treat a short page as an error.
- D4 is stated for exact search. A future approximate index behind the same seam must either
  meet it or amend this record with its weaker guarantee before shipping.

## Alternatives considered

**Storing the identity-to-table binding as configuration rows.** Rejected: a stored binding is
a second copy of the identity that can drift from it; the pure-function derivation cannot.

**Erroring on orphaned hits.** Rejected: it turns routine curation of old artifacts into
search outages for unrelated queries.

**Backfilling or re-ranking around dropped orphans.** Rejected: materialization would then
return results the ranking stage never saw in that order, breaking the filter-before-rank
property of the retrieval design.

## Non-claims

This record does not cover the retrieval scoring design itself (ADR-148), embedding compute
placement, or ANN index persistence for the knowledge corpus (ADR-079's territory). It states
restart semantics for exact-search named vector spaces only.

## References

- `crates/khive-db/src/stores/vectors.rs` — one instance per `vec_{model_key}` table;
  namespace and model predicates applied before rank projection
- `crates/khive-pack-moodboard/src/model.rs` — descriptor identity fingerprint and
  `NamedVectorIdentity` derivation
- `crates/khive-pack-moodboard/src/handlers.rs` — result materialization and orphan skip
