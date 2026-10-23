# Quantization benchmarks

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-quant`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                            | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ------------------------------------ | ---------------- | --------- | ------------------------------------------ | ----------- |
| `sq8_kernel/384d/GsSq8Codec::l2_sq`  | 127.582 ns       | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/384d/f32_l2sq`           | 305.455 ns       | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/384d/u8_l2sq_u32`        | 123.819 ns       | 1.001     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/f32_l2sq/128`    | 88.303 ns        | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/f32_l2sq/256`    | 196.991 ns       | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/f32_l2sq/384`    | 305.473 ns       | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/f32_l2sq/64`     | 38.258 ns        | 1.001     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/f32_l2sq/768`    | 631.072 ns       | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/u8_l2sq_u32/128` | 42.380 ns        | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/u8_l2sq_u32/256` | 83.118 ns        | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/u8_l2sq_u32/384` | 123.774 ns       | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/u8_l2sq_u32/64`  | 22.034 ns        | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `sq8_kernel/scaling/u8_l2sq_u32/768` | 245.999 ns       | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |

<!-- bench-reference:end -->

The Criterion targets use generated, seeded fixtures and the public scalar
codec APIs. They need no external data or model. The existing `sq8_kernel`
target is preserved.

```sh
cargo bench -p khive-quant --benches -- --quick --noplot
```

For a recorded baseline, run without `--quick` and retain Criterion's estimate
files, including the median estimate, together with the commit, toolchain,
architecture and machine. The new targets use 30 samples, 100 ms warmup and a
one-second measurement request per scenario; Criterion can extend collection
when an operation takes longer. Timing is advisory and must be compared on the
same machine with a rolling and a fixed reference.

## Scenarios

| Target / group                       | Scenarios                                                                                 | Input and measured work                                                               | Baseline |
| ------------------------------------ | ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- | -------- |
| `sq8_kernel/384d`                    | Existing `f32_l2sq`, `u8_l2sq_u32`, `GsSq8Codec::l2_sq`                                   | Two encoded 384d vectors and the f32 reference                                        | pending  |
| `sq8_kernel/scaling`                 | Existing f32 and u8 kernels at 64/128/256/384/768 dimensions                              | Two vectors per dimension                                                             | pending  |
| `sq8_affine/scaling`                 | `approx_dot`, `approx_cosine_dist`, `approx_l2_sq`, each at 64/128/256/384/768 dimensions | Two encoded rows from a 1000-row training corpus; one distance call                   | pending  |
| `sq8_throughput/train_flat_1000x384` | `Sq8Codec`, `GsSq8Codec`                                                                  | Train one codec on 1000 vectors of 384 dimensions; 1000 elements per operation        | pending  |
| `sq8_throughput/encode_1000x384`     | `Sq8Codec`, `GsSq8Codec`                                                                  | Serially encode all 1000 vectors and collect the outputs; 1000 elements per operation | pending  |

There are 19 new configured scenarios: 15 affine-distance combinations, two
training scenarios and two encoding scenarios. The 384d affine cases are part
of the scaling group, rather than repeated under a second identifier.

## Fixtures and timing boundaries

The new fixtures use `StdRng`, seed `0xA052_BE42` XOR the dimensionality, 1000
rows of components drawn from `[-1, 1)`, and normalization of every row to unit
length. Distance comparisons use rows 17 and 29 from the same corpus and codec.
Corpus generation, normalization, codec training and encoding occur before the
distance timers. The measured distance calls include the public API's dimension
checks and numerical work. Both inputs and codec references are opaque to the
optimizer.

The training timers include the complete public `train_flat` call: min/max
scanning, derived metadata, and allocation of the resulting codec. Corpus
construction and destruction of the returned codecs are outside timing.
Encoding trains its codecs once before measurement, calls the public `encode`
method for every row, and includes output allocation and collection into a
vector. Criterion drops the returned batch after timing. It reports elements
per second, where one element means one 384d vector; divide the operation time
by 1000 for the amortized time per vector. This is serial `encode` throughput,
not the `encode_flat_par` or `encode_par` Rayon paths.

Bench setup checks dimensions and encoded lengths; distance fixtures also check
finite results, the cosine distance range and zero self L2 distance. These are
fixture sanity checks, not newly established approximation-error bounds. The
existing ordinary codec accuracy and affine-correction tests remain unchanged.

## Performance guarantees

| Operation                | Input             | Metric                 | Bound                                                                                                              |
| ------------------------ | ----------------- | ---------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `u8_l2sq_u32`            | 384d u8 pair      | latency                | <= 1.5× `f32_l2sq` (ADR-052, `sq8_kernel.rs:5`) — repository-stated timing target, advisory until promoted (#5642) |
| `GsSq8Codec::l2_sq`      | 384d encoded pair | latency                | <= 3× `f32_l2sq` (ADR-052, `sq8_kernel.rs:8`) — repository-stated timing target, advisory until promoted (#5642)   |
| `Sq8Codec::approx_l2_sq` | 384d encoded pair | median latency         | Advisory trend against the recorded baseline                                                                       |
| `Sq8Codec::encode`       | 384d vector       | throughput (vectors/s) | Advisory trend against the recorded baseline                                                                       |

These timing rows are advisory targets until promotion under #5642. Neither a
historical sample nor a Criterion estimate constitutes a cross-machine latency
promise. No mandatory wall-clock ratio failure is added to the benchmark run.

The repository already has `tests/sq8_cost_gate.rs`: an ordinary test exercises
the ratio assertion helper's boundary behavior, while an ignored test measures
the actual kernels and fails above the 1.5x/3x limits. The latter is an explicit
release-mode diagnostic, separate from this reporting-only benchmark command:

```sh
cargo test -p khive-quant --release --test sq8_cost_gate \
  sq8_kernel_cost_bars_are_enforced -- --ignored --exact --nocapture
```

That existing test is unchanged. Its fixed-order wall-clock sampling can be
sensitive to warmup, frequency changes and shared-machine load; its output must
be interpreted in the measurement environment where it ran. Criterion's kernel
estimates allow the same raw/f32 and full-codec/f32 ratios to be calculated, but
Criterion does not itself assert those ratios.

## Historical references and new baseline record

ADR-052 records approximately 12.8 ns for the global-scale kernel and compares
it to an approximately 214 ns f32 dot observation. The retained Criterion
reference is f32 **L2 squared**, so that historical dot comparison is not a new
measurement of this benchmark's L2 ratio. The old benchmark comments describe
1.5x/3x cost bars; the explicit test above is where actual pass/fail sampling
resides. No historical number is replaced here.

| New baseline field               | Value   |
| -------------------------------- | ------- |
| Commit                           | pending |
| Rust toolchain                   | pending |
| Machine / architecture           | pending |
| Affine distance median estimates | pending |
| Training vectors/s               | pending |
| Encoding vectors/s               | pending |

The crate belongs to the `ann-query` benchmark component. Workflow selection
and scheduling are maintained separately from these crate-local targets.
