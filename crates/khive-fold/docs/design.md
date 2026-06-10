# khive-fold Design

**Scope:** Cognitive primitives — Fold, Anchor, Objective, Selector.

**Last reviewed:** 2026-06-06

---

## Modules

| Module       | Purpose                                                              |
| ------------ | -------------------------------------------------------------------- |
| `fold`       | Deterministic reduce: entries → derived state                        |
| `anchor`     | Causal graph traversal (provenance chains)                           |
| `objective`  | Score candidates and select best (precision-weighted scoring)        |
| `selector`   | Budget-constrained pack: many → subset                               |
| `ordering`   | Deterministic IEEE-754 ordering primitives                           |
| `checkpoint` | Generic snapshot envelope + in-memory store for fold-managed indexes |
| `compose`    | Composition combinators: filter, map, sequential, dual               |
| `pipeline`   | ComposePipeline: objective scoring + selector budget packing         |

## Key Invariants

- No clock calls (`Utc::now`). Callers supply `as_of` timestamps explicitly. The foundation
  layer defaults `as_of` to the Unix epoch so contexts are safe to construct without
  knowing the wall-clock time.
- Non-finite scores are rejected at every selection boundary (`passes_score`).
- Non-finite precision falls back to 1.0 (full trust) rather than propagating NaN into ranking.
- Deterministic tie-breaking: UUID ascending after score descending everywhere.

## Dependency Boundary

`khive-fold` is a foundation-layer crate. Accepted direct dependencies:
`khive-types`, `khive-score`, `serde`/`serde_json` (optional feature), `uuid`, `chrono`
(DateTime type only, no clock feature), `thiserror`, `blake3` (checkpoint hashing).

## ADR Compliance

### ADR-024: Fold Cognitive Primitives (no-clock rule)

`FoldContext` and `ObjectiveContext` both default `as_of` to `DateTime::<Utc>::default()`
(Unix epoch) rather than calling `Utc::now()`. This is deliberate: the foundation layer must
be clock-free so that fold operations are deterministic and testable without time injection.
Callers that need the current time must use `FoldContext::at(Utc::now())` or
`ObjectiveContext::at(Utc::now())` explicitly.

The same rule applies to `Checkpoint::new` and `Checkpoint::with_hash`: `created_at` is
set to the epoch on construction. Callers that need a real wall-clock timestamp should set
`checkpoint.created_at = Utc::now()` after construction.

### ADR-024 §"Bayesian extensions": Selector Budget Packing and Precision-Weighted Scoring

These behaviors are both specified in ADR-024 ("Fold Cognitive Primitives") under the
§"Bayesian extensions" section. There are no separate ADR-058 or ADR-059 documents.

**`GreedySelector`** implements priority-ordered budget packing with category-weight multipliers
and deterministic tie-breaking (effective-score desc, size asc, id asc). The `diversity_bias`
field controls how aggressively the selector prefers different categories at each pick step.
At `diversity_bias = 0.0` the behavior reduces to a single-pass greedy sort (backward-compatible).

**Precision-weighted scoring** — Both the `Objective` trait and the `Selector` implement
precision-weighted scoring as specified in ADR-024:

_Objective_: The `precision()` hook returns an inverse-variance estimate in $(0, 1]$ for
each candidate's score. The default is 1.0 (fully trusted). The effective ranking score is

$$\text{effective} = \text{score} \times \text{precision}$$

Non-finite precision falls back to 1.0. This allows objectives derived from uncertain models
(e.g., embedding similarity) to discount their own scores.

_Selector_: The `SelectorWeights.epistemic_weight` field adds an information-gain bonus to
the pragmatic score. Effective selection score:

$$\text{effective} = \text{pragmatic\_score} + \text{epistemic\_weight} \times \text{information\_gain}$$

`information_gain` is pre-computed by callers (the Selector has no access to embedding space).
At `epistemic_weight = 0.0` the behavior is identical to pure pragmatic selection.

## Consistency Notes

- `SelectorInput.information_gain` is caller-supplied because the Selector is pure-math with
  no embedding access. This is an intentional design boundary: callers that have embedding
  models pre-compute KL divergence proxies before calling `select`.
- `ConsensusObjective` uses the geometric mean of sub-objective scores:
  $\text{score} = \exp\!\left(\frac{1}{n}\sum_{i=1}^{n}\ln s_i\right)$.
  Any sub-score at or below zero causes the consensus to return 0.0 (not an error). Callers
  relying on ConsensusObjective should ensure sub-objectives return strictly positive scores
  for passing candidates.
- `InMemoryCheckpointStore.load_latest` breaks `created_at` ties by `uuid` (lexicographic
  ascending). Callers should not rely on ordering when `created_at` values are equal.

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
