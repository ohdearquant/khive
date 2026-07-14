# Objective

Technical reference for the `Objective` trait's precision-weighted scoring and the
built-in `ConsensusObjective` combinator (`objective/` module).

## Precision-weighted scoring

Both the `Objective` trait and the `Selector` (see [selector.md](selector.md)) implement
precision-weighted scoring as specified in ADR-024. The `precision()` hook returns an
inverse-variance estimate in $(0, 1]$ for each candidate's score. The default is 1.0 (fully
trusted). The effective ranking score is:

$$\text{effective} = \text{score} \times \text{precision}$$

Non-finite precision falls back to 1.0. This allows objectives derived from uncertain models
(e.g., embedding similarity) to discount their own scores without propagating NaN into
ranking.

## `ConsensusObjective`

Uses the geometric mean of sub-objective scores:

$$\text{score} = \exp\!\left(\frac{1}{n}\sum_{i=1}^{n}\ln s_i\right)$$

Any sub-score at or below zero causes the consensus to return `0.0` (not an error). Callers
relying on `ConsensusObjective` should ensure sub-objectives return strictly positive scores
for passing candidates.

## Errors

- `ObjectiveError::NoCandidates` — `select_deterministic` called with empty slice.
- `ObjectiveError::NoMatch` — no candidate passes the minimum score threshold.
