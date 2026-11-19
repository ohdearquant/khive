//! Union fusion (max score per ID).

use khive_score::DeterministicScore;
use std::collections::HashMap;
use std::hash::Hash;

use crate::ordering::cmp_desc_then_id;

/// Return every ID at its maximum source score, ordered by score then ID.
///
/// See `crates/khive-fusion/docs/api/fusion-functions.md`.
pub fn union_fusion<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
) -> Vec<(Id, DeterministicScore)> {
    if sources.is_empty() {
        return Vec::new();
    }

    let estimated_capacity: usize = sources.iter().map(|s| s.len()).sum();
    let mut combined: HashMap<Id, DeterministicScore> = HashMap::with_capacity(estimated_capacity);

    for results in sources {
        for (id, score) in results {
            combined
                .entry(id)
                .and_modify(|existing| {
                    if score > *existing {
                        *existing = score;
                    }
                })
                .or_insert(score);
        }
    }

    let mut fused: Vec<(Id, DeterministicScore)> = combined.into_iter().collect();
    fused.sort_by(cmp_desc_then_id);
    fused
}

#[cfg(test)]
#[path = "union_tests.rs"]
mod tests;
