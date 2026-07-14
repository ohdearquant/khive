# Deterministic fixed-point scores

`DeterministicScore` is the common cross-platform ranking value: a transparent `i64` scaled by
`2^32`, with saturating arithmetic and explicit infinity sentinels.

## Representation and sentinels

Finite value `x` is stored as `round(x * 2^32)`. `MAX` is `i64::MAX` and converts to positive
infinity; `NEG_INF` is `i64::MIN + 1` and converts to negative infinity. `MIN` (`i64::MIN`) is
reserved and must not appear as a runtime value. `ZERO` stores raw zero.

## Raw constructors

`from_raw` is intentionally unchecked and can construct the reserved `MIN`; use it only for trusted
internal values. `from_raw_saturating` maps `i64::MIN` to `NEG_INF`, and `from_raw_checked` returns
`None` for it. `to_raw` exposes the stored fixed-point integer.

## Float conversion

`from_f64` rounds finite values to the nearest raw integer, clamps overflow, maps NaN to `ZERO`, and
maps infinities to `MAX`/`NEG_INF`. `from_f32` delegates through `f64`. `to_f64` reverses the scale
for finite values and restores infinities for the two reachable sentinels.

## Arithmetic invariant

Addition, subtraction, integer multiplication, floating multiplication, and division saturate to
`[NEG_INF, MAX]`. Wider intermediates prevent machine overflow, and no arithmetic operation emits
the reserved `MIN`. NaN introduced by floating arithmetic maps to `ZERO`.

## Serialization

The serde wire form is the raw integer. Custom deserialization rejects `i64::MIN`, enforcing the
reserved-sentinel invariant at the untrusted-data boundary.
