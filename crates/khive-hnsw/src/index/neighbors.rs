//! Neighbor selection for HNSW index.

use super::HnswIndex;
use crate::distance::{compute_ordering_distance, OrderedF32};

impl HnswIndex {
    /// Select neighbors using the diversified heuristic; O(N log N) sort upfront avoids O(N²) repeated min-scan.
    pub(crate) fn select_neighbors(&self, candidates: &[(f32, usize)], m: usize) -> Vec<usize> {
        if candidates.len() <= m {
            return candidates.iter().map(|(_, id)| *id).collect();
        }

        // Sort candidates by distance (ascending), tie-break by external ID for determinism.
        // This replaces the per-iteration O(N) min_by scan with a single O(N log N) sort.
        let mut sorted: Vec<(f32, usize)> = candidates.to_vec();
        sorted.sort_by(|a, b| match OrderedF32(a.0).cmp(&OrderedF32(b.0)) {
            std::cmp::Ordering::Equal => self.external_id(a.1).cmp(&self.external_id(b.1)),
            other => other,
        });

        let mut selected: Vec<(f32, usize)> = Vec::with_capacity(m);

        // Iterate through sorted candidates in distance order, applying diversity check.
        for &(dist_to_query, candidate_id) in &sorted {
            if selected.len() >= m {
                break;
            }

            let candidate_node = &self.nodes[candidate_id];
            let candidate_vec = &candidate_node.vector;
            let candidate_norm = candidate_node.norm;

            // Check diversity: candidate is closer to query than to any selected neighbor
            let is_diverse = selected.iter().all(|(_, sel_id)| {
                let sel_node = &self.nodes[*sel_id];
                let dist_to_selected = compute_ordering_distance(
                    candidate_vec,
                    candidate_norm,
                    &sel_node.vector,
                    sel_node.norm,
                    self.config.metric,
                );
                dist_to_query <= dist_to_selected
            });

            if is_diverse || selected.is_empty() {
                selected.push((dist_to_query, candidate_id));
            }
        }

        // Fill with closest remaining if the diversity heuristic was too aggressive
        if selected.len() < m {
            for &(dist, id) in &sorted {
                if selected.len() >= m {
                    break;
                }
                if !selected.iter().any(|(_, sid)| *sid == id) {
                    selected.push((dist, id));
                }
            }
        }

        selected.into_iter().map(|(_, id)| id).collect()
    }
}
