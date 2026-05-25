//! BFS (Breadth-First Search) traversal.
//!
//! # Formal Verification
//!
//! This implementation corresponds to the formal proofs in
//! `proofs/Lion/Retrieval/Graph.lean`. Key theorems:
//!
//! - `bfs_terminates`: BFS always terminates (queue eventually empty)
//! - `bfs_complete`: all reachable vertices are visited
//! - `visited_mono`: visited set grows monotonically
//! - `reachable_trans`: reachability is transitive

use std::collections::{HashSet, VecDeque};

use super::compat::{EntityRef, LinkStore, StorageContext};

use crate::error::Result;

use super::helpers::{get_edge_weight, get_neighbor_entity, get_neighbors, matches_link_type};
use super::types::{PathNode, TraversalOptions, MAX_TRAVERSAL_DEPTH, MAX_TRAVERSAL_RESULTS};

/// Perform BFS traversal from a starting entity.
///
/// BFS explores nodes level by level, guaranteeing that nodes at depth N
/// are visited before nodes at depth N+1. This makes it ideal for:
///
/// - Finding all entities within N hops
/// - Social network expansion (friends of friends)
/// - Entity neighborhood exploration
///
/// # Arguments
///
/// * `store` - The link store to query
/// * `ctx` - Storage context for namespace isolation
/// * `start` - Starting entity reference
/// * `options` - Traversal options (depth, direction, filters)
///
/// # Returns
///
/// Vector of [`PathNode`] in BFS order. The first element is always the start node.
///
/// # Complexity
///
/// - Time: O(V + E) where V = vertices, E = edges
/// - Space: O(V) for visited set and queue
///
/// # Example
///
/// ```ignore
/// let options = TraversalOptions::new(3)
///     .with_direction(Direction::Out)
///     .with_link_types(["KNOWS"]);
///
/// let nodes = bfs_traverse(&store, &ctx, start_ref, &options).await?;
/// for node in &nodes {
///     println!("Entity {:?} at depth {}", node.entity_id, node.depth);
/// }
/// ```
///
/// **PROOF CORRESPONDENCE**: `khive.Retrieval.Graph.bfs_terminates`
/// Queue shrinks each iteration; visited set prevents re-enqueue; terminates when queue empty.
///
/// **PROOF CORRESPONDENCE**: `khive.Retrieval.Graph.bfs_complete`
/// All reachable vertices within max_depth are visited; BFS explores level-by-level.
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

            // Get edge weight and filter
            let edge_weight = get_edge_weight(&link);
            if edge_weight < min_weight {
                continue;
            }

            // Determine neighbor entity based on direction
            let neighbor = get_neighbor_entity(&link, &current, &options.direction);

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
