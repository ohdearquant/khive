//! Vamana graph construction and greedy-search implementation.

use std::collections::HashSet;

use rand::prelude::*;
use rayon::prelude::*;

use crate::{
    config::VamanaConfig,
    distance::l2_squared,
    error::{Result, VamanaError},
};

const BUILD_BATCH_SIZE: usize = 1024;
const MEDOID_SAMPLE_K: usize = 1000;
const BUILD_SEED: u64 = 0x5641_4d41_4e41;

/// Output of a single greedy-search traversal over the Vamana graph.
#[derive(Debug, Clone, PartialEq)]
pub struct GreedySearchResult {
    /// Top-k neighbors sorted by distance (ascending), ties broken by node ID.
    pub results: Vec<(u32, f32)>,
    /// All nodes expanded during the traversal, in expansion order.
    pub expanded: Vec<(u32, f32)>,
}

/// Generation-based visited-node tracker for greedy search.
///
/// Avoids clearing a `Vec<bool>` on every query by incrementing a generation counter.
#[derive(Debug, Clone)]
pub struct VisitedSet {
    marks: Vec<u64>,
    generation: u64,
}

impl VisitedSet {
    /// Create a new `VisitedSet` with pre-allocated capacity for `capacity` nodes.
    pub fn new(capacity: usize) -> Self {
        Self {
            marks: vec![0; capacity],
            generation: 1,
        }
    }

    /// Reset the visited state for all nodes in O(1) by advancing the generation.
    #[inline]
    pub fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.marks.fill(0);
            self.generation = 1;
        }
    }

    /// Grow the internal buffer if `node` would be out of range.
    #[inline]
    pub fn ensure_capacity(&mut self, node: usize) {
        if node >= self.marks.len() {
            self.marks.resize(node + 1, 0);
        }
    }

    /// Mark `node` as visited if it has not been visited in this generation.
    ///
    /// Returns `true` on first visit, `false` on subsequent calls for the same node.
    #[inline]
    pub fn mark_if_new(&mut self, node: usize) -> bool {
        if node >= self.marks.len() {
            self.marks.resize(node + 1, 0);
        }
        if self.marks[node] == self.generation {
            false
        } else {
            self.marks[node] = self.generation;
            true
        }
    }

    /// Return `true` if `node` has been marked in the current generation.
    #[inline]
    pub fn is_marked(&self, node: usize) -> bool {
        node < self.marks.len() && self.marks[node] == self.generation
    }

    #[cfg(test)]
    pub(crate) fn with_generation(capacity: usize, generation: u64) -> Self {
        Self {
            marks: vec![0; capacity],
            generation,
        }
    }
}

/// The Vamana proximity graph over `u32` node IDs.
///
/// `reverse_adj[v]` holds every node `u` such that `adjacency[u]` contains `v`.
/// It is kept consistent with `adjacency` throughout build and any future mutation.
/// Required by the Wolverine 2-hop delete-repair algorithm (ADR-052 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VamanaGraph {
    adjacency: Vec<Vec<u32>>,
    /// In-neighbor index: `reverse_adj[v]` = `{ u | v ∈ adjacency[u] }`.
    reverse_adj: Vec<Vec<u32>>,
    medoid: u32,
}

impl VamanaGraph {
    /// Create an empty graph. Errors if `num_nodes == 0` or `medoid >= num_nodes`.
    ///
    /// `reverse_adj` is initialized with empty lists; callers must populate it after
    /// filling `adjacency` (via `rebuild_reverse_adj_from_adjacency` or incrementally).
    pub fn new(num_nodes: usize, medoid: u32) -> Result<Self> {
        if num_nodes == 0 {
            return Err(VamanaError::EmptyInput);
        }
        if medoid as usize >= num_nodes {
            return Err(VamanaError::invalid_format(format!(
                "medoid {} out of range for {num_nodes} nodes",
                medoid
            )));
        }
        Ok(Self {
            adjacency: vec![Vec::new(); num_nodes],
            reverse_adj: vec![Vec::new(); num_nodes],
            medoid,
        })
    }

    /// Rebuild `reverse_adj` from scratch by scanning the current `adjacency`.
    ///
    /// O(N × R) where N is node count and R is average out-degree. Called after
    /// `build()` completes, and after `load` / `from_snapshot` restores adjacency
    /// from disk (v1 format does not persist `reverse_adj`).
    pub(crate) fn rebuild_reverse_adj_from_adjacency(&mut self) {
        let n = self.adjacency.len();
        let mut rev: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (u, neighbors) in self.adjacency.iter().enumerate() {
            for &v in neighbors {
                rev[v as usize].push(u as u32);
            }
        }
        self.reverse_adj = rev;
    }

    /// Build a Vamana graph from `vectors` using the given `config`.
    pub fn build(vectors: &[f32], config: &VamanaConfig) -> Result<Self> {
        config.validate()?;
        let num_vectors = validate_vectors(vectors, config.dimensions)?;

        if num_vectors > u32::MAX as usize {
            return Err(VamanaError::TooManyVectors { count: num_vectors });
        }

        let medoid = select_medoid(vectors, config.dimensions, num_vectors)?;
        let mut adjacency = initial_random_adjacency(num_vectors, config.max_degree)?;

        let mut rng = StdRng::seed_from_u64(BUILD_SEED ^ 0x0101_0101_0101_0101);
        let mut order: Vec<u32> = (0..num_vectors as u32).collect();
        order.shuffle(&mut rng);

        for pass_alpha in [1.0f64, config.alpha] {
            for batch in order.chunks(BUILD_BATCH_SIZE) {
                let snapshot = adjacency.clone();

                let proposals: Vec<(u32, Vec<u32>)> = batch
                    .par_iter()
                    .map(|&node| {
                        let mut visited = VisitedSet::new(num_vectors);
                        let query = row(vectors, config.dimensions, node);
                        let search = greedy_search_inner(
                            vectors,
                            config.dimensions,
                            &snapshot,
                            query,
                            medoid,
                            config.max_degree,
                            config.search_list_size,
                            &mut visited,
                            None, // no tombstones during build
                        );

                        let mut candidates: Vec<u32> = search
                            .expanded
                            .iter()
                            .map(|(id, _)| *id)
                            .chain(search.results.iter().map(|(id, _)| *id))
                            .chain(snapshot[node as usize].iter().copied())
                            .collect();
                        sort_dedup_u32(&mut candidates);

                        let neighbors = robust_prune_inner(
                            vectors,
                            config.dimensions,
                            node,
                            candidates,
                            pass_alpha,
                            config.max_degree,
                        );

                        (node, neighbors)
                    })
                    .collect();

                for (node, neighbors) in &proposals {
                    adjacency[*node as usize] = neighbors.clone();
                }

                let mut backedges: Vec<Vec<u32>> = vec![Vec::new(); num_vectors];
                for (source, neighbors) in &proposals {
                    for &target in neighbors {
                        if target != *source {
                            backedges[target as usize].push(*source);
                        }
                    }
                }

                adjacency
                    .par_iter_mut()
                    .enumerate()
                    .for_each(|(target, neighbors)| {
                        if backedges[target].is_empty() {
                            return;
                        }
                        for &source in &backedges[target] {
                            if !neighbors.contains(&source) {
                                neighbors.push(source);
                            }
                        }
                        if neighbors.len() > config.max_degree {
                            let candidates = std::mem::take(neighbors);
                            *neighbors = robust_prune_inner(
                                vectors,
                                config.dimensions,
                                target as u32,
                                candidates,
                                pass_alpha,
                                config.max_degree,
                            );
                        }
                    });
            }
        }

        for list in &mut adjacency {
            sort_dedup_u32(list);
            list.truncate(config.max_degree);
        }

        let reverse_adj = build_reverse_adj(&adjacency);
        Ok(Self {
            adjacency,
            reverse_adj,
            medoid,
        })
    }

    /// Append a new node with no neighbors and return its `u32` ID.
    ///
    /// Both `adjacency` and `reverse_adj` are extended atomically so the index
    /// remains consistent after every call.
    pub fn add_node(&mut self) -> Result<u32> {
        let new_id = self.adjacency.len();
        if new_id >= u32::MAX as usize {
            return Err(VamanaError::TooManyVectors { count: new_id + 1 });
        }
        self.adjacency.push(Vec::new());
        self.reverse_adj.push(Vec::new());
        Ok(new_id as u32)
    }

    /// Return the number of nodes in this graph.
    pub fn node_count(&self) -> usize {
        self.adjacency.len()
    }

    /// Return the medoid node ID (start node for greedy search).
    pub fn medoid(&self) -> u32 {
        self.medoid
    }

    /// Return a slice of all adjacency lists, one per node.
    pub fn adjacency(&self) -> &[Vec<u32>] {
        &self.adjacency
    }

    pub(crate) fn adjacency_mut_for_load(&mut self) -> &mut Vec<Vec<u32>> {
        &mut self.adjacency
    }

    /// Directly restore `reverse_adj` from a previously serialized in-neighbor list.
    /// Used by the v2 fast-load path to avoid an O(N*R) rebuild from adjacency.
    pub(crate) fn restore_reverse_adj(&mut self, reverse_adj: Vec<Vec<u32>>) {
        self.reverse_adj = reverse_adj;
    }

    /// Mutable access to both adjacency and reverse_adj for Wolverine repair.
    pub(crate) fn adjacency_and_reverse_mut(&mut self) -> (&mut Vec<Vec<u32>>, &mut Vec<Vec<u32>>) {
        (&mut self.adjacency, &mut self.reverse_adj)
    }

    /// Replace `adjacency[node]` with `new_neighbors` and update `reverse_adj` in lockstep.
    ///
    /// For every node removed from the old list, `node` is removed from its `reverse_adj`
    /// entry. For every node added, `node` is appended. Called by Wolverine repair.
    pub(crate) fn replace_adjacency_and_update_reverse(
        &mut self,
        node: u32,
        new_neighbors: Vec<u32>,
    ) {
        let node_idx = node as usize;
        let old = std::mem::take(&mut self.adjacency[node_idx]);

        // Remove `node` from reverse_adj of nodes no longer in the list.
        for &v in &old {
            if !new_neighbors.contains(&v) {
                let rev = &mut self.reverse_adj[v as usize];
                if let Some(pos) = rev.iter().position(|&x| x == node) {
                    rev.swap_remove(pos);
                }
            }
        }

        // Add `node` to reverse_adj of newly added nodes.
        for &v in &new_neighbors {
            if !old.contains(&v) {
                self.reverse_adj[v as usize].push(node);
            }
        }

        self.adjacency[node_idx] = new_neighbors;
    }

    /// Set the medoid (entry point for greedy search).
    pub(crate) fn set_medoid(&mut self, medoid: u32) {
        self.medoid = medoid;
    }

    /// Return the neighbor list for `node`, or an error if out of range.
    pub fn neighbors(&self, node: u32) -> Result<&[u32]> {
        let idx = node as usize;
        if idx >= self.adjacency.len() {
            return Err(VamanaError::invalid_format(format!(
                "node {node} out of range"
            )));
        }
        Ok(&self.adjacency[idx])
    }

    /// Return the in-neighbors of `node`: every `u` such that `adjacency[u]` contains `node`.
    ///
    /// Required by the Wolverine 2-hop delete-repair algorithm (ADR-052 §2 step 2).
    /// Returns an error if `node` is out of range.
    pub fn in_neighbors(&self, node: u32) -> Result<&[u32]> {
        let idx = node as usize;
        if idx >= self.reverse_adj.len() {
            return Err(VamanaError::invalid_format(format!(
                "node {node} out of range for reverse_adj"
            )));
        }
        Ok(&self.reverse_adj[idx])
    }

    /// Return all in-neighbor lists, one per node.
    pub fn reverse_adjacency(&self) -> &[Vec<u32>] {
        &self.reverse_adj
    }

    /// Run greedy beam search from the medoid and return the `k` nearest candidates.
    ///
    /// `tombstones` is an optional bit-packed slice produced by `VamanaIndex`.
    /// Tombstoned nodes are skipped during beam expansion (defense-in-depth for
    /// the pre-PR4 window where the Wolverine invariant is not crash-safe).
    // REASON: `tombstones` was added to the existing 7-parameter signature (PR2);
    // bundling params into a struct would add allocation overhead on the hot path.
    #[allow(clippy::too_many_arguments)]
    pub fn greedy_search(
        &self,
        vectors: &[f32],
        dimensions: usize,
        query: &[f32],
        k: usize,
        search_list_size: usize,
        visited: &mut VisitedSet,
        tombstones: Option<&[u64]>,
    ) -> Result<GreedySearchResult> {
        if query.len() != dimensions {
            return Err(VamanaError::DimensionMismatch {
                expected: dimensions,
                actual: query.len(),
            });
        }
        if !vectors.len().is_multiple_of(dimensions) {
            return Err(VamanaError::DimensionMismatch {
                expected: dimensions,
                actual: vectors.len() % dimensions,
            });
        }
        validate_graph_vectors(vectors, dimensions, self.adjacency.len())?;
        if k == 0 {
            return Err(VamanaError::invalid_config("k must be > 0".into()));
        }
        if search_list_size == 0 {
            return Err(VamanaError::invalid_config(
                "search_list_size must be > 0".into(),
            ));
        }

        Ok(greedy_search_inner(
            vectors,
            dimensions,
            &self.adjacency,
            query,
            self.medoid,
            k,
            search_list_size,
            visited,
            tombstones,
        ))
    }

    /// Apply the DiskANN robust-prune heuristic to select at most `max_degree` neighbors for `node`.
    pub fn robust_prune(
        &self,
        vectors: &[f32],
        dimensions: usize,
        node: u32,
        candidates: &[u32],
        alpha: f64,
        max_degree: usize,
    ) -> Result<Vec<u32>> {
        if node as usize >= self.adjacency.len() {
            return Err(VamanaError::invalid_format(format!(
                "node {node} out of range"
            )));
        }
        if !vectors.len().is_multiple_of(dimensions) {
            return Err(VamanaError::DimensionMismatch {
                expected: dimensions,
                actual: vectors.len() % dimensions,
            });
        }
        validate_graph_vectors(vectors, dimensions, self.adjacency.len())?;
        if !alpha.is_finite() {
            return Err(VamanaError::invalid_config("alpha must be finite".into()));
        }
        if alpha < 1.0 {
            return Err(VamanaError::invalid_config("alpha must be >= 1.0".into()));
        }

        let mut all: Vec<u32> = candidates
            .iter()
            .copied()
            .chain(self.adjacency[node as usize].iter().copied())
            .collect();
        sort_dedup_u32(&mut all);

        Ok(robust_prune_inner(
            vectors, dimensions, node, all, alpha, max_degree,
        ))
    }
}

#[derive(Clone, Copy)]
struct Candidate {
    id: u32,
    distance: f32,
    expanded: bool,
}

// REASON: greedy_search_inner requires nine parameters to avoid bundling them into
// a struct that would add allocation overhead on the hot search path. The extra
// `tombstones` parameter is `None` during build (no tombstones exist) and
// `Some(&[u64])` during live search (defense-in-depth pre-PR4).
#[allow(clippy::too_many_arguments)]
pub(crate) fn greedy_search_inner(
    vectors: &[f32],
    dimensions: usize,
    adjacency: &[Vec<u32>],
    query: &[f32],
    start: u32,
    k: usize,
    search_list_size: usize,
    visited: &mut VisitedSet,
    tombstones: Option<&[u64]>,
) -> GreedySearchResult {
    let effective_l = search_list_size.max(k);
    visited.clear();

    // Defense-in-depth: if the medoid seed is tombstoned (possible in the window between
    // a crash-truncated medoid tombstone and PR4's crash-safe medoid update), skip seeding
    // it and return an empty result rather than surfacing a deleted node in results.
    if let Some(ts) = tombstones {
        if is_tombstoned_bit(ts, start as usize) {
            return GreedySearchResult {
                results: Vec::new(),
                expanded: Vec::new(),
            };
        }
    }

    let start_dist = l2_squared(query, row(vectors, dimensions, start));
    visited.mark_if_new(start as usize);

    let mut frontier = vec![Candidate {
        id: start,
        distance: start_dist,
        expanded: false,
    }];
    let mut expanded: Vec<(u32, f32)> = Vec::new();

    while let Some((best_idx, _)) = frontier
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.expanded)
        .min_by(|(_, a), (_, b)| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.id.cmp(&b.id))
        })
    {
        let current_id = frontier[best_idx].id;
        let current_dist = frontier[best_idx].distance;
        frontier[best_idx].expanded = true;
        expanded.push((current_id, current_dist));

        for &neighbor in &adjacency[current_id as usize] {
            // Defense-in-depth: skip tombstoned neighbors during live search.
            // Under a correct Wolverine repair no live forward edge should point
            // at a tombstoned node, but this guard catches crash-truncated repairs
            // before PR4 makes the invariant crash-safe.
            if let Some(ts) = tombstones {
                if is_tombstoned_bit(ts, neighbor as usize) {
                    continue;
                }
            }
            if !visited.mark_if_new(neighbor as usize) {
                continue;
            }
            let d = l2_squared(query, row(vectors, dimensions, neighbor));
            frontier.push(Candidate {
                id: neighbor,
                distance: d,
                expanded: false,
            });
        }

        frontier.sort_unstable_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.id.cmp(&b.id))
        });
        frontier.dedup_by_key(|c| c.id);
        if frontier.len() > effective_l {
            frontier.truncate(effective_l);
        }
    }

    frontier.sort_unstable_by(|a, b| {
        a.distance
            .total_cmp(&b.distance)
            .then_with(|| a.id.cmp(&b.id))
    });

    // Filter tombstoned nodes from the result set (defense-in-depth for the no-repair
    // control path and crash-truncated repair windows pre-PR4).
    let results = frontier
        .iter()
        .filter(|c| {
            tombstones
                .map(|ts| !is_tombstoned_bit(ts, c.id as usize))
                .unwrap_or(true)
        })
        .take(k)
        .map(|c| (c.id, c.distance))
        .collect();

    GreedySearchResult { results, expanded }
}

/// Test whether bit `idx` is set in a `Vec<u64>` tombstone bitvec.
#[inline]
pub(crate) fn is_tombstoned_bit(tombstones: &[u64], idx: usize) -> bool {
    let word = idx / 64;
    if word >= tombstones.len() {
        return false;
    }
    tombstones[word] & (1u64 << (idx % 64)) != 0
}

pub(crate) fn robust_prune_inner(
    vectors: &[f32],
    dimensions: usize,
    node: u32,
    candidates: Vec<u32>,
    alpha: f64,
    max_degree: usize,
) -> Vec<u32> {
    let node_vec = row(vectors, dimensions, node);
    let mut seen = HashSet::new();
    let mut pool: Vec<(u32, f32)> = Vec::new();

    for candidate in candidates {
        if candidate == node {
            continue;
        }
        if !seen.insert(candidate) {
            continue;
        }
        let d2 = l2_squared(node_vec, row(vectors, dimensions, candidate));
        pool.push((candidate, d2));
    }

    pool.sort_unstable_by(|(a_id, a_d), (b_id, b_d)| {
        a_d.total_cmp(b_d).then_with(|| a_id.cmp(b_id))
    });

    let alpha2 = (alpha * alpha) as f32;
    let mut selected: Vec<u32> = Vec::with_capacity(max_degree);

    'candidate: for (candidate_id, d2_node_candidate) in pool {
        if selected.len() == max_degree {
            break;
        }
        for &selected_id in &selected {
            let d2_selected_candidate = l2_squared(
                row(vectors, dimensions, selected_id),
                row(vectors, dimensions, candidate_id),
            );
            if alpha2 * d2_selected_candidate <= d2_node_candidate {
                continue 'candidate;
            }
        }
        selected.push(candidate_id);
    }

    selected
}

fn validate_vectors(vectors: &[f32], dimensions: usize) -> Result<usize> {
    if vectors.is_empty() {
        return Err(VamanaError::EmptyInput);
    }
    if !vectors.len().is_multiple_of(dimensions) {
        return Err(VamanaError::DimensionMismatch {
            expected: dimensions,
            actual: vectors.len() % dimensions,
        });
    }
    Ok(vectors.len() / dimensions)
}

/// Validate that `vectors` contains at least `graph_nodes` rows of `dimensions` floats.
/// Used to guard public-facing graph operations against out-of-bounds row accesses.
fn validate_graph_vectors(vectors: &[f32], dimensions: usize, graph_nodes: usize) -> Result<()> {
    if !vectors.len().is_multiple_of(dimensions) {
        return Err(VamanaError::DimensionMismatch {
            expected: dimensions,
            actual: vectors.len() % dimensions,
        });
    }
    let vector_count = vectors.len() / dimensions;
    if vector_count < graph_nodes {
        return Err(VamanaError::invalid_format(format!(
            "vectors has {vector_count} rows but graph has {graph_nodes} nodes"
        )));
    }
    Ok(())
}

pub(crate) fn row(vectors: &[f32], dimensions: usize, node: u32) -> &[f32] {
    let start = node as usize * dimensions;
    &vectors[start..start + dimensions]
}

fn select_medoid(vectors: &[f32], dimensions: usize, num_vectors: usize) -> Result<u32> {
    let mut rng = StdRng::seed_from_u64(BUILD_SEED);
    let sample_count = MEDOID_SAMPLE_K.min(num_vectors);

    let mut indices: Vec<usize> = (0..num_vectors).collect();
    indices.partial_shuffle(&mut rng, sample_count);
    let sample = &indices[..sample_count];

    let mut mean = vec![0.0f32; dimensions];
    for &idx in sample {
        let v = row(vectors, dimensions, idx as u32);
        for (m, x) in mean.iter_mut().zip(v.iter()) {
            *m += x;
        }
    }
    let scale = 1.0 / sample_count as f32;
    for m in &mut mean {
        *m *= scale;
    }

    let best_id = (0..num_vectors as u32)
        .into_par_iter()
        .map(|id| {
            let d = l2_squared(&mean, row(vectors, dimensions, id));
            (id, d)
        })
        .reduce(
            || (0u32, f32::INFINITY),
            |(best_id, best_d), (id, d)| {
                if d < best_d || (d == best_d && id < best_id) {
                    (id, d)
                } else {
                    (best_id, best_d)
                }
            },
        )
        .0;

    Ok(best_id)
}

fn initial_random_adjacency(num_vectors: usize, max_degree: usize) -> Result<Vec<Vec<u32>>> {
    if num_vectors == 0 {
        return Err(VamanaError::EmptyInput);
    }
    let mut rng = StdRng::seed_from_u64(BUILD_SEED);

    // Use a partial Fisher-Yates shuffle over a candidate pool that excludes
    // node `i` itself. This avoids the O(n²) worst-case of rejection sampling
    // when `num_vectors` is close to `max_degree`.
    //
    // Build one shared pool [0..num_vectors), then for each node swap out its
    // own index before sampling, and restore after. Because the function is
    // single-threaded (uses a seeded RNG), this is deterministic.
    let count = max_degree.min(num_vectors - 1);
    let mut pool: Vec<u32> = (0..num_vectors as u32).collect();

    let mut shuffle_swaps: Vec<(usize, usize)> = Vec::with_capacity(count);

    let adjacency: Vec<Vec<u32>> = (0..num_vectors)
        .map(|i| {
            // Move i out of the active range by swapping with the last element.
            let last = num_vectors - 1;
            pool.swap(i, last);

            // Partial Fisher-Yates over pool[0..last] (excludes i).
            // Record every swap so we can undo them and restore the shared pool.
            let available = last; // == num_vectors - 1
            let mut neighbors: Vec<u32> = Vec::with_capacity(count);
            shuffle_swaps.clear();
            for k in 0..count {
                let j = rng.gen_range(k..available);
                pool.swap(k, j);
                shuffle_swaps.push((k, j));
                neighbors.push(pool[k]);
            }

            // Undo the partial Fisher-Yates swaps in reverse order.
            for &(k, j) in shuffle_swaps.iter().rev() {
                pool.swap(k, j);
            }

            // Restore pool[i] so subsequent iterations see a full, intact pool.
            pool.swap(i, last);

            neighbors.sort_unstable();
            neighbors
        })
        .collect();

    Ok(adjacency)
}

pub(crate) fn sort_dedup_u32(values: &mut Vec<u32>) {
    values.sort_unstable();
    values.dedup();
}

/// Build a complete reverse-adjacency index from a forward adjacency list.
///
/// For each edge `u -> v` in `adjacency`, records `u` in `result[v]`.
/// O(N × R) where R is the average out-degree. Used at `build()` completion
/// and when reconstructing the index after a v1-format load (which does not
/// persist `reverse_adj`).
fn build_reverse_adj(adjacency: &[Vec<u32>]) -> Vec<Vec<u32>> {
    let n = adjacency.len();
    let mut rev: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (u, neighbors) in adjacency.iter().enumerate() {
        for &v in neighbors {
            rev[v as usize].push(u as u32);
        }
    }
    rev
}

// INLINE TEST JUSTIFICATION: Tests here require direct access to `adjacency` (private field)
// and internal helpers (`initial_random_adjacency`, `validate_graph_vectors`) that are not
// exposed in the public API. Moving them to `tests/` would require pub(crate) re-exports
// that would bloat the public surface. The graph.rs build logic is complex enough that
// keeping unit tests close to the code they cover outweighs the file-size cost.
#[cfg(test)]
mod tests {
    use super::*;

    fn make_line_vectors(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32 / n as f32).collect()
    }

    #[test]
    fn visited_set_marks_first_visit_only() {
        let mut vs = VisitedSet::new(10);
        assert!(vs.mark_if_new(3));
        assert!(!vs.mark_if_new(3));
    }

    #[test]
    fn visited_set_clear_advances_generation() {
        let mut vs = VisitedSet::new(10);
        vs.mark_if_new(5);
        vs.clear();
        assert!(vs.mark_if_new(5));
    }

    #[test]
    fn visited_set_resizes_for_large_node() {
        let mut vs = VisitedSet::new(4);
        assert!(vs.mark_if_new(100));
        assert!(!vs.mark_if_new(100));
    }

    #[test]
    fn visited_set_wraparound_resets_marks() {
        let mut vs = VisitedSet::with_generation(4, u64::MAX);
        vs.mark_if_new(0);
        vs.clear();
        assert!(vs.mark_if_new(0));
        assert!(!vs.mark_if_new(0));
    }

    #[test]
    fn new_rejects_empty_graph() {
        assert!(matches!(
            VamanaGraph::new(0, 0),
            Err(VamanaError::EmptyInput)
        ));
    }

    #[test]
    fn new_rejects_invalid_medoid() {
        assert!(matches!(
            VamanaGraph::new(5, 5),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    #[test]
    fn add_node_returns_u32_id() {
        let mut g = VamanaGraph::new(3, 0).unwrap();
        let id = g.add_node().unwrap();
        assert_eq!(id, 3);
        assert_eq!(g.node_count(), 4);
    }

    #[test]
    fn greedy_search_finds_nearest_on_line_graph() {
        // 5 points at 0.0, 0.2, 0.4, 0.6, 0.8 connected as a line
        let vectors: Vec<f32> = vec![0.0, 0.2, 0.4, 0.6, 0.8];
        let mut g = VamanaGraph::new(5, 2).unwrap();
        // Connect as chain: 0-1-2-3-4
        g.adjacency[0] = vec![1];
        g.adjacency[1] = vec![0, 2];
        g.adjacency[2] = vec![1, 3];
        g.adjacency[3] = vec![2, 4];
        g.adjacency[4] = vec![3];

        let query = [0.81f32];
        let mut visited = VisitedSet::new(5);
        let result = g
            .greedy_search(&vectors, 1, &query, 1, 5, &mut visited, None)
            .unwrap();

        assert_eq!(result.results[0].0, 4);
    }

    #[test]
    fn greedy_search_sorts_ties_by_node_id() {
        // Two points equidistant from query — expect lower id first
        let vectors: Vec<f32> = vec![0.0, 2.0];
        let mut g = VamanaGraph::new(2, 0).unwrap();
        g.adjacency[0] = vec![1];
        g.adjacency[1] = vec![0];

        let query = [1.0f32];
        let mut visited = VisitedSet::new(2);
        let result = g
            .greedy_search(&vectors, 1, &query, 2, 5, &mut visited, None)
            .unwrap();

        assert!(result.results.len() >= 2);
        assert_eq!(result.results[0].0, 0);
        assert_eq!(result.results[1].0, 1);
    }

    #[test]
    fn greedy_search_rejects_query_dimension_mismatch() {
        let vectors = vec![0.1f32, 0.2, 0.3, 0.4];
        let g = VamanaGraph::new(2, 0).unwrap();
        let mut visited = VisitedSet::new(2);
        let err = g.greedy_search(&vectors, 2, &[0.1f32], 1, 5, &mut visited, None);
        assert!(matches!(err, Err(VamanaError::DimensionMismatch { .. })));
    }

    #[test]
    fn robust_prune_enforces_degree_bound() {
        let n = 20usize;
        let dim = 2usize;
        let vectors: Vec<f32> = (0..n)
            .flat_map(|i| {
                let angle = i as f32 * std::f32::consts::TAU / n as f32;
                vec![angle.cos(), angle.sin()]
            })
            .collect();
        let g = VamanaGraph::new(n, 0).unwrap();
        let candidates: Vec<u32> = (1..n as u32).collect();
        let pruned = g
            .robust_prune(&vectors, dim, 0, &candidates, 1.2, 4)
            .unwrap();
        assert!(pruned.len() <= 4);
    }

    #[test]
    fn robust_prune_uses_diskann_alpha_squared_condition() {
        // Reproduce algorithm.md walkthrough with 2D coordinates
        // node v = (0,0), alpha = 1.2, R = 3
        let vectors: Vec<f32> = vec![
            0.0, 0.0, // node 0: v
            1.0, 0.0, // node 1: A, d(v,A)=1.0
            1.1, 0.1, // node 2: B, d(v,B)≈1.105
            0.0, 1.4, // node 3: C, d(v,C)=1.4
            2.0, 0.0, // node 4: D, d(v,D)=2.0
            -2.0, 1.9, // node 5: E, d(v,E)≈2.759
        ];
        let g = VamanaGraph::new(6, 0).unwrap();
        let candidates: Vec<u32> = vec![1, 2, 3, 4, 5];
        let pruned = g.robust_prune(&vectors, 2, 0, &candidates, 1.2, 3).unwrap();
        // Expected: [A(1), C(3)] — B, D, E get occluded
        assert!(pruned.contains(&1), "A should be selected: {pruned:?}");
        assert!(pruned.contains(&3), "C should be selected: {pruned:?}");
        assert!(!pruned.contains(&2), "B should be pruned: {pruned:?}");
        assert!(!pruned.contains(&4), "D should be pruned: {pruned:?}");
        assert!(!pruned.contains(&5), "E should be pruned: {pruned:?}");
    }

    #[test]
    fn robust_prune_removes_self_and_duplicates() {
        let vectors: Vec<f32> = vec![0.0, 0.5, 1.0];
        let g = VamanaGraph::new(3, 0).unwrap();
        let candidates: Vec<u32> = vec![0, 1, 1, 2];
        let pruned = g.robust_prune(&vectors, 1, 0, &candidates, 1.0, 4).unwrap();
        assert!(!pruned.contains(&0), "self-loop must be removed");
        // No duplicates
        let mut deduped = pruned.clone();
        deduped.dedup();
        assert_eq!(pruned.len(), deduped.len());
    }

    #[test]
    fn build_rejects_empty_vectors() {
        let cfg = VamanaConfig::default();
        assert!(matches!(
            VamanaGraph::build(&[], &cfg),
            Err(VamanaError::EmptyInput)
        ));
    }

    #[test]
    fn build_rejects_non_row_major_vectors() {
        let cfg = VamanaConfig::with_dimensions(3);
        let vectors = vec![0.1f32; 7]; // 7 not divisible by 3
        assert!(matches!(
            VamanaGraph::build(&vectors, &cfg),
            Err(VamanaError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn build_creates_bounded_degree_graph() {
        use rand::SeedableRng;
        let mut rng = StdRng::seed_from_u64(42);
        let n = 50usize;
        let dim = 8usize;
        let mut raw: Vec<f32> = (0..n * dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
        normalize_rows(&mut raw, dim);

        let cfg = VamanaConfig::with_dimensions(dim)
            .with_max_degree(8)
            .with_search_list_size(16);

        let g = VamanaGraph::build(&raw, &cfg).unwrap();
        for list in g.adjacency() {
            assert!(list.len() <= 8);
        }
    }

    #[test]
    fn build_is_deterministic_for_same_input() {
        use rand::SeedableRng;
        let mut rng = StdRng::seed_from_u64(77);
        let n = 30usize;
        let dim = 4usize;
        let mut raw: Vec<f32> = (0..n * dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
        normalize_rows(&mut raw, dim);

        let cfg = VamanaConfig::with_dimensions(dim)
            .with_max_degree(6)
            .with_search_list_size(12);

        let g1 = VamanaGraph::build(&raw, &cfg).unwrap();
        let g2 = VamanaGraph::build(&raw, &cfg).unwrap();
        assert_eq!(g1, g2);
    }

    fn normalize_rows(v: &mut [f32], dim: usize) {
        for row in v.chunks_mut(dim) {
            let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in row.iter_mut() {
                    *x /= norm;
                }
            }
        }
    }

    // REASON: test helper kept for future graph traversal tests that need a
    // simple chain topology with known distances.
    #[allow(dead_code)]
    fn make_line_graph_test(n: usize) -> (Vec<f32>, VamanaGraph) {
        let vectors = make_line_vectors(n);
        let mut g = VamanaGraph::new(n, 0).unwrap();
        for i in 0..n {
            if i > 0 {
                g.adjacency[i].push(i as u32 - 1);
            }
            if i + 1 < n {
                g.adjacency[i].push(i as u32 + 1);
            }
        }
        (vectors, g)
    }

    // ---- PR1: reverse_adj consistency tests ----

    /// After `build`, every forward edge u→v must be reflected in `reverse_adj[v]` as `u`.
    /// Equivalently: for every node v, `reverse_adj[v]` must contain exactly the set of nodes
    /// that have `v` in their forward adjacency list.
    #[test]
    fn build_reverse_adj_consistent_with_forward_adjacency() {
        use rand::SeedableRng;
        let mut rng = StdRng::seed_from_u64(0x00AD_C052);
        let n = 60usize;
        let dim = 4usize;
        let raw: Vec<f32> = (0..n * dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();

        let cfg = VamanaConfig::with_dimensions(dim)
            .with_max_degree(8)
            .with_search_list_size(16);

        let g = VamanaGraph::build(&raw, &cfg).unwrap();

        // For each node v, build the expected in-neighbor set by scanning forward adjacency.
        let expected_rev: Vec<std::collections::HashSet<u32>> = (0..n)
            .map(|v| {
                (0..n)
                    .filter(|&u| u != v && g.adjacency()[u].contains(&(v as u32)))
                    .map(|u| u as u32)
                    .collect()
            })
            .collect();

        for (v, expected) in expected_rev.iter().enumerate() {
            let actual: std::collections::HashSet<u32> =
                g.reverse_adjacency()[v].iter().copied().collect();
            assert_eq!(
                &actual, expected,
                "reverse_adj[{v}] inconsistent with forward adjacency after build: \
                 got {actual:?}, expected {expected:?}",
            );
        }
    }

    /// After `VamanaGraph::new` + manual adjacency edits (simulating a load path),
    /// calling `rebuild_reverse_adj_from_adjacency` must restore full consistency.
    #[test]
    fn rebuild_reverse_adj_restores_consistency_after_manual_adjacency_load() {
        // Build a small chain graph manually (as `load` paths do).
        let n = 5usize;
        let mut g = VamanaGraph::new(n, 0).unwrap();
        // Chain: 0→1, 1→0,2, 2→1,3, 3→2,4, 4→3
        g.adjacency[0] = vec![1];
        g.adjacency[1] = vec![0, 2];
        g.adjacency[2] = vec![1, 3];
        g.adjacency[3] = vec![2, 4];
        g.adjacency[4] = vec![3];

        // Before rebuild, reverse_adj is all-empty (from `new`).
        for v in 0..n {
            assert!(
                g.reverse_adj[v].is_empty(),
                "reverse_adj[{v}] should be empty before rebuild"
            );
        }

        g.rebuild_reverse_adj_from_adjacency();

        // Now verify consistency: every forward edge u→v must appear as v's in-neighbor.
        for u in 0..n {
            for &v in &g.adjacency[u] {
                assert!(
                    g.reverse_adj[v as usize].contains(&(u as u32)),
                    "after rebuild: forward edge {u}→{v} not reflected in reverse_adj[{v}]"
                );
            }
        }
        // And the reverse: every entry in reverse_adj[v] must be a forward neighbor pointing at v.
        for v in 0..n {
            for &u in &g.reverse_adj[v] {
                assert!(
                    g.adjacency[u as usize].contains(&(v as u32)),
                    "after rebuild: reverse_adj[{v}] contains {u} \
                     but adjacency[{u}] does not contain {v}"
                );
            }
        }
    }

    /// `in_neighbors` returns the correct set for a known graph.
    #[test]
    fn in_neighbors_matches_expected_for_small_graph() {
        // Graph: 0→1, 0→2, 1→2
        let n = 3usize;
        let mut g = VamanaGraph::new(n, 0).unwrap();
        g.adjacency[0] = vec![1, 2];
        g.adjacency[1] = vec![2];
        g.adjacency[2] = vec![];
        g.rebuild_reverse_adj_from_adjacency();

        // Node 0: no one points at 0
        let in0: std::collections::HashSet<u32> =
            g.in_neighbors(0).unwrap().iter().copied().collect();
        assert_eq!(in0, std::collections::HashSet::new());

        // Node 1: only 0 points at 1
        let in1: std::collections::HashSet<u32> =
            g.in_neighbors(1).unwrap().iter().copied().collect();
        assert_eq!(in1, std::collections::HashSet::from([0u32]));

        // Node 2: both 0 and 1 point at 2
        let in2: std::collections::HashSet<u32> =
            g.in_neighbors(2).unwrap().iter().copied().collect();
        assert_eq!(in2, std::collections::HashSet::from([0u32, 1u32]));
    }

    /// `in_neighbors` returns an error for out-of-range node IDs.
    #[test]
    fn in_neighbors_rejects_out_of_range_node() {
        let g = VamanaGraph::new(3, 0).unwrap();
        assert!(
            matches!(g.in_neighbors(99), Err(VamanaError::InvalidFormat { .. })),
            "in_neighbors must return InvalidFormat for out-of-range node"
        );
    }

    /// After `add_node`, reverse_adj grows in sync with adjacency so the
    /// graph remains consistent for any subsequent rebuild. The new node
    /// starts with no in-neighbors, and the bidirectional invariant holds.
    #[test]
    fn add_node_extends_reverse_adj_in_sync() {
        let mut g = VamanaGraph::new(2, 0).unwrap();
        assert_eq!(g.adjacency.len(), g.reverse_adj.len());
        g.add_node().unwrap();
        assert_eq!(
            g.adjacency.len(),
            g.reverse_adj.len(),
            "reverse_adj must grow with adjacency after add_node"
        );
        assert_eq!(g.node_count(), 3);

        // The freshly added node carries no forward or reverse edges yet.
        let new_id = g.node_count() as u32 - 1;
        assert!(
            g.in_neighbors(new_id).unwrap().is_empty(),
            "newly added node must have no in-neighbors"
        );
        assert!(
            g.reverse_adjacency()[new_id as usize].is_empty(),
            "reverse_adj for newly added node must be empty"
        );

        // Full bidirectional invariant still holds after the mutation.
        for (u, outs) in g.adjacency().iter().enumerate() {
            for &v in outs {
                assert!(
                    g.reverse_adjacency()[v as usize].contains(&(u as u32)),
                    "forward edge {u}→{v} missing from reverse_adj[{v}] after add_node"
                );
            }
        }
        for (v, ins) in g.reverse_adjacency().iter().enumerate() {
            for &u in ins {
                assert!(
                    g.adjacency()[u as usize].contains(&(v as u32)),
                    "reverse edge {v}←{u} missing from adjacency[{u}] after add_node"
                );
            }
        }
    }

    // ---- Regression tests for P0/P1 fixes ----

    /// P0: initial_random_adjacency must never produce self-loops.
    #[test]
    fn initial_random_adjacency_has_no_self_edges() {
        for num_vectors in [2usize, 5, 10, 100] {
            let max_degree = (num_vectors - 1).min(8);
            let adjacency = initial_random_adjacency(num_vectors, max_degree).unwrap();
            for (i, neighbors) in adjacency.iter().enumerate() {
                assert!(
                    !neighbors.contains(&(i as u32)),
                    "self-loop found at node {i} with num_vectors={num_vectors}"
                );
            }
        }
    }

    /// P0: initial_random_adjacency neighbors must all be distinct.
    #[test]
    fn initial_random_adjacency_neighbors_are_unique() {
        let adjacency = initial_random_adjacency(20, 8).unwrap();
        for (i, neighbors) in adjacency.iter().enumerate() {
            let mut sorted = neighbors.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                neighbors.len(),
                "duplicate neighbors at node {i}"
            );
        }
    }

    /// P0: greedy_search rejects vectors with fewer rows than graph node count.
    #[test]
    fn greedy_search_rejects_graph_vector_count_mismatch() {
        // Graph has 3 nodes but vectors only has 2 rows (dim=2).
        let vectors = vec![0.0f32, 1.0, 2.0, 3.0]; // 2 rows × 2 dims
        let g = VamanaGraph::new(3, 0).unwrap();
        let mut visited = VisitedSet::new(3);
        let query = [0.5f32, 0.5];
        let err = g.greedy_search(&vectors, 2, &query, 1, 5, &mut visited, None);
        assert!(
            matches!(err, Err(VamanaError::InvalidFormat { .. })),
            "expected InvalidFormat, got {err:?}"
        );
    }

    /// P1: robust_prune must reject NaN alpha.
    #[test]
    fn robust_prune_rejects_nan_alpha() {
        let vectors: Vec<f32> = vec![0.0, 0.5, 1.0];
        let g = VamanaGraph::new(3, 0).unwrap();
        let candidates: Vec<u32> = vec![1, 2];
        let err = g.robust_prune(&vectors, 1, 0, &candidates, f64::NAN, 4);
        assert!(
            matches!(err, Err(VamanaError::InvalidConfig { .. })),
            "expected InvalidConfig for NaN alpha, got {err:?}"
        );
    }

    /// P1: robust_prune must reject alpha < 1.0.
    #[test]
    fn robust_prune_rejects_alpha_below_one() {
        let vectors: Vec<f32> = vec![0.0, 0.5, 1.0];
        let g = VamanaGraph::new(3, 0).unwrap();
        let candidates: Vec<u32> = vec![1, 2];
        let err = g.robust_prune(&vectors, 1, 0, &candidates, 0.5, 4);
        assert!(
            matches!(err, Err(VamanaError::InvalidConfig { .. })),
            "expected InvalidConfig for alpha < 1.0, got {err:?}"
        );
    }

    /// P1: robust_prune rejects vectors with fewer rows than graph node count.
    #[test]
    fn robust_prune_rejects_graph_vector_count_mismatch() {
        // Graph has 4 nodes but only 2 rows supplied (dim=2 → 2 rows).
        let vectors = vec![0.0f32, 0.5, 1.0, 1.5]; // 4 scalars × dim=2 → 2 rows
        let g = VamanaGraph::new(4, 0).unwrap();
        let candidates: Vec<u32> = vec![1, 2];
        let err = g.robust_prune(&vectors, 2, 0, &candidates, 1.2, 4);
        assert!(
            matches!(err, Err(VamanaError::InvalidFormat { .. })),
            "expected InvalidFormat, got {err:?}"
        );
    }
}
