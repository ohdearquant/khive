# `resolve_compose_type_weights` tier resolution

Source: `crates/khive-pack-knowledge/src/handlers.rs`. Covers the three-tier lookup
`knowledge.compose` uses to weight section types, and the regression tests (#346) that
pin each tier's behavior.

## Tier order

1. **Tier 1** — an explicit `brain_profile` passed by the caller.
2. **Tier 2** — a namespace-bound brain profile registered via `brain.bind`, dispatched
   across the pack boundary through `load_profile_type_weights`.
3. **Tier 3** — this pack's own tuned `section_posteriors` (Bayesian per-section
   feedback state), falling back to `SectionPosteriorState::default()` only when no
   feedback has been recorded.

Each tier is tried in order; the first one present wins. A pre-#346 regression returned
`SectionPosteriorState::default()` unconditionally from Tier 3, ignoring learned feedback
entirely — the tests below exist to pin that this cannot recur.

## Tier 3: tuned `section_posteriors` (`resolve_compose_type_weights_reads_tuned_section_posteriors_at_tier3`)

With Tiers 1 and 2 absent (empty registry, no brain pack), Tier 3 must read the pack-local
`section_posteriors` rather than silently returning fresh default priors. The test skews
posteriors heavily toward `Formalism` and against `OperationalGuidance` — enough that the
tuned formalism weight exceeds the tuned operational_guidance weight, the opposite of the
default prior ordering (`α_og=6, β_og=1.5` vs `α_form=1.5, β_form=4`). Against the old
`SectionPosteriorState::default()`-inside-`compose` code path, this assertion fails,
because that path always reflects the fresh priors and never observes recorded feedback.

## Tier 2: bound brain profile (`resolve_compose_type_weights_reads_bound_profile_weights_at_tier2`)

With Tier 1 absent (`brain_profile=None`) and a real binding registered via `brain.bind`
for `namespace="local"` + `consumer_kind="knowledge_compose"`, Tier 2 must return the
bound profile's weights. The test registers a profile whose `seed_priors` invert the
default ordering (formalism high, operational_guidance low) and asserts the resolved
weights match it — exercising `load_profile_type_weights` dispatching `brain.profile`
across the pack boundary, the code path that had zero coverage until this test was added.
If `load_profile_type_weights` returns `None` or mis-extracts the profile, Tier 2 falls
through to Tier 3/default and operational_guidance dominates instead — the assertion
catches that regression.
