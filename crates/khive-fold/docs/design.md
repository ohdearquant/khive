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

### Fold Cognitive Primitives (no-clock rule) (ADR-024)

`FoldContext` and `ObjectiveContext` both default `as_of` to `DateTime::<Utc>::default()`
(Unix epoch) rather than calling `Utc::now()`. This is deliberate: the foundation layer must
be clock-free so that fold operations are deterministic and testable without time injection.
Callers that need the current time must use `FoldContext::at(Utc::now())` or
`ObjectiveContext::at(Utc::now())` explicitly.

The same rule applies to `Checkpoint::new` and `Checkpoint::with_hash`: `created_at` is
set to the epoch on construction. Callers that need a real wall-clock timestamp should set
`checkpoint.created_at = Utc::now()` after construction.

### ADR-024 §"Bayesian extensions": Selector Budget Packing and Precision-Weighted Scoring

Selector budget packing and precision-weighted scoring are both specified in ADR-024
("Fold Cognitive Primitives") under the §"Bayesian extensions" section — there are no
separate ADR-058 or ADR-059 documents. See [api/selector.md](api/selector.md) and
[api/objective.md](api/objective.md) for the technical reference (formulas, tie-breaking,
precision pitfalls); this section covers only the design intent.

`SelectorInput.information_gain` is caller-supplied because the Selector is pure-math with
no embedding access. This is an intentional design boundary: callers that have embedding
models pre-compute KL divergence proxies before calling `select`.

## Testing

Inline test sections exceed 300 lines in `selector.rs`, `objective/mod.rs`, and
`ordering/mod.rs` because they exercise private helpers or pub(crate) constants.
See `// INLINE TEST JUSTIFICATION` comments in each file for specifics. Test-by-test
rationale for the non-obvious regression tests lives with the subsystem they cover:
[api/selector.md](api/selector.md#test-coverage), [api/checkpoint.md](api/checkpoint.md).
