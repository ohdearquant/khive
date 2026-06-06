# khive-fold Design

**Scope:** Cognitive primitives — Fold, Anchor, Objective, Selector.

**ADR:** [ADR-024](../../docs/adr/ADR-024-fold-cognitive-primitives.md)

**Last reviewed:** 2026-06-06

---

## Modules

| Module | Purpose |
|--------|---------|
| `fold` | Deterministic reduce: entries → derived state |
| `anchor` | Causal graph traversal (provenance chains) |
| `objective` | Score candidates and select best (ADR-059 precision weighting) |
| `selector` | Budget-constrained pack: many → subset (ADR-058/059) |
| `ordering` | Deterministic IEEE-754 ordering primitives |
| `checkpoint` | Generic snapshot envelope + in-memory store for fold-managed indexes |
| `compose` | Composition combinators: filter, map, sequential, dual |

## Key Invariants

- No clock calls (`Utc::now`). Callers supply `as_of` timestamps explicitly (ADR-024).
- Non-finite scores are rejected at every selection boundary (`passes_score`).
- Non-finite precision falls back to 1.0 (full trust) rather than propagating NaN into ranking.
- Deterministic tie-breaking: UUID ascending after score descending everywhere.

## Dependency Boundary

Per ADR-024, `khive-fold` is a foundation-layer crate. Accepted direct dependencies:
`khive-types`, `khive-score`, `serde`/`serde_json` (optional feature), `uuid`, `chrono`
(DateTime type only, no clock feature), `thiserror`, `blake3` (checkpoint hashing).

## Testing

Inline test sections exceed 300 lines in `selector.rs`, `objective/mod.rs`, and
`ordering/mod.rs` because they exercise private helpers or pub(crate) constants.
See `// INLINE TEST JUSTIFICATION` comments in each file for specifics.

## Failure Modes

- `FoldError::Serialization` — state serialization failed during checkpoint save.
- `FoldError::IntegrityMismatch` — stored BLAKE3 hash does not match recomputed hash on load.
- `FoldError::CheckpointNotFound` — delete or load of a non-existent checkpoint ID.
- `FoldError::LockPoisoned` — RwLock poisoned (thread panic while holding write lock).
- `ObjectiveError::NoCandidates` — `select_deterministic` called with empty slice.
- `ObjectiveError::NoMatch` — no candidate passes the minimum score threshold.
