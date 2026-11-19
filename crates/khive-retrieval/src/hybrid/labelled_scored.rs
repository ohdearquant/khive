//! Labelled fusion for arms that carry a score as well as a label.
//!
//! [`fuse_labelled_scored`] fuses arms of `(id, score, label)` rows under whichever strategy a
//! [`HybridConfig`] names. Scoring is [`fuse_search_results`] on the arms with their labels
//! removed, so every strategy keeps the behaviour it has for plain scored arms, and the function
//! only attaches to each returned id the label its appearances combine to. `fuse_labelled` is the
//! reciprocal rank form for arms whose scores are not read.

use std::collections::HashMap;
use std::hash::Hash;

use khive_fusion::FuseError;
use khive_score::DeterministicScore;

use super::config::HybridConfig;
use super::searcher::fuse_search_results;

/// Fuse arms of `(id, score, label)` rows and return each surviving id with the score and the
/// label it ends with.
///
/// Scoring is [`fuse_search_results`] on the arms with their labels removed. The weighted
/// strategy's two-source check, the fallback of a custom strategy to reciprocal rank, the `top_k`
/// cut and `min_score` therefore behave exactly as they do there, and the ids, order and scores
/// returned are the ones it returns. Keep an empty arm in its slot: the weighted strategy reads
/// the position of each arm. Weighted-RRF errors propagate before labels are combined.
///
/// `combine(held, incoming)` is called for every appearance of an id after its first, in arm order
/// and then position order within an arm. That covers every arm, including one the strategy does
/// not score, so a vector only fusion still folds in the labels of the keyword arm. Labels attach
/// after the cut: an id that scoring drops is not returned, and its label is not used.
///
/// A pass-through strategy (vector only, keyword only) returns a repeated id of the selected arm
/// once per copy, as [`fuse_search_results`] does. Each copy carries the same combined label,
/// which is why the label type is `Clone`.
pub fn fuse_labelled_scored<Id, L, F>(
    arms: Vec<Vec<(Id, DeterministicScore, L)>>,
    config: &HybridConfig,
    combine: F,
) -> Result<Vec<(Id, DeterministicScore, L)>, FuseError>
where
    Id: Eq + Hash + Clone + Ord,
    L: Clone,
    F: Fn(L, L) -> L,
{
    let mut scored_arms = Vec::with_capacity(arms.len());
    for arm in &arms {
        let mut scored = Vec::with_capacity(arm.len());
        for (id, score, _) in arm {
            scored.push((id.clone(), *score));
        }
        scored_arms.push(scored);
    }
    let ranked = fuse_search_results(scored_arms, config)?;

    let mut labels: HashMap<Id, L> = HashMap::new();
    for (id, _, label) in arms.into_iter().flatten() {
        let merged = match labels.remove(&id) {
            Some(held) => combine(held, label),
            None => label,
        };
        labels.insert(id, merged);
    }

    Ok(ranked
        .into_iter()
        .map(|(id, score)| {
            let label = labels.get(&id).expect("every scored id has a label");
            (id, score, label.clone())
        })
        .collect())
}
