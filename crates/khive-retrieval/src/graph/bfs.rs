//! BFS (Breadth-First Search) traversal over the graph legacy layer.

use std::collections::{HashSet, VecDeque};

use super::compat::{EntityRef, LinkStore, StorageContext};

use crate::error::Result;

use super::helpers::{get_edge_weight, get_neighbor_entity, get_neighbors, matches_link_type};
use super::types::{PathNode, TraversalOptions, MAX_TRAVERSAL_DEPTH, MAX_TRAVERSAL_RESULTS};

/// BFS traversal from `start`. Returns [`PathNode`]s in level order, start node first.
pub async fn bfs_traverse<S: LinkStore>(
    store: &S,
    ctx: &StorageContext,
    start: EntityRef,
    options: &TraversalOptions,
) -> Result<Vec<PathNode>> {
    let max_depth = options.max_depth.min(MAX_TRAVERSAL_DEPTH);
    let limit = options
        .limit
        .unwrap_or(MAX_TRAVERSAL_RESULTS)
        .min(MAX_TRAVERSAL_RESULTS);
    let min_weight = options.min_weight.unwrap_or(f64::NEG_INFINITY);

    // An explicit limit of 0 means "return nothing" — return immediately so
    // the start node is never pushed into results.
    if limit == 0 {
        return Ok(Vec::new());
    }

    // **PROOF CORRESPONDENCE**: `khive.Retrieval.Graph.visited_mono`
    // Visited set only grows (insert-only); never shrinks during traversal.
    // EntityRef implements Hash + Eq, enabling direct use as HashMap key.
    let mut visited: HashSet<EntityRef> = HashSet::new();
    let mut results: Vec<PathNode> = Vec::new();
    // Queue: (entity_ref, depth, path_weight)
    let mut queue: VecDeque<(EntityRef, usize, f64)> = VecDeque::new();

    // Start node
    visited.insert(start.clone());
    results.push(PathNode::start(start.clone()));
    queue.push_back((start, 0, 0.0));

    while let Some((current, depth, path_weight)) = queue.pop_front() {
        // Check depth limit
        if depth >= max_depth {
            continue;
        }

        // Check result limit
        if results.len() >= limit {
            break;
        }

        // Get neighbors based on direction
        let links = get_neighbors(store, ctx, &current, &options.direction).await?;

        for link in links {
            // Filter by link type
            if !matches_link_type(&link, &options.link_types) {
                continue;
            }

            // Get edge weight and filter.
            // Reject NaN/Inf: non-finite weights propagate into path_weight and
            // corrupt ranking. NaN comparisons are always false, so the
            // min_weight check alone would silently let NaN through.
            let edge_weight = get_edge_weight(&link);
            if !edge_weight.is_finite() || edge_weight < min_weight {
                continue;
            }

            // Determine neighbor entity based on direction.
            // Returns None when current is not a valid endpoint for this link
            // (e.g., a backend returned a link that doesn't involve current).
            let Some(neighbor) = get_neighbor_entity(&link, &current, &options.direction) else {
                continue;
            };

            // Skip if already visited (EntityRef implements Hash + Eq)
            if visited.contains(&neighbor) {
                continue;
            }

            // Mark as visited and add to results
            visited.insert(neighbor.clone());
            let new_weight = path_weight + edge_weight;

            let node = PathNode {
                entity_id: neighbor.clone(),
                depth: depth + 1,
                via_link: Some(link),
                path_weight: new_weight,
            };
            results.push(node);

            // Check limit after adding
            if results.len() >= limit {
                break;
            }

            // Add to queue for further exploration
            queue.push_back((neighbor, depth + 1, new_weight));
        }
    }

    Ok(results)
}
