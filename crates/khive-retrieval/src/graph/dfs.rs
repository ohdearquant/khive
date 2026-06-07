//! DFS (Depth-First Search) traversal over the graph legacy layer.

use std::collections::HashSet;

use super::compat::{EntityRef, Link, LinkStore, StorageContext};

use crate::error::Result;

use super::helpers::{get_edge_weight, get_neighbor_entity, get_neighbors, matches_link_type};
use super::types::{PathNode, TraversalOptions, MAX_TRAVERSAL_DEPTH, MAX_TRAVERSAL_RESULTS};

/// DFS traversal from `start`. Returns [`PathNode`]s in pre-order (parent before children).
pub async fn dfs_traverse<S: LinkStore>(
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

    // `discovered` prevents pushing the same unvisited node multiple times onto
    // the stack.  Without it, a high fan-in DAG can push the same node once per
    // parent, leading to O(parents) redundant stack entries and unnecessary link
    // clones.  `visited` handles the pop-side skip, but `discovered` avoids the
    // wasted stack space and clone cost before the node is ever popped.
    let mut discovered: HashSet<EntityRef> = HashSet::new();

    // Stack: (entity_ref, depth, path_weight, via_link)
    let mut stack: Vec<(EntityRef, usize, f64, Option<Link>)> = Vec::new();
    discovered.insert(start.clone());
    stack.push((start, 0, 0.0, None));

    while let Some((current, depth, path_weight, via_link)) = stack.pop() {
        // Skip if already visited (EntityRef implements Hash + Eq)
        if visited.contains(&current) {
            continue;
        }

        // Mark as visited and add to results
        visited.insert(current.clone());
        results.push(PathNode {
            entity_id: current.clone(),
            depth,
            via_link,
            path_weight,
        });

        // Check result limit
        if results.len() >= limit {
            break;
        }

        // Check depth limit before exploring children
        if depth >= max_depth {
            continue;
        }

        // Get neighbors and push to stack (reverse order for consistent traversal)
        let links = get_neighbors(store, ctx, &current, &options.direction).await?;

        // Push in reverse order so first neighbor is processed first
        for link in links.into_iter().rev() {
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

            // Determine neighbor entity.
            // Returns None when current is not a valid endpoint for this link
            // (e.g., a backend returned a link that doesn't involve current).
            let Some(neighbor) = get_neighbor_entity(&link, &current, &options.direction) else {
                continue;
            };

            // Skip if already discovered or visited.
            // `discovered` prevents the same unvisited node from being pushed
            // multiple times by different parents (high fan-in DAG protection).
            if visited.contains(&neighbor) || discovered.contains(&neighbor) {
                continue;
            }
            discovered.insert(neighbor.clone());

            let new_weight = path_weight + edge_weight;
            stack.push((neighbor, depth + 1, new_weight, Some(link)));
        }
    }

    Ok(results)
}
