//! Reciprocal rank fusion that carries a label with every fused id.
//!
//! [`fuse_labelled`] scores arms of `(id, label)` pairs the way [`reciprocal_rank_fusion`]
//! scores `(id, score)` pairs and returns each id's label next to its score. It is generic over
//! the id type and the label type, and it does scoring only: the caller supplies the rule that
//! combines two labels for one id, and the caller applies any bonus, filter and cut to the full
//! fused order the function returns.
//!
//! [`HitLabel`] is a label for search hits. [`combine_leg_first_appearance`] and
//! [`combine_best_ranked_evidence`] are the two label rules callers use today, kept apart
//! because they disagree on repeated ids and on which appearance supplies the signals.

use std::collections::HashMap;
use std::hash::Hash;

use khive_fusion::reciprocal_rank_fusion;
use khive_score::DeterministicScore;

use crate::hit::{SearchSignals, SearchSource};

/// What one appearance of a hit contributes to its fused hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HitLabel {
    /// Zero-based position of the appearance in its own list.
    pub rank: usize,
    /// Component scores carried by the appearance.
    pub signals: SearchSignals,
    /// The retrieval leg or legs the appearance came from.
    pub source: SearchSource,
    /// Title carried by the appearance.
    pub title: Option<String>,
    /// Excerpt carried by the appearance.
    pub snippet: Option<String>,
}

/// Fuse arms of `(id, label)` pairs with reciprocal rank fusion and return every id with its
/// score and label, ordered by descending score and then ascending id.
///
/// Scoring is [`reciprocal_rank_fusion`] with the caller's `k`: each arm votes once per id, at
/// the id's first position in that arm. Nothing is boosted, filtered or cut here, so the caller
/// applies any bonus, filter and limit to the returned order.
///
/// `combine(held, incoming)` is called for every appearance of an id after its first, in arm
/// order and then position order within an arm. `held` is the label accumulated so far and
/// `incoming` is the later appearance, so a repeat inside one arm reaches `combine` too.
pub fn fuse_labelled<Id, L, F>(
    arms: Vec<Vec<(Id, L)>>,
    k: usize,
    combine: F,
) -> Vec<(Id, DeterministicScore, L)>
where
    Id: Eq + Hash + Clone + Ord,
    F: Fn(L, L) -> L,
{
    // Reciprocal rank scoring reads only the ids and their positions.
    let scored_arms: Vec<Vec<(Id, DeterministicScore)>> = arms
        .iter()
        .map(|arm| {
            arm.iter()
                .map(|(id, _)| (id.clone(), DeterministicScore::ZERO))
                .collect()
        })
        .collect();
    let ranked = reciprocal_rank_fusion(scored_arms, k);

    let mut labels: HashMap<Id, L> = HashMap::new();
    for (id, label) in arms.into_iter().flatten() {
        let merged = match labels.remove(&id) {
            Some(held) => combine(held, label),
            None => label,
        };
        labels.insert(id, merged);
    }

    ranked
        .into_iter()
        .map(|(id, score)| {
            let label = labels.remove(&id).expect("every ranked id has a label");
            (id, score, label)
        })
        .collect()
}

/// Label rule for a text leg and a vector leg that each contribute once per id.
///
/// An appearance from a leg that has already contributed is ignored, so a repeated id inside one
/// leg keeps the labels of its first copy even when that copy carries none. An appearance from a
/// new leg widens the source to both legs, fills each missing signal from the new appearance and
/// takes its title and snippet only when none is held. The held rank is kept.
pub fn combine_leg_first_appearance(earlier: HitLabel, later: HitLabel) -> HitLabel {
    let joined = earlier.source.union(later.source);
    if joined == earlier.source {
        return earlier;
    }
    let (held, added) = (earlier.signals, later.signals);
    HitLabel {
        rank: earlier.rank,
        signals: SearchSignals {
            vector_similarity: held.vector_similarity.or(added.vector_similarity),
            keyword_score: held.keyword_score.or(added.keyword_score),
        },
        source: joined,
        title: earlier.title.or(later.title),
        snippet: earlier.snippet.or(later.snippet),
    }
}

/// Label rule for lists from several backends merged into one order.
///
/// Signals come from the best-ranked appearance, and the earlier appearance wins a tie. Every
/// later appearance still widens the source and supplies a title or snippet when none is held, so
/// a repeated id inside one list contributes its labels too.
pub fn combine_best_ranked_evidence(earlier: HitLabel, later: HitLabel) -> HitLabel {
    let (rank, signals) = if later.rank < earlier.rank {
        (later.rank, later.signals)
    } else {
        (earlier.rank, earlier.signals)
    };
    HitLabel {
        rank,
        signals,
        source: earlier.source.union(later.source),
        title: earlier.title.or(later.title),
        snippet: earlier.snippet.or(later.snippet),
    }
}
