//! Pure per-engine similarity calibration before rank fusion.

use std::collections::BTreeSet;

use crate::{Result, RetrievalError};

/// One distinct candidate retained by [`calibrate_engine_hits`].
#[derive(Clone, Debug, PartialEq)]
pub struct CalibratedHit<Id> {
    /// Caller-supplied candidate identity.
    pub id: Id,
    /// Original similarity, preserved without fixed-point conversion or normalization.
    pub raw_similarity: f64,
    /// Similarity normalized to `[0, 1]` by the supplied calibration bounds.
    pub normalized_similarity: f64,
    /// Consecutive one-based rank after filtering and identity deduplication.
    pub rank: usize,
}

/// Calibrate one bounded engine candidate list without activating a retrieval path.
///
/// Requires finite `-1 <= noise_floor < max_similarity <= 1` and a finite threshold
/// in `[0, 1]`. Invalid parameters return [`RetrievalError::Configuration`], even
/// for empty input. Every raw similarity must be finite; otherwise the whole call
/// returns [`RetrievalError::InvalidQuery`] naming its zero-based input position.
/// Parameter validation precedes raw-input validation, which precedes filtering.
///
/// Scores below the floor are discarded. Remaining scores are normalized as
/// `((score - noise_floor) / (max_similarity - noise_floor)).clamp(0.0, 1.0)`;
/// only values at or above the threshold survive. Retained hits sort by descending
/// raw similarity, then ascending ID, so normalization caps cannot reorder them.
/// Signed zeros tie numerically. Duplicate IDs retain their highest raw similarity;
/// an exact numeric score/ID tie retains its first input occurrence, including its
/// raw score bits. Ranks are compacted over the retained distinct IDs.
///
/// This is an additive foundation for proposed ADR-031 Amendment 5, not handler or
/// configuration activation. Callers must establish the adapter's cosine/similarity
/// contract and bound the input list. Cosine defaults are `(-1.0, 1.0, 0.0)`.
pub fn calibrate_engine_hits<Id: Ord + Clone>(
    mut hits: Vec<(Id, f64)>,
    noise_floor: f64,
    max_similarity: f64,
    threshold: f64,
) -> Result<Vec<CalibratedHit<Id>>> {
    if !noise_floor.is_finite()
        || !max_similarity.is_finite()
        || noise_floor < -1.0
        || noise_floor >= max_similarity
        || max_similarity > 1.0
    {
        return Err(RetrievalError::configuration(
            "calibration requires finite -1 <= noise_floor < max_similarity <= 1",
        ));
    }
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err(RetrievalError::configuration(
            "calibration threshold must be finite and in [0, 1]",
        ));
    }
    for (position, (_, score)) in hits.iter().enumerate() {
        if !score.is_finite() {
            return Err(RetrievalError::invalid_query(format!(
                "calibration raw similarity at input position {position} must be finite"
            )));
        }
    }

    hits.sort_by(|(a_id, a_score), (b_id, b_score)| {
        b_score
            .partial_cmp(a_score)
            .expect("finite raw similarities were validated")
            .then_with(|| a_id.cmp(b_id))
    });
    let mut seen = BTreeSet::new();
    let mut retained = Vec::new();
    for (id, raw_similarity) in hits {
        if raw_similarity < noise_floor {
            continue;
        }
        let normalized_similarity =
            ((raw_similarity - noise_floor) / (max_similarity - noise_floor)).clamp(0.0, 1.0);
        if normalized_similarity < threshold || !seen.insert(id.clone()) {
            continue;
        }
        retained.push(CalibratedHit {
            id,
            raw_similarity,
            normalized_similarity,
            rank: retained.len() + 1,
        });
    }
    Ok(retained)
}
