# khive-bm25 Benchmark Ledger

## Benchmark Targets

| Target | Location | Harness | Description |
| --- | --- | --- | --- |
| `bench_bm25_wand_vs_bruteforce_zipf_matrix` | `src/index/bench_wand.rs` | `#[ignore]` test | WAND vs brute-force on Zipf-distributed corpora (10K/50K/100K docs, 1-3 query terms, 64 queries each) |

Command:
`cargo test -p khive-bm25 bench_bm25_wand_vs_bruteforce_zipf_matrix -- --ignored --nocapture`

## Release Ledger

### v0.2.6 (2026-06-06)

- **Commit**: `ca7d72d` (staging branch)
- **Toolchain**: rustc 1.94.1 (e408947bf 2026-03-25), debug profile
- **Machine**: arm64 (Apple Silicon), macOS Darwin 25.5.0
- **Dataset**: Zipf-distributed synthetic corpus (exponent 1.07, vocab
  2048, doc length 24-64 tokens), deterministic seed per config.

#### WAND vs Brute-Force (64 queries, k=10)

| Corpus | Query Terms | Brute-Force (ms) | BMW (ms) | Speedup |
| --- | --- | --- | --- | --- |
| 10K docs | 1 | 47.1 | 47.4 | 0.99x |
| 10K docs | 2 | 67.2 | 138.5 | 0.49x |
| 10K docs | 3 | 96.3 | 95.9 | 1.00x |
| 50K docs | 1 | 189.1 | 429.7 | 0.44x |
| 50K docs | 2 | 361.9 | 197.3 | 1.83x |
| 50K docs | 3 | 524.2 | 336.8 | 1.56x |
| 100K docs | 1 | 394.4 | 797.0 | 0.49x |
| 100K docs | 2 | 783.8 | 353.4 | 2.22x |
| 100K docs | 3 | 917.3 | 406.1 | 2.26x |

**Notes**: Single-term queries with common Zipf terms produce large
posting lists where WAND's block-metadata overhead exceeds its pruning
benefit. Multi-term queries at 50K+ docs show 1.6-2.3x WAND speedup
from threshold-based pruning. Numbers are from debug profile; release
builds are ~3-5x faster across the board. NEON SIMD is active on this
machine (aarch64 baseline).

**Regression notes**: None. First formal ledger entry for this crate.

Last reviewed: 2026-06-06
