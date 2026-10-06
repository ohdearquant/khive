#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::{
    distance::l2_squared,
    error::{Result, VamanaError},
    graph::{is_tombstoned_bit, robust_prune_inner, sort_dedup_u32, VamanaGraph},
};

#[cfg(feature = "mmap")]
use super::VamanaIndex;

// ---- PR2: tombstone bit helpers (Vec<u64> bitvec, no external crate) ----

/// Number of `u64` words needed for `n` bits.
pub(super) fn tombstone_words_for(n: usize) -> Vec<u64> {
    let words = n.div_ceil(64);
    vec![0u64; words]
}

#[inline]
pub(super) fn set_tombstone_bit(tombstones: &mut Vec<u64>, idx: usize) {
    let word = idx / 64;
    if word >= tombstones.len() {
        tombstones.resize(word + 1, 0);
    }
    tombstones[word] |= 1u64 << (idx % 64);
}

// ---- PR2: Wolverine 2-hop repair (ADR-052 §2 steps 3-8) ----

/// Core Wolverine repair: rewire each live in-neighbor of `deleted` to bypass it,
/// updating `reverse_adj` in lockstep. See
/// crates/khive-vamana/docs/api/algorithm.md#wolverine-2-hop-repair for the RobustPrune
/// derivation and paper references.
pub(super) fn wolverine_repair(
    vectors: &[f32],
    dimensions: usize,
    graph: &mut VamanaGraph,
    deleted: u32,
    tombstones: &[u64],
    alpha: f64,
    max_degree: usize,
) {
    // Collect in-neighbors and out-neighbors before any mutation.
    let in_neighbors: Vec<u32> = graph.reverse_adjacency()[deleted as usize]
        .iter()
        .copied()
        .filter(|&p| !is_tombstoned_bit(tombstones, p as usize))
        .collect();

    let out_neighbors: Vec<u32> = graph.adjacency()[deleted as usize]
        .iter()
        .copied()
        .filter(|&v| !is_tombstoned_bit(tombstones, v as usize))
        .collect();

    for p in in_neighbors {
        // Build candidate pool: out(deleted) ∪ (adj(p) \ {deleted}), drop tombstoned.
        let mut pool: Vec<u32> = out_neighbors
            .iter()
            .copied()
            .chain(
                graph.adjacency()[p as usize]
                    .iter()
                    .copied()
                    .filter(|&v| v != deleted),
            )
            .filter(|&v| !is_tombstoned_bit(tombstones, v as usize) && v != p)
            .collect();
        sort_dedup_u32(&mut pool);

        // Use exact f32 distances for repair: the SQ8 codec is trained on the
        // build corpus and may be stale for vectors inserted after training.
        let new_neighbors = robust_prune_inner(vectors, dimensions, p, pool, alpha, max_degree);

        // Replace adjacency[p] and update reverse_adj in lockstep (PR1 invariant).
        graph.replace_adjacency_and_update_reverse(p, new_neighbors);
    }

    // Remove `deleted` from its own reverse_adj entry of every out-neighbor
    // (the deleted node's forward edges are now dead; reverse_adj must reflect this).
    for v in graph.adjacency()[deleted as usize].clone() {
        let rev = graph.adjacency_and_reverse_mut().1;
        if let Some(pos) = rev[v as usize].iter().position(|&x| x == deleted) {
            rev[v as usize].swap_remove(pos);
        }
    }

    // Clear the deleted node's own adjacency list so it has no live forward edges.
    graph.replace_adjacency_and_update_reverse(deleted, Vec::new());
}

/// Elect a new medoid: centroid of all live (non-tombstoned) vectors, nearest live node.
pub(super) fn elect_medoid(
    vectors: &[f32],
    dimensions: usize,
    num_vectors: usize,
    tombstones: &[u64],
) -> Result<u32> {
    // Compute mean of live vectors.
    let mut centroid = vec![0.0f32; dimensions];
    let mut live_count = 0usize;
    for i in 0..num_vectors {
        if !is_tombstoned_bit(tombstones, i) {
            let v = &vectors[i * dimensions..(i + 1) * dimensions];
            for (c, x) in centroid.iter_mut().zip(v.iter()) {
                *c += x;
            }
            live_count += 1;
        }
    }
    if live_count == 0 {
        return Err(VamanaError::EmptyInput);
    }
    let scale = 1.0 / live_count as f32;
    for c in &mut centroid {
        *c *= scale;
    }

    // Find live node nearest the centroid.
    let mut best_id = u32::MAX;
    let mut best_dist = f32::INFINITY;
    for i in 0..num_vectors {
        if is_tombstoned_bit(tombstones, i) {
            continue;
        }
        let v = &vectors[i * dimensions..(i + 1) * dimensions];
        let d = l2_squared(&centroid, v);
        if d < best_dist || (d == best_dist && (i as u32) < best_id) {
            best_dist = d;
            best_id = i as u32;
        }
    }
    Ok(best_id)
}

pub(super) fn exact_search(
    vectors: &[f32],
    dimensions: usize,
    query: &[f32],
    k: usize,
    tombstones: Option<&[u64]>,
) -> Vec<(u32, f32)> {
    let n = vectors.len() / dimensions;
    #[cfg(feature = "parallel")]
    let ids = (0..n as u32).into_par_iter();
    #[cfg(not(feature = "parallel"))]
    let ids = 0..n as u32;
    let mut dists: Vec<(u32, f32)> = ids
        .filter(|&id| {
            tombstones
                .map(|ts| !is_tombstoned_bit(ts, id as usize))
                .unwrap_or(true)
        })
        .map(|id| {
            let v = &vectors[id as usize * dimensions..(id as usize + 1) * dimensions];
            (id, l2_squared(query, v))
        })
        .collect();

    // Use select_nth_unstable_by to find the k-th element in O(N) rather than
    // full-sorting in O(N log N). Only the top-k prefix needs to be sorted.
    let effective_k = k.min(dists.len());
    if effective_k == 0 {
        return Vec::new();
    }
    if effective_k < dists.len() {
        dists.select_nth_unstable_by(effective_k - 1, |(a_id, a_d), (b_id, b_d)| {
            a_d.total_cmp(b_d).then_with(|| a_id.cmp(b_id))
        });
    }
    dists.truncate(effective_k);
    dists.sort_unstable_by(|(a_id, a_d), (b_id, b_d)| {
        a_d.total_cmp(b_d).then_with(|| a_id.cmp(b_id))
    });
    dists
}

#[cfg(feature = "mmap")]
pub(super) fn capped_reverse_adjacency(index: &VamanaIndex) -> Vec<Vec<u32>> {
    let adjacency = index.graph.adjacency();
    let medoid = index.graph.medoid() as usize;
    let mut reverse_adj = vec![Vec::new(); adjacency.len()];
    for (source, neighbors) in adjacency.iter().enumerate() {
        let neighbors = if source == medoid {
            &neighbors[..index.config.max_degree.min(neighbors.len())]
        } else {
            neighbors
        };
        for &target in neighbors {
            reverse_adj[target as usize].push(source as u32);
        }
    }
    reverse_adj
}

pub(super) fn validate_reverse_adjacency(
    graph: &VamanaGraph,
    reverse_adj: &[Vec<u32>],
) -> Result<()> {
    let mut expected = vec![Vec::new(); graph.node_count()];
    for (source, neighbors) in graph.adjacency().iter().enumerate() {
        for &target in neighbors {
            expected[target as usize].push(source as u32);
        }
    }
    for neighbors in &mut expected {
        neighbors.sort_unstable();
    }
    for (node, (expected, actual)) in expected.iter().zip(reverse_adj).enumerate() {
        let mut actual = actual.clone();
        actual.sort_unstable();
        if *expected != actual {
            return Err(VamanaError::invalid_format(format!(
                "lifecycle.bin reverse_adj[{node}] is not the inverse of graph.bin forward adjacency"
            )));
        }
    }
    Ok(())
}
