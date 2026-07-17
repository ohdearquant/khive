# Vamana Testing

**Scope:** `crates/khive-vamana/src/` (inline unit tests) and `crates/khive-vamana/tests/`
**Last reviewed:** 2026-06-06

---

## Test organization

| Location                         | What it covers                                                                                                                                                                                                           |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `src/config.rs` `#[cfg(test)]`   | `VamanaConfig` validation: zero-dimension, zero-degree, non-finite alpha, alpha < 1.0, builder setters                                                                                                                   |
| `src/distance.rs` `#[cfg(test)]` | `l2_squared` correctness, `cosine_from_l2sq`, `try_l2_squared` error on mismatch                                                                                                                                         |
| `src/graph.rs` `#[cfg(test)]`    | `VisitedSet` generation semantics, `VamanaGraph::new` rejections, `greedy_search` correctness on line graphs, tie-breaking, `robust_prune` alpha-squared condition, build determinism, self-loop and duplicate rejection |
| `src/index.rs` `#[cfg(test)]`    | `VamanaIndex::build` / `search` / `save` / `load` / `recall_at_k` / `to_snapshot` / `from_snapshot`; binary corruption tests; non-finite float rejection at all public boundaries                                        |
| `tests/benchmark.rs`             | Integration recall tests at realistic corpus sizes                                                                                                                                                                       |

---

## Adversarial invariants tested

- **NaN / Infinity in build vectors** — `NonFiniteFloat` error returned
- **NaN / Infinity in search query** — `NonFiniteFloat` error returned
- **NaN in snapshot vectors** — `NonFiniteFloat` error returned on restore
- **Duplicate neighbors in `graph.bin`** — `InvalidFormat` returned by `load`
- **Out-of-range neighbor in `graph.bin`** — `InvalidFormat` returned
- **Self-loop in `graph.bin`** — `InvalidFormat` returned
- **Trailing bytes in `graph.bin`** — `InvalidFormat` returned
- **Bad metadata/graph magic** — `InvalidFormat` returned
- **Truncated `vectors.bin`** — `InvalidFormat` returned
- **Stale snapshot fingerprint mismatch** — caller-side detection test

---

## OQ1 no-repair control

`VamanaIndex::tombstone_batch_no_repair` (`#[doc(hidden)]`, test support only) sets
tombstone bits and clears each deleted node's own forward adjacency (updating
`reverse_adj` in lockstep), but does NOT reselect in-neighbor lists via RobustPrune —
no Wolverine rewiring. The medoid is re-elected once at the end if it falls in the
batch. It builds a genuine no-repair control for the OQ1 empirical drift test: search
still skips tombstoned nodes via the `Option<&[u64]>` guard in `greedy_search_inner`,
but in-neighbors that previously pointed to deleted nodes are NOT rewired, so the graph
retains dead-end paths that Wolverine repair would otherwise have bypassed.

---

## Ignored / long-running tests

`benchmark_random_5000x384_recall_at_10_at_least_85_percent` is marked
`#[ignore]` because it builds a 5000×384 index (approximately 60 seconds on
CI). Run it explicitly with:

```sh
cargo test -p khive-vamana --ignored
```

---

## Running all tests

```sh
# From crates/ directory:
cargo test -p khive-vamana

# Including ignored:
cargo test -p khive-vamana -- --include-ignored
```
