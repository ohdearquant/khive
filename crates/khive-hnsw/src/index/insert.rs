//! Insert operations for HNSW index.

use crate::NodeId;
use rand::Rng;

use super::HnswIndex;
use crate::config::MAX_LEVEL;
use crate::distance::compute_ordering_distance;
use crate::error::{validate_finite_vector, Result, RetrievalError};
use crate::metrics::{self, MetricEvent, MetricValue};
use crate::node::HnswNode;

impl HnswIndex {
    /// Insert a vector into the index; updates in place if the ID already exists.
    /// Returns an error on dimension mismatch. Emits insert metrics when a sink is attached.
    pub fn insert(&mut self, id: NodeId, vector: Vec<f32>) -> Result<()> {
        let start = std::time::Instant::now();

        let result = self.insert_inner(id, vector);

        // Emit metrics regardless of success/failure
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_INSERT_DURATION_MS,
                value: MetricValue::Histogram(elapsed),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_INSERT_COUNT,
                value: MetricValue::Counter(1),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_INDEX_SIZE,
                value: MetricValue::Gauge(self.len_live() as f64),
                labels: vec![],
            },
        );

        result
    }

    /// Insert a batch of vectors; returns IDs that failed with their errors.
    pub fn insert_many(
        &mut self,
        items: impl IntoIterator<Item = (NodeId, Vec<f32>)>,
    ) -> Vec<(NodeId, crate::error::RetrievalError)> {
        let mut failures = Vec::new();
        for (id, vector) in items {
            if let Err(e) = self.insert(id, vector) {
                failures.push((id, e));
            }
        }
        failures
    }

    /// Inner insert logic (uninstrumented).
    pub(super) fn insert_inner(&mut self, id: NodeId, vector: Vec<f32>) -> Result<()> {
        if vector.len() != self.config.dimensions {
            return Err(RetrievalError::DimensionMismatch {
                expected: self.config.dimensions,
                actual: vector.len(),
            });
        }
        validate_finite_vector(&vector)?;

        // If updating an existing node that is tombstoned, perform delete + fresh insert
        // so that the new vector is properly reconnected in the graph. A simple
        // vector-only update on a tombstoned node leaves it unreachable (the
        // tombstone prevents graph traversal to it and the in-edges were not
        // added back).
        if let Some(&iid) = self.id_to_internal.get(&id) {
            let is_tombstoned = iid < self.tombstones.len() && self.tombstones[iid];
            if is_tombstoned {
                // Undo the tombstone so `delete` does not double-count it,
                // and so the fresh insert below sees the ID as gone.
                self.tombstones[iid] = false;
                self.tombstone_count -= 1;
                // Remove from the ID maps so insert_inner treats this as a new node.
                // The internal slot (iid) becomes a permanent hole (like any deleted node).
                self.id_to_internal.remove(&id);
                // internal_to_id still holds the old external ID at position iid;
                // leave it in place — the slot is effectively dead (tombstoned above).
                // Fall through to the fresh-insert path below.
            } else {
                // Live node update: just swap the vector. Graph edges remain valid
                // because neighbors were chosen by proximity; after an in-place
                // vector update the edges may be slightly stale but the node
                // stays reachable and participates in search.
                self.nodes[iid].update_vector(vector.clone());
                // Update quantized arena to stay in sync
                self.quantized.update(iid, &vector, self.nodes[iid].norm);
                return Ok(());
            }
        }

        // Budget check before allocating a new node
        if let Some(limit) = self.config.memory_budget {
            let current = self.memory_usage();
            let cost = self.estimate_insert_cost();
            if current + cost > limit {
                return Err(RetrievalError::budget_exceeded(current, cost, limit));
            }
        }

        let level = self.random_level();
        let node = HnswNode::new(vector.clone(), level);
        let query_norm = node.norm;

        // Assign internal ID = next index in vec
        let internal_id = self.nodes.len();

        // First node
        if self.nodes.is_empty() {
            self.quantized.push(&vector, node.norm);
            self.nodes.push(node);
            self.id_to_internal.insert(id, internal_id);
            self.internal_to_id.push(id);
            self.entry_point = Some(internal_id);
            self.max_level = level;
            self.additions_since_rebuild += 1;
            return Ok(());
        }

        let entry_point = self.entry_point.ok_or_else(|| {
            RetrievalError::hnsw("HNSW invariant violated: no entry point despite non-empty index")
        })?;
        let current_max_level = self.max_level;

        // Search from top layer down to level + 1
        let mut current_nearest = vec![entry_point];

        for l in (level + 1..=current_max_level).rev() {
            let nearest = self.search_layer(&vector, query_norm, &current_nearest, 1, l);
            if !nearest.is_empty() {
                current_nearest = vec![nearest[0].1];
            }
        }

        // Collect neighbors for all layers (using internal usize IDs)
        let mut layer_neighbors: Vec<(usize, Vec<usize>)> = Vec::new();

        for l in (0..=level.min(current_max_level)).rev() {
            let candidates = self.search_layer(
                &vector,
                query_norm,
                &current_nearest,
                self.config.ef_construction,
                l,
            );

            let m = if l == 0 {
                self.config.m_max0
            } else {
                self.config.m
            };
            let neighbors = self.select_neighbors(&candidates, m);

            if !candidates.is_empty() {
                current_nearest = vec![candidates[0].1];
            }

            layer_neighbors.push((l, neighbors));
        }

        // Insert node first (including quantized arena)
        let mut new_node = node;
        for (l, neighbors) in &layer_neighbors {
            while new_node.neighbors.len() <= *l {
                new_node.neighbors.push(Vec::new());
            }
            new_node.neighbors[*l] = neighbors.clone();
        }
        self.quantized.push(&new_node.vector, new_node.norm);
        self.nodes.push(new_node);
        self.id_to_internal.insert(id, internal_id);
        self.internal_to_id.push(id);

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
        if level > current_max_level {
            self.entry_point = Some(internal_id);
            self.max_level = level;
        }

        self.additions_since_rebuild += 1;
        Ok(())
    }

    /// Generate random level for a new node using exponential distribution.
    /// Uses seeded RNG when `config.seed` is set for reproducible builds.
    pub(super) fn random_level(&mut self) -> usize {
        let r: f64 = self.rng.gen::<f64>().max(f64::MIN_POSITIVE);
        let level = (-r.ln() * self.config.ml).floor() as usize;
        level.min(MAX_LEVEL)
    }

    /// Add bidirectional connection using internal IDs.
    pub(crate) fn connect(&mut self, from: usize, to: usize, layer: usize) {
        let node = &mut self.nodes[from];
        while node.neighbors.len() <= layer {
            node.neighbors.push(Vec::new());
        }
        if !node.neighbors[layer].contains(&to) {
            node.neighbors[layer].push(to);
        }
    }

    /// Shrink connections if over limit.
    pub(crate) fn shrink_connections(&mut self, id: usize, layer: usize, m: usize) {
        use crate::distance::OrderedF32;

        // Phase 1: Compute new neighbors (read only)
        let new_neighbors = {
            let node = &self.nodes[id];
            if layer >= node.neighbors.len() || node.neighbors[layer].len() <= m {
                return;
            }

            let node_vec = &node.vector;
            let node_norm = node.norm;
            let neighbor_ids = &node.neighbors[layer];

            let mut scored: Vec<(f32, usize)> = neighbor_ids
                .iter()
                .map(|&n_id| {
                    let n = &self.nodes[n_id];
                    (
                        compute_ordering_distance(
                            node_vec,
                            node_norm,
                            &n.vector,
                            n.norm,
                            self.config.metric,
                        ),
                        n_id,
                    )
                })
                .collect();

            // Sort by distance, then by external ID for deterministic neighbor selection
            scored.sort_by(|a, b| match OrderedF32(a.0).cmp(&OrderedF32(b.0)) {
                std::cmp::Ordering::Equal => self.external_id(a.1).cmp(&self.external_id(b.1)),
                other => other,
            });
            scored
                .into_iter()
                .take(m)
                .map(|(_, id)| id)
                .collect::<Vec<_>>()
        };

        // Phase 2: Mutate
        let node = &mut self.nodes[id];
        if layer < node.neighbors.len() {
            node.neighbors[layer] = new_neighbors;
        }
    }

    /// Sort a node's neighbor list by distance to the node.
    ///
    /// Available for batch operations like post-rebuild optimization.
    // REASON: Kept for future batch post-rebuild neighbor sorting; not yet
    // wired into the rebuild path but part of the planned graph optimization pass.
    #[allow(dead_code)]
    pub(super) fn sort_neighbors(&mut self, id: usize, layer: usize) {
        use crate::distance::OrderedF32;

        // Phase 1: Compute sorted order (read only)
        let sorted = {
            let node = &self.nodes[id];
            if layer >= node.neighbors.len() || node.neighbors[layer].is_empty() {
                return;
            }

            let node_vec = &node.vector;
            let node_norm = node.norm;

            let mut scored: Vec<(f32, usize)> = node.neighbors[layer]
                .iter()
                .map(|&n_id| {
                    let dist = {
                        let n = &self.nodes[n_id];
                        compute_ordering_distance(
                            node_vec,
                            node_norm,
                            &n.vector,
                            n.norm,
                            self.config.metric,
                        )
                    };
                    (dist, n_id)
                })
                .collect();

            scored.sort_by(|a, b| match OrderedF32(a.0).cmp(&OrderedF32(b.0)) {
                std::cmp::Ordering::Equal => self.external_id(a.1).cmp(&self.external_id(b.1)),
                other => other,
            });
            scored.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
        };

        // Phase 2: Mutate
        let node = &mut self.nodes[id];
        if layer < node.neighbors.len() {
            node.neighbors[layer] = sorted;
        }
    }
}
