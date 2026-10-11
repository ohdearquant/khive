# ADR-199: Shared Score Arithmetic

- **Status**: Proposed
- **Date**: 2026-10-11
- **Addresses**: [#4478](https://github.com/ohdearquant/khive/issues/4478)
- **Related**: [ADR-006](ADR-006-deterministic-scoring.md),
  [ADR-012](ADR-012-retrieval-composition.md),
  [ADR-021](ADR-021-memory-pack.md), [ADR-024](ADR-024-fold-cognitive-primitives.md),
  [ADR-033](ADR-033-recall-pipeline.md),
  [ADR-051](ADR-051-section-embeddings-hybrid-compose.md),
  [ADR-081](ADR-081-recall-retune-driver.md),
  [ADR-104](ADR-104-posterior-serving-recall.md),
  [ADR-130](ADR-130-search-response-completeness-and-ranking-evidence.md),
  [ADR-160](ADR-160-shared-pack-infrastructure.md)

## Context and disposition

Packs, runtime objectives and fusion repeat scalar arithmetic while selecting different
score domains, validation rules, time units and numerical cutoffs. Moving an expression
to a lower crate does not make those policies equivalent. The shared owner is
`khive-score`; callers retain policy, configuration, filtering and storage ownership.

This record adds contracts for six arithmetic families. It does not supersede the ADRs
above, change their status, replace fixed-point ranking, or change a retrieval strategy.
ADR-006 continues to govern `DeterministicScore`, its sentinels and deterministic
boundaries. The other records continue to govern their scoring, decay and persistence
policies. Proposed amendments elsewhere are not ratified by this record.

These contracts are proposals, not descriptions of an already implemented API.
Implementation issues #4479–#4484 must wait for acceptance. Adoption issues #4513–#4516
and #4553 additionally require the compatibility evidence below. A docs merge alone
does not satisfy #4478's acceptance dependency or authorize a numerical behavior change.
An explicit adoption decision can ratify the final proposed text before implementation;
there is no requirement to merge a dependent implementation to obtain that decision.
The dedicated status change follows the [ADR lifecycle](README.md#adr-lifecycle).

## Current source and first adopters

The census is pinned to
[`30a67c8ecb2171183afd8461cd3e2e5d59cbb871`](https://github.com/ohdearquant/khive/tree/30a67c8ecb2171183afd8461cd3e2e5d59cbb871).
Line references describe that revision; implementations must refresh the census.

| Family            | Current source anchor and behavior                                                                                                                                                                                                                                                                                                                                                     | Owner and first adoption                              |
| ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------- |
| Unit interval     | [memory scoring:584](../../crates/khive-pack-memory/src/scoring.rs#L584), `normalize_min_score`; `calculate_score`:771 clamps once after all adjustments                                                                                                                                                                                                                               | `khive-score`; memory, #4479/#4513                    |
| Nonunit factors   | [memory scoring:828](../../crates/khive-pack-memory/src/scoring.rs#L828), posterior multiplier in `[0.85,1.15]`; absent posterior gives exactly `1`                                                                                                                                                                                                                                    | Arithmetic in `khive-score`; policy remains in memory |
| Decay             | [memory config:463](../../crates/khive-pack-memory/src/config.rs#L463), `DecayModel::apply`; [runtime objectives:182](../../crates/khive-runtime/src/objectives.rs#L182) and :282; [fold builtin:170](../../crates/khive-fold/src/objective/builtin.rs#L170); [brain fold gate:36](../../crates/khive-pack-brain/src/fold_gate.rs#L36)                                                 | `khive-score`; #4480, then #4513/#4516/#4553          |
| Weights           | [memory rerank:27](../../crates/khive-pack-memory/src/rerank.rs#L27), scaled mean; [config:395](../../crates/khive-pack-memory/src/config.rs#L395), validation; [scoring:458](../../crates/khive-pack-memory/src/scoring.rs#L458), final-score weights                                                                                                                                 | `khive-score`; #4481/#4513                            |
| Min–max           | [memory scoring:654](../../crates/khive-pack-memory/src/scoring.rs#L654) and :703, two distinct degeneracy rules; [fusion weighted:12](../../crates/khive-fusion/src/weighted.rs#L12), integer normalization                                                                                                                                                                           | `khive-score`; #4482/#4513, then fusion               |
| Ordering          | [score comparator:66](../../crates/khive-score/src/comparator.rs#L66), fixed-point comparator; [fusion ordering:11](../../crates/khive-fusion/src/ordering.rs#L11), duplicate; raw float sorts in memory/knowledge/KG/runtime                                                                                                                                                          | `khive-score`; #4483, then #4513–#4515                |
| Vector arithmetic | [compose:336](../../crates/khive-pack-knowledge/src/knowledge/compose.rs#L336), clamped cosine; [search:1834](../../crates/khive-pack-knowledge/src/knowledge/search.rs#L1834), unclamped cosine; [sections:13](../../crates/khive-pack-knowledge/src/knowledge/sections_index.rs#L13), norm cutoff `1e-12`; [Vamana:77](../../crates/khive-vamana/src/distance.rs#L77), cutoff `1e-8` | `khive-score::vector`; #4484                          |

`khive-score` keeps only its current `khive-types` and optional `serde` normal
dependencies. These families need no runtime, pack, storage, clock, registry or I/O
dependency. Math helpers receive values; they do not read configuration or choose defaults
for a particular pack. Pure validation reports typed errors; adapters retain public error
messages and the point at which validation occurs.

## Decision 1 — UnitScore and explicit boundaries

`UnitScore` is a private-field `f64` carrier for finite values in `[0,1]`, with checked
construction and a value accessor. Deserialization, if exposed, uses the same check.
Both signed zeros qualify and their input bits are retained; no zero canonicalization
is hidden in a memory parser adoption. Neither sign gives a new score magnitude.
It is used only where the caller already means a unit-interval quantity. Raw cosine,
distance, BM25, signed rerank values, posterior multipliers and boosted rank scores are
not automatically `UnitScore`. In particular, a brain evidence mass can exceed one.

`from_fraction_or_percent(x)` rejects nonfinite values and values outside `[0,100]`;
`[0,1]` is a fraction, and `(1,100]` is divided by `100`. Thus `1` means one, not one
percent. Conversion to an existing `f32` boundary happens at the same point as today.
`clamp01(x)` rejects nonfinite input and clamps finite input to `[0,1]`. Rejection and
clamping are distinct operations, not interchangeable constructor spellings.

The existing memory final-score rule is an adapter: evaluate its entire `f32` expression
and ordered adjustment chain, return zero for a nonfinite final result, otherwise clamp
once. Inserting a clamp after each multiplication is forbidden as an equivalent adoption.
Likewise, the minimum-score parser must preserve its finite/range errors, not clamp them.

`UnitScore::boost(self, weight, x)` is an explicitly terminal bounded operation:
evaluate `self.value() * (1 + weight * x)`, then clamp the finite result to `[0,1]`.
It satisfies #4479's bounded-boost contract, including saturation at one. It is not
an equivalent replacement for an intermediate multiplication in a longer adjustment
chain. Inputs and intermediates must be finite; overflow returns an error before
clamping. Finite results below zero clamp to zero.

Two scalar helpers belong beside the type rather than returning it:

- `boost_factor(weight, x)` computes `1 + weight * x` without a unit-interval clamp.
- `affine_clamped(mean, weight, lo, hi)` computes
  `clamp(1 + weight * (mean - 0.5), lo, hi)`.

They retain `f32` and `f64` entry points and expression order. Inputs and ordered bounds
must be finite; a nonfinite intermediate is reported, not converted into a unit score.
Their outputs may exceed one or be negative when the supplied domain permits it. The
caller owns parameter validation, the absent-posterior identity, and any legacy nonfinite
fallback. No posterior weights or band constants move into this crate.

Conversion from `UnitScore` to `DeterministicScore` uses the existing `from_f64` boundary:
nearest rounding at scale `2^32`, with halfway values rounded away from zero. The spacing
is `2^-32`; for a unit input the absolute conversion error is at most `2^-33`.
The reverse conversion is checked: only raw values from `0` through `2^32` qualify.
Negative values, values above one and sentinels refuse; no implicit clamp is introduced.
`UnitScore` float computations make no new cross-platform bit-reproducibility promise.
Existing deterministic fusion and persisted integer scores do not switch to float math.
ADR-130's covered rank/component evidence retains its fixed-point backend boundary;
ADR-032's brain event and cross-profile boundaries also remain fixed-point. An inner
float helper does not relax either fence or enlarge ADR-130's knowledge exception.

## Decision 2 — Decay and HalfLife

`HalfLife::try_new(h)` accepts every finite `h > 0`, including positive subnormals.
Zero, negative values, NaN and infinities refuse. Units are supplied by the caller;
`elapsed`, rates and half-life must use the same unit. There is no built-in “days” or
“microseconds” policy. Rate constructors accept finite nonnegative rates, including zero.

The family has four variants and the following mathematical factors for positive time:

| Variant     | Parameters    | Factor at elapsed `t > 0`                          |
| ----------- | ------------- | -------------------------------------------------- |
| Exponential | rate `r >= 0` | `exp(-r*t)`; half-life form `2^(-t/h)`             |
| Hyperbolic  | rate `r >= 0` | `1/(1+r*t)`; half-life form `1/(1+t/h)`            |
| PowerLaw    | `HalfLife h`  | `h/(h+t)`, the existing rational exponent-one form |
| None        | none          | `1`                                                |

PowerLaw is not replaced with exponential decay or an arbitrary exponent parameter.
Exponential and hyperbolic expose half-life constructors, retaining `h` when reciprocal
rate conversion would overflow. For positive `r`, mathematical half-lives are `ln(2)/r`
and `1/r`; rate zero is an identity and has no finite half-life. All three decaying
variants have factor `0.5` at their supplied half-life; `None` deliberately does not.

The checked scalar API validates elapsed time and input before every identity shortcut:
NaN or infinity refuses even when the value or rate is zero. For finite time `t <= 0`,
the input is returned unchanged before decay arithmetic. Zero input and zero rate also
return unchanged, avoiding `0 * infinity` intermediates. A finite
positive product/ratio that overflows denotes the limiting factor zero, not an error or
NaN. Stable evaluation must handle finite `h+t` overflow without incorrectly producing
zero at `h=t`; acceptance includes the largest finite and smallest positive half-lives.
Expose the bounded `factor(elapsed)` separately so raw callers can multiply a mass by
it. If a rate-to-half-life conversion is exposed, an unrepresentable positive result
must refuse rather than construct zero or infinity as a valid `HalfLife`.

The checked scalar application accepts finite values outside `[0,1]` and multiplies by
the factor. A unit-score convenience preserves the invariant without a policy clamp.
This keeps brain mass decay usable above one. Callers may not insert `UnitScore` around
mass or raw salience merely because they use the same factor.

Adoption must preserve these existing differences explicitly:

- Per-note `decay_factor = 0` is meaningful and overrides a profile default. The memory
  exponential/hyperbolic variant uses that rate; its currently unused `_half_life`
  argument must not suddenly control the result. PowerLaw uses its own configured `h`.
- Brain already returns unchanged for `delta_us <= 0`; its seven-day duration stays in
  brain. Fold clamps negative whole-second age and floors a valid half-life at one second.
- Runtime objectives currently evaluate negative ages directly; one clamps its public
  half-life field with `f64::EPSILON`. The new nonpositive-time rule and checked half-life
  cannot silently replace those public behaviors. Their adoption must either preserve
  them through a named compatibility path or obtain an explicit contract amendment and
  changed-case tests. “Unchanged expected values” cannot hide this discrepancy.
- Existing `f32` scoring expressions are not promoted to `f64`, nor `exp` replaced by
  `powf`, under a bit-equivalence claim. Differential evidence defines the allowed error
  and checks downstream ranking, thresholds and adjustment order.
- Stabilization is itself observable: at `salience=0.75, h=t=f64::MAX`, the legacy
  PowerLaw expression returns zero whereas the stable result is `0.375`; at zero time
  and the smallest positive `h`, multiplying `0.5*h` first can underflow. Adoption must
  accept these changed cases explicitly rather than describe them as pure moves.

## Decision 3 — Validated and keyed weights

`ValidatedWeights::try_new` accepts a nonempty ordered sequence of finite nonnegative
values with at least one positive value. Positive support is checked directly, without
requiring the naive raw sum to be finite. `[MAX, MAX]` is valid; all zero is not.
The type retains raw values and can derive a normalized view. It does not silently
normalize every consumer or replace the existing signed fixed-point `weighted_sum` API.

Normalization first divides by the largest weight, then sums those scaled weights in
the supplied order. This preserves equal subnormal weights and avoids overflow of a
raw sum. No epsilon substitutes for a strictly positive weight. Ratios that round to
zero may contribute zero; the API does not promise exact real arithmetic below precision.

`NamedWeights<K>` validates all configured values as finite and nonnegative, but permits
an empty or all-zero map because a keyed consumer can have no active features. A mean
receives a caller-declared, unique ordered key list and matching feature values. Unknown
configured keys do not affect selection, the maximum scale or the denominator. Missing
keys mean zero weight; duplicate keys in the declared sequence refuse.

The recognized positive-weight subset determines the computation. If it is empty, the
result is zero. Zero-weight features are not evaluated; a NaN in one must not contaminate
the result. Active nonfinite feature values return an error. Pack configuration may
continue to reject an invalid unknown-key weight before this arithmetic selection.

For active finite values `x_i`, use weight scale `w_max` and feature scale
`x_scale = max(1, max(abs(x_i)))`. In the declared order, calculate:

```text
u_i = w_i / w_max
n = sum(u_i * (x_i / x_scale))
d = sum(u_i)
mean = clamp(n / d, -1, 1) * x_scale
```

The clamp bounds the normalized intermediate, not the final output: a single active
feature equal to `f64::MAX` returns `f64::MAX`. Do not clamp the result to `[-1,1]` or
`[0,1]`. Memory retains its fixed order: relevance, salience, temporal, text_match,
vector_match. Hash-map insertion order must not change score bits.

These helpers do not join two distinct memory configurations: `RecallConfig` weights
feed pipeline objectives and the score breakdown; `ScoringConfig.weights` controls final
`calculate_score`. Adoption tests each wiring separately. Existing lossy fusion weight
sanitization and fallback are not replaced by strict validation under this record.

## Decision 4 — Min–max bands without policy unification

`min_max_band` accepts scores, finite `floor <= ceiling`, and an explicit degeneracy
policy. Float entry points preserve their input precision (`f32` or `f64`), item order
and cardinality. Empty input returns empty. Nonfinite scores refuse; caller filtering
stays outside the helper. The ordinary mathematical transform is
`floor + clamp((x-min)/(max-min), 0, 1) * (ceiling-floor)`.

The policy supplies a finite nonnegative absolute span tolerance and one of three
fallbacks when `span <= tolerance`: `Ceiling` returns the supplied ceiling directly;
`UpperEndpoint` evaluates `range = ceiling - floor` followed by `floor + range`;
`ClampMaximum` returns `clamp(max, floor, ceiling)`. `UpperEndpoint` retains separately
rounded subtraction and addition in the caller's precision. It must not be widened,
reassociated or simplified to `Ceiling`. The ordinary affine transform likewise retains
separate subtraction, division, multiplication and addition, without fused multiply-add.
There is no universal epsilon or automatic signal gate.
Implementation must report an unrepresentable intermediate rather than silently drop
an item; any stable reformulation must be checked against each adopting caller.

Memory keeps its finite/minimum-score filter, positive-maximum guard, configured floor
and `0.82` ceiling. Both paths use `f32::EPSILON`; single-source calibration uses
`ClampMaximum` and then applies its signal-strength factor, while RRF calibration uses
`UpperEndpoint` without that factor. With the default `0.15f32` floor and `0.82f32`
ceiling, the current RRF fallback produces bits `0x3f51eb84`; returning the ceiling
directly produces `0x3f51eb85`. Even this default path is not bit-equivalent. Those are
deliberate compatibility policies, not duplicate constants to erase. The shared
operation only replaces the agreed arithmetic between those steps.

Fixed-point fusion keeps a separate exact `DeterministicScore` entry point: i128
differences/products, integer division, and equal/singleton inputs mapping to one for
its `[0,1]` band. It must not round-trip through floats. The float and fixed-point
helpers share ownership and terminology, not an assertion of identical representations.

## Decision 5 — Raw float total ordering

`khive-score` owns score-descending, ID-ascending comparators for `f32` and `f64`.
Every non-NaN value precedes every NaN. Non-NaN values use descending `total_cmp`,
including infinities and signed zero (`+0` before `-0`); equal score order breaks by
ascending ID. NaN payloads and signs are equivalent, and IDs order NaNs. Identical
score category and ID compare equal. No fixed-point conversion is performed.

This matches the proposal in [#5142](https://github.com/ohdearquant/khive/pull/5142) at
`6af4a0fc82c84e1403eb1787fc9d27b956995f46`, which is open at this census. Its
`cmp_desc_then_id_f32`/`f64` additions and fusion delegation must be reconciled with
this contract, not duplicated or described as already landed. Fusion's fixed-point
comparator becomes a direct call to the existing fixed-point comparator in `khive-score`.

Replacing `partial_cmp(...).unwrap_or(Equal)` changes NaN placement and may change signed
zero ties. Each adoption enumerates those differences and pins ID tie behavior; an
unchanged-results assertion applies only to its proven domain. This does not authorize
new raw-float ranking at a boundary governed by ADR-006.

## Decision 6 — Vector arithmetic owner and epsilon migration

`khive-score::vector` owns shared `f32` cosine and in-place L2 normalization. No new
`khive-quant` dependency or vector storage responsibility is introduced. The proposed
canonical numeric cutoff is `1e-8`, with separate named tests for the two operations:
cosine compares a product of norms; normalization compares one norm. Matching literals
do not make those quantities dimensionally interchangeable.

Cosine preserves sequential `f32` accumulation. Empty or mismatched inputs return zero;
a denominator below `1e-8` returns zero; otherwise return the quotient, with an explicit
clamp option for the caller that currently clamps to `[-1,1]`. Normalization computes
the sequential squared-sum norm and divides components only when `norm > 1e-8`.
Equality leaves the vector unchanged. Neither helper validates or sanitizes nonfinite
coordinates: existing caller validation remains responsible. Tests preserve NaN and
overflow outcomes rather than describing these functions as universally finite.

[#5146](https://github.com/ohdearquant/khive/pull/5146) at
`0bd8428e320b8175b522d9211bc8ebd957996b65` proposes these helpers and is open at this
census. It is not evidence that the epsilon migration is accepted. Knowledge section
normalization currently divides for norms above `1e-12`; replacing it changes vectors
whose norms lie in `(1e-12,1e-8]`. This is a real persisted-data behavior change.

Before that caller adopts the canonical cutoff, its implementation must establish the
namespace/index version boundary, treatment of existing stored vectors, and explicit
reindex or no-reindex decision with justification. Old and new normalization must not
be assumed compatible. Regression fixtures cover below, equal to and above both cutoffs,
and retrieval from preexisting rows. A compatibility adapter may retain `1e-12` pending
that separate migration decision; it is not the canonical default. The pure math helper
never rewrites vectors, chooses a namespace, or triggers a reindex.

## Acceptance and sequence

Each implementation supplies the refreshed caller census, owning crate, removed duplicate
sites, and tests pinning the actual boundaries below. No new empirical quality, latency,
or statistical efficacy claim is made by these algebraic contracts.

| Family           | Required positive cases and meaningful negative controls                                                                                                                                                                           |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Unit and factors | Fractions/percent boundary `1`, endpoints, invalid/nonfinite refusal, half-step quantization; negative cosine and mass `1.5` remain nonunit; moving the terminal memory clamp earlier changes a fixture and must fail equivalence  |
| Decay            | Each decaying variant at zero, negative time, `h`, extreme finite `h`; value `1.5`, rate zero, explicit per-note zero, None; invalid half-lives refuse; replacing rational PowerLaw with exponential fails a non-half-life fixture |
| Weights          | Existing rerank tests for MAX/subnormal weights, huge features, unknown keys, zero-weight NaN and map insertion order; replacing scaled accumulation with a naive raw sum must fail                                                |
| Bands            | Empty/singleton/equal/near-equal scores, all three fallback policies, filtering and signal factor; default f32 RRF endpoint bits and fixed-point raw results unchanged; simplifying UpperEndpoint to Ceiling must fail             |
| Ordering         | Antisymmetry/transitivity over finite values, infinities, both zeros, several NaN signs/payloads and ID ties; replacing NaN handling with unordered-equals must fail                                                               |
| Vectors          | Current cosine mismatch/empty/clamp behavior, normalization boundary fixtures and nonfinite outcomes; blind `1e-12` to `1e-8` adoption must fail the legacy fixture until its migration is accepted                                |

Keep current caller tests unchanged for behavior-preserving moves. Add differential tests
around the old implementation before removing it. A changed result is reported with its
domain and disposition; it is not hidden by widening tolerances or updating every expected
value. Preserve existing error timing, public diagnostics and data-format boundaries.
Tests for memory configuration must distinguish breakdown weights from final rank weights.
Existing anchors include `calculate_score_floors_nonfinite_adjustment_chain`,
`exponential_uses_note_decay_factor_not_half_life`,
`finite_weights_with_overflowing_sum_stay_normalized`,
`smallest_subnormal_weights_stay_normalized`,
`nan_in_an_unweighted_feature_does_not_contaminate_the_score`, and
`weight_map_insertion_order_does_not_change_the_score_bits` in the cited memory files.

Land the accepted contracts before dependent primitives, then adopt one family at a time.
Reconcile #5142/#5146 at their current heads and retain their useful tests. Numerical
policy migrations and public API changes need their own explicit acceptance; the smaller
ownership PR is not permission to bundle them. No unrelated crate removal, scoring-policy
default, calibration constant, storage rewrite or retrieval redesign is part of this work.
