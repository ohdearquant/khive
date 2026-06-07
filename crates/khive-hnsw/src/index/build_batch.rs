//! Batch build for HNSW: sequential seed phase (sqrt(N) nodes) then parallel neighbor search.

use std::collections::HashSet;

use super::HnswIndex;
use crate::error::{validate_finite_vector, Result, RetrievalError};
use crate::node::HnswNode;
use crate::NodeId;
use rayon::prelude::*;

/// Pre-computed neighbor information for a node to be inserted.
///
/// Produced during the parallel search phase, consumed during the sequential merge.
struct PrecomputedInsert {
    id: NodeId,
    vector: Vec<f32>,
    level: usize,
    /// Neighbors per layer: (layer, Vec<(distance, internal_id)>).
    layer_candidates: Vec<(usize, Vec<(f32, usize)>)>,
}

impl HnswIndex {
    /// Build from a batch (seed sequential, then parallel search + sequential merge). Errors on bad dims.
    pub fn build_batch(&mut self, items: Vec<(NodeId, Vec<f32>)>) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }

        // Pre-scan for duplicates within the input batch before any mutation.
        // Duplicate IDs in items would corrupt id_to_internal (last writer wins,
        // but multiple internal nodes would exist for the same external ID).
        let mut seen_in_batch: HashSet<NodeId> = HashSet::with_capacity(items.len());
        for (id, vector) in &items {
            if vector.len() != self.config.dimensions {
                return Err(RetrievalError::DimensionMismatch {
                    expected: self.config.dimensions,
                    actual: vector.len(),
                });
            }
            validate_finite_vector(vector)?;
            // Check for duplicates against existing index
            if self.id_to_internal.contains_key(id) {
                return Err(RetrievalError::hnsw(format!(
                    "build_batch does not support updates: ID {id:?} already exists"
                )));
            }
            // Check for duplicates within the batch itself
            if !seen_in_batch.insert(*id) {
                return Err(RetrievalError::hnsw(format!(
                    "build_batch: duplicate ID {id:?} within the input batch"
                )));
            }
        }

        // Budget check for entire batch -- use checked arithmetic to avoid overflow.
        if let Some(limit) = self.config.memory_budget {
            let current = self.memory_usage();
            let cost_per_node = self.estimate_insert_cost();
            // cost_per_node * items.len() can overflow for very large batches.
            let total_cost = cost_per_node.saturating_mul(items.len());
            if current.saturating_add(total_cost) > limit {
                return Err(RetrievalError::budget_exceeded(current, total_cost, limit));
            }
        }

        let n = items.len();

        // For very small batches, fall back to sequential insertion
        if n <= 32 {
            for (id, vector) in items {
                self.insert(id, vector)?;
            }
            return Ok(());
        }

        // Phase 1: Sequential seed insertion of sqrt(N) nodes
        // These build the upper-layer graph structure
        let seed_count = ((n as f64).sqrt() as usize).max(1);
        let (seed_items, remaining_items) = items.split_at(seed_count);

        for (id, vector) in seed_items {
            self.insert(*id, vector.clone())?;
        }

        // Phase 2: Pre-generate levels for remaining nodes in deterministic RNG order.
        let mut pending: Vec<(NodeId, Vec<f32>, usize)> = Vec::with_capacity(remaining_items.len());
        for (id, vector) in remaining_items {
            let level = self.random_level();
            pending.push((*id, vector.clone(), level));
        }

        // Phase 3: Neighbor search
        // The graph is frozen during this phase -- only read-only search_layer is called.
        let entry_point = self.entry_point;
        let current_max_level = self.max_level;
        let config_ef = self.config.ef_construction;
        let index = &*self;

        let precomputed: Vec<PrecomputedInsert> = pending
            .into_par_iter()
            .map(|(id, vector, level)| {
                let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();

                // Navigate upper layers to find entry region
                let ep = match entry_point {
                    Some(ep) => ep,
                    None => {
                        // Should not happen after seed phase, but handle gracefully
                        return PrecomputedInsert {
                            id,
                            vector,
                            level,
                            layer_candidates: Vec::new(),
                        };
                    }
                };

                let mut current_nearest = vec![ep];

                // Search from top layer down to level + 1 (greedy, ef=1)
                for l in (level + 1..=current_max_level).rev() {
                    let nearest = index.search_layer(&vector, norm, &current_nearest, 1, l);
                    if !nearest.is_empty() {
                        current_nearest = vec![nearest[0].1];
                    }
                }

                // Search layers from min(level, max_level) down to 0
                let mut layer_candidates = Vec::new();
                for l in (0..=level.min(current_max_level)).rev() {
                    let candidates =
                        index.search_layer(&vector, norm, &current_nearest, config_ef, l);

                    if !candidates.is_empty() {
                        current_nearest = vec![candidates[0].1];
                    }

                    layer_candidates.push((l, candidates));
                }

                PrecomputedInsert {
                    id,
                    vector,
                    level,
                    layer_candidates,
                }
            })
            .collect();

        // Phase 4: Sequential merge -- insert nodes with pre-computed neighbors
        for pc in precomputed {
            self.insert_with_precomputed(pc)?;
        }

        Ok(())
    }

    /// Insert a node using pre-computed neighbor candidates; skips the search phase.
    fn insert_with_precomputed(&mut self, pc: PrecomputedInsert) -> Result<()> {
        let internal_id = self.nodes.len();

        // Select neighbors from pre-computed candidates
        let mut layer_neighbors: Vec<(usize, Vec<usize>)> = Vec::new();
        for (l, candidates) in &pc.layer_candidates {
            let m = if *l == 0 {
                self.config.m_max0
            } else {
                self.config.m
            };
            let neighbors = self.select_neighbors(candidates, m);
            layer_neighbors.push((*l, neighbors));
        }

        // Build the node with neighbor lists
        let mut node = HnswNode::new(pc.vector, pc.level);
        for (l, neighbors) in &layer_neighbors {
            while node.neighbors.len() <= *l {
                node.neighbors.push(Vec::new());
            }
            node.neighbors[*l] = neighbors.clone();
        }

        // Insert into storage (including quantized arena)
        self.quantized.push(&node.vector, node.norm);
        self.nodes.push(node);
        self.id_to_internal.insert(pc.id, internal_id);
        self.internal_to_id.push(pc.id);

        // Add bidirectional connections
        for (l, neighbors) in layer_neighbors {
            let m = if l == 0 {
                self.config.m_max0
            } else {
                self.config.m
            };
            for neighbor_id in neighbors {
                self.connect(neighbor_id, internal_id, l);
                // Shrink if over m (m is already m_max0 for layer 0, m for upper layers)
                self.shrink_connections(neighbor_id, l, m);
            }
        }

        // Update entry point if new node is at higher level
        if pc.level > self.max_level {
            self.entry_point = Some(internal_id);
            self.max_level = pc.level;
        }

        self.additions_since_rebuild += 1;
        Ok(())
    }
}
