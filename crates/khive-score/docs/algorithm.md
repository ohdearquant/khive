# khive-score Algorithms

**Scope:** Design rationale and usage guidance for the scoring algorithms in `khive-score`:
fixed-point representation, distance conversion, aggregation, and deterministic ranking.

**ADRs:** [ADR-006 Deterministic Scoring](../../../../docs/adr/ADR-006-deterministic-scoring.md) |
[ADR-012 Retrieval Composition](../../../../docs/adr/ADR-012-retrieval-composition.md) |
[ADR-024 Fold Cognitive Primitives](../../../../docs/adr/ADR-024-fold-cognitive-primitives.md)

**Sources:**
- [`crates/khive-score/src/score.rs`](../src/score.rs) — `DeterministicScore`
- [`crates/khive-score/src/distance.rs`](../src/distance.rs) — distance conversion
- [`crates/khive-score/src/ops.rs`](../src/ops.rs) — aggregation and fusion
- [`crates/khive-score/src/comparator.rs`](../src/comparator.rs) — `Ranked<T>`

**Tests:** inline `#[cfg(test)] mod tests` in each source file
**Bench:** [`crates/khive-score/benches/score_ops.rs`](../benches/score_ops.rs)

---

## Fixed-point representation

`DeterministicScore` wraps `i64` scaled by $2^{32}$ (`4_294_967_296`). This gives sub-`1e-9`
resolution for scores in `[−1, 1]` and avoids floating-point non-determinism across platforms
(x86_64, ARM64, WASM).

Sentinels:

| Constant | Raw value | Float equivalent |
| -------- | --------- | ---------------- |
| `MAX` | `i64::MAX` | `+∞` |
| `NEG_INF` | `i64::MIN + 1` | `−∞` |
| `MIN` | `i64::MIN` | reserved (never a runtime value) |
| `ZERO` | `0` | `0.0` |

The Lean proof in `proofs/` guarantees that arithmetic operations never produce `MIN`
(the reserved sentinel) — all underflow clamps to `NEG_INF`.

## Distance conversion

See [`docs/api/distance-conversion.md`](api/distance-conversion.md) for formulas, invariants, and the
deprecation note on `score_from_distance`.

## Aggregation

`ops.rs` provides saturation-safe aggregation over `&[DeterministicScore]`:

- `sum_scores` / `avg_scores` — clamp to `[NEG_INF, MAX]` using `i128` intermediates.
- `avg_scores_checked` — additionally returns a saturation flag when intermediate magnitudes
  approach `i64::MAX`.
- `max_score` / `min_score` — return sentinels for empty slices.
- `weighted_sum` — validates all weights are finite before accumulating.

## RRF (Reciprocal Rank Fusion)

`rrf_score(rank, k)` computes $\frac{1}{k + rank}$ as a `DeterministicScore`.

Two named variants enforce rank-base at the type level:

- `rrf_score_one_based(rank: NonZeroUsize, k)` — rank 1 = first result (standard RRF).
- `rrf_score_zero_based(index: usize, k)` — index 0 = first result; converts to 1-based internally.

Using `rrf_score` directly with a raw `enumerate()` index of 0 inflates the top result by
$\frac{1}{k}$ instead of $\frac{1}{k+1}$. Prefer the named variants.

## Ranked<T> ordering semantics

`Ranked<T>` implements `Ord` as a **max-heap adapter**:

- Higher score → `Greater` in `Ord`.
- Equal scores: lower ID → `Greater` (deterministic tie-break).

Consequences:

- `BinaryHeap<Ranked<T>>::pop()` returns the **best** item first (highest score, lowest ID on
  ties). This is the primary intended use.
- `Vec<Ranked<T>>::sort()` produces **ascending** order (lowest score first) because `Ord` is
  inverted. This is the opposite of ranking order.

For descending (ranking) order in a `Vec`, use `cmp_desc_then_id`:

```rust
items.sort_unstable_by(|(sa, ia), (sb, ib)| cmp_desc_then_id(*sa, ia, *sb, ib));
```

## Commands

```bash
# Full crate checks
cargo check -p khive-score
cargo test -p khive-score
cargo clippy -p khive-score -- -D warnings
cargo bench -p khive-score --bench score_ops
```

Last reviewed: 2026-06-06 (v0.2.3)
