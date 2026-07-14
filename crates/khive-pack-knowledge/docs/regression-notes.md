# Regression test notes

Source: `crates/khive-pack-knowledge/src/handlers.rs` `#[cfg(test)]` module.

## `resolve_compose_type_weights_reads_tuned_section_posteriors_at_tier3` (#346)

`resolve_compose_type_weights` must read pack-local `section_posteriors` (Tier 3) rather
than silently returning `SectionPosteriorState::default()` regardless of learned feedback.

Setup: heavily skew `section_posteriors` toward `Formalism` and against
`OperationalGuidance`. With an empty `VerbRegistry` (no brain pack), tiers 1 and 2 fall
through and tier 3 must return the tuned weights — where formalism's weight now exceeds
operational_guidance's, the opposite of the default prior (α_og=6,β_og=1.5 vs
α_form=1.5,β_form=4).

The old code path (`SectionPosteriorState::default()` inside `compose`) would always
return weights reflecting the fresh priors, ignoring `section_posteriors` entirely — this
test would fail against that behavior.

## `resolve_compose_type_weights_reads_bound_profile_weights_at_tier2` (#346 Tier-2)

`resolve_compose_type_weights` must read weights from a namespace-bound brain profile when
one is registered via `brain.bind`.

This test exercises `load_profile_type_weights` dispatching `brain.profile` across the
pack boundary — the code path that had zero coverage after the Tier-3 test was added.

Setup: register a brain profile with `seed_priors` that INVERT the default ordering
(formalism high, operational_guidance low), bind it for `namespace="local"` +
`consumer_kind="knowledge_compose"`, then call `resolve_compose_type_weights` with a
registry that has `BrainPack` wired.

With Tier 1 absent (`brain_profile=None`) and a real binding in Tier 2, the method must
return the bound profile's weights — formalism dominant. If `load_profile_type_weights`
returned `None` or mis-extracted, Tier 2 falls through to Tier 3/default and
operational_guidance dominates → FAIL.
