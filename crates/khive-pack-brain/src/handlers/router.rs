use super::*;

// ── lattice-router seam (#345 M1 / #352 M2) ──────────────────────────────────

/// Context-vector dimension for the lattice-fann router seam.
/// Layout:
///   [0..2]  = relevance  {mean, ess}
///   [2..4]  = salience   {mean, ess}
///   [4..6]  = temporal   {mean, ess}
///   [6..]   = one section posterior mean per type in `SectionType::all()` order
#[cfg(feature = "lattice-router")]
pub(super) const ROUTER_CONTEXT_DIM: usize = 6 + SectionType::ALL.len();

/// Build a `ROUTER_CONTEXT_DIM`-element context vector from live brain posteriors.
///
/// ESS values are stored as raw f32 casts.  Normalization is deferred to the
/// engine `route()` seam (#343) once the network is trained; documenting the
/// omission here so it is not forgotten.
///
/// When `sections` is `None` the section slots are filled from
/// `SectionPosteriorState::default_priors()` means — deterministically neutral,
/// never arbitrary zeros.
#[cfg(feature = "lattice-router")]
pub(super) fn build_context_vector(
    relevance: &BetaPosterior,
    salience: &BetaPosterior,
    temporal: &BetaPosterior,
    sections: Option<&std::collections::HashMap<SectionType, BetaPosterior>>,
) -> [f32; ROUTER_CONTEXT_DIM] {
    let mut v = [0.0f32; ROUTER_CONTEXT_DIM];
    v[0] = relevance.mean() as f32;
    v[1] = relevance.effective_sample_size() as f32;
    v[2] = salience.mean() as f32;
    v[3] = salience.effective_sample_size() as f32;
    v[4] = temporal.mean() as f32;
    v[5] = temporal.effective_sample_size() as f32;
    // Section slots: deterministic ordering via SectionType::all().
    // Pre-compute default priors unconditionally so that slots absent from a
    // caller-supplied partial map fall back to the configured prior mean, not
    // an arbitrary 0.5 that would disagree with the seeded posteriors.
    let default_priors = SectionPosteriorState::default_priors();
    let posteriors: &std::collections::HashMap<SectionType, BetaPosterior> =
        sections.unwrap_or(&default_priors);
    for (i, st) in SectionType::all().iter().enumerate() {
        let mean = posteriors
            .get(st)
            .or_else(|| default_priors.get(st))
            .map_or(0.5, |p| p.mean());
        v[6 + i] = mean as f32;
    }
    v
}

/// M1 seam: links lattice-fann and produces routed mixture weights.
/// The real engine `route(context_vector, available_adapters)` (lattice mixture-runtime,
/// #343) swaps in here when shipped; M1 proves the dependency links and the seam runs.
#[cfg(feature = "lattice-router")]
pub(super) fn route_via_fann(context: &[f32]) -> Vec<f32> {
    use lattice_fann::{Activation, NetworkBuilder};
    const ADAPTER_SLOTS: usize = 4;
    let mut network = match NetworkBuilder::new()
        .input(ROUTER_CONTEXT_DIM)
        .hidden(8, Activation::ReLU)
        .output(ADAPTER_SLOTS, Activation::Softmax)
        .build()
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("[brain] lattice-router: network build failed (non-fatal): {e}");
            return Vec::new();
        }
    };
    match network.forward(context) {
        Ok(weights) => weights.to_vec(),
        Err(e) => {
            tracing::warn!("[brain] lattice-router: forward failed (non-fatal): {e}");
            Vec::new()
        }
    }
}
