# BLAKE3 `pure` accepted risk for lattice 0.9.0

## Decision

Khive accepts the workspace-wide BLAKE3 `pure` performance risk only while its
exact lattice dependency pins remain at 0.9.0. The acceptance is explicit
because the measured CPU-hashing delta is material for payloads of 4 KiB and
larger; it is not a claim that the regression is negligible.

This is the bounded tradeoff:

- `lattice-embed` 0.9.0 deliberately enables `blake3` features `std,pure` so
  little-endian aarch64 consumers can cross-compile without a target C
  toolchain. [lattice#1299](https://github.com/ohdearquant/lattice/issues/1299)
  records the failure and portability requirement.
- Cargo feature unification applies `pure` to the one BLAKE3 1.8.5 instance
  used by Khive's blob, database, fold, moodboard, and Vamana paths. A
  downstream manifest cannot subtract that transitive feature.
- The merged [lattice#1403](https://github.com/ohdearquant/lattice/pull/1403)
  removes BLAKE3 from `lattice-embed` in favor of SHA-256. It belongs to
  lattice 0.10.0 because the stable `ModelProvenance::hash` digest changes.
  Lattice 0.10.0 is not published as of this measurement, and Khive has no
  Rust `ModelProvenance` consumer.

The exit condition is the first compatible published lattice release containing
#1403: update all exact lattice pins together, confirm the feature graph no
longer activates `blake3/pure`, and rerun the affected hash and persistence
checks. This acceptance must not silently carry beyond the 0.9.0 pins.

## Measurement

Measured 2026-08-30 on an Apple M2 Max MacBook Pro (12 cores, 32 GiB, arm64),
Darwin 27.0.0, with `rustc 1.98.0` and `cargo 1.98.0`. Both arms used BLAKE3
1.8.5, an optimized release build, identical input bytes, and seven samples per
payload. Iterations were calibrated until one sample lasted at least 250 ms.
The arm order was ABBA: default, pure, pure, default.

The table reports each arm's median MiB/s, followed by the midpoint of the two
arm medians. `pure delta` compares those midpoints.

| Payload | default A1 / A2 | pure B1 / B2 | midpoint default | midpoint pure | pure delta |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 64 B | 773.28 / 736.93 | 744.43 / 807.34 | 755.11 | 775.89 | +2.8% |
| 4 KiB | 1644.56 / 1526.62 | 793.29 / 773.98 | 1585.59 | 783.64 | -50.6% |
| 1 MiB | 1636.77 / 1658.35 | 788.86 / 779.78 | 1647.56 | 784.32 | -52.4% |
| 64 MiB | 1660.80 / 1693.69 | 770.57 / 773.77 | 1677.25 | 772.17 | -54.0% |

The 64-byte arm does not show a stable regression under this resolution. For
4 KiB through 64 MiB, `pure` delivers about 46-49% of the default backend's
throughput (a 2.0-2.2x CPU-hashing slowdown).

No exclusive benchmark window or idle-host gate was available. ABBA ordering
bounds gradual drift but cannot remove arbitrary co-tenant interference. In the
64 MiB row, the two medians varied by 2.0% within the default backend and 0.4%
within `pure`, well below the 54.0% between-backend delta; that separation makes
the direction and rough size useful while leaving a controlled multi-host run
as the stronger evidence if this acceptance ever needs to be extended.

## Reproduction

The harness is `bench/blake3-feature-compare`. Build each feature graph into a
separate target directory, then run the binaries in ABBA order:

```text
CARGO_TARGET_DIR=target/blake3-default cargo build --release --locked --manifest-path bench/blake3-feature-compare/Cargo.toml --no-default-features
CARGO_TARGET_DIR=target/blake3-pure cargo build --release --locked --manifest-path bench/blake3-feature-compare/Cargo.toml --features pure

target/blake3-default/release/blake3-feature-compare
target/blake3-pure/release/blake3-feature-compare
target/blake3-pure/release/blake3-feature-compare
target/blake3-default/release/blake3-feature-compare
```

The checksum read and `black_box` calls keep the digest computation observable.
The harness uses separate Cargo invocations because enabling `pure` and the
default backend in one dependency graph would unify the feature and invalidate
the comparison.

## Scope and residual risk

This is an isolated in-memory CPU-throughput measurement, not an end-to-end
latency claim. BlobStore and Vamana hash whole buffers, so the larger payloads
bound a real hot portion of those operations. Disk, S3, SQLite, allocation, and
serialization can hide some of that CPU delta in wall-clock measurements; this
run does not quantify how much. It also does not characterize x86_64.

Correctness and persisted digest bytes do not change: both arms implement the
same BLAKE3 algorithm. The accepted risk is performance only, justified for the
bounded 0.9.0 window by the cross-compilation requirement and the already-merged
upstream exit. If lattice 0.10.0 publication is delayed past the next dependency
refresh, the decision must be reopened rather than copied forward.
