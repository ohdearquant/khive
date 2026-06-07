//! Shortest path algorithm using bidirectional BFS.

use std::collections::{HashMap, VecDeque};

use super::compat::{EntityRef, Link, LinkStore, StorageContext};

use crate::error::{Result, RetrievalError};

use super::types::{PathNode, MAX_TRAVERSAL_DEPTH};

/// Bidirectional BFS shortest path. Returns `Some(path)` inclusive of endpoints, or `None` if unreachable within `max_depth`.
pub async fn find_shortest_path<S: LinkStore>(
    store: &S,
    ctx: &StorageContext,
    from: EntityRef,
    to: EntityRef,
    max_depth: usize,
) -> Result<Option<Vec<PathNode>>> {
    // Clamp max_depth to prevent excessive search
    let max_depth = max_depth.min(MAX_TRAVERSAL_DEPTH);

    // Same node = trivial path (EntityRef implements Eq)
    if from == to {
        return Ok(Some(vec![PathNode::start(from)]));
    }

    // Forward search state: entity -> (depth, parent_entity, link to this node)
    // EntityRef implements Hash + Eq, enabling direct use as HashMap key.
    let mut forward_visited: HashMap<EntityRef, (usize, Option<EntityRef>, Option<Link>)> =
        HashMap::new();
    let mut forward_queue: VecDeque<EntityRef> = VecDeque::new();
    forward_visited.insert(from.clone(), (0, None, None));
    forward_queue.push_back(from.clone());

    // Backward search state: entity -> (depth, child_entity, link from this node)
    let mut backward_visited: HashMap<EntityRef, (usize, Option<EntityRef>, Option<Link>)> =
        HashMap::new();
    let mut backward_queue: VecDeque<EntityRef> = VecDeque::new();
    backward_visited.insert(to.clone(), (0, None, None));
    backward_queue.push_back(to.clone());

    let mut best_meeting: Option<(EntityRef, usize)> = None; // (node, total_dist)
    let mut current_depth = 0;

    // Alternate between forward and backward expansion.
    // Process entire BFS levels before checking for a meeting point so we
    // find the meeting node with the smallest total distance, not just the
    // first one encountered (which depends on HashMap iteration order).
    while !forward_queue.is_empty() || !backward_queue.is_empty() {
        // Stop expanding once we've reached max_depth — any neighbor would be
        // at depth current_depth + 1 = max_depth + 1, exceeding the budget.
        if current_depth >= max_depth {
            break;
        }

        // Expand forward frontier (following outgoing edges)
        let forward_level_size = forward_queue.len();
        for _ in 0..forward_level_size {
            if let Some(current) = forward_queue.pop_front() {
                let outgoing = store.outgoing(ctx, &current).await.map_err(|e| {
                    RetrievalError::GraphTraversal(format!("link store error: {e}"))
                })?;

                for link in outgoing {
                    let neighbor = link.target.clone();

                    if !forward_visited.contains_key(&neighbor) {
                        let fwd_dist = current_depth + 1;
                        // Store: link goes from current to neighbor
                        forward_visited.insert(
                            neighbor.clone(),
                            (fwd_dist, Some(current.clone()), Some(link)),
                        );
                        forward_queue.push_back(neighbor.clone());

                        // Check if we've met the backward search.
                        // Only accept meetings whose total path length ≤ max_depth.
                        if let Some((bwd_dist, _, _)) = backward_visited.get(&neighbor) {
                            let total = fwd_dist + bwd_dist;
                            if total <= max_depth
                                && best_meeting.as_ref().is_none_or(|&(_, best)| total < best)
                            {
                                best_meeting = Some((neighbor, total));
                            }
                        }
                    }
                }
            }
        }

        // If we found a meeting point during forward expansion, the best
        // meeting at this depth is optimal -- no need to expand backward.
        if best_meeting.is_some() {
            break;
        }

        // Expand backward frontier (following incoming edges)
        let backward_level_size = backward_queue.len();
        for _ in 0..backward_level_size {
            if let Some(current) = backward_queue.pop_front() {
                let incoming = store.incoming(ctx, &current).await.map_err(|e| {
                    RetrievalError::GraphTraversal(format!("link store error: {e}"))
                })?;

                for link in incoming {
                    // For incoming: link goes from neighbor to current
                    let neighbor = link.source.clone();

                    if !backward_visited.contains_key(&neighbor) {
                        let bwd_dist = current_depth + 1;
                        // Store: link goes from neighbor to current (for path reconstruction)
                        backward_visited.insert(
                            neighbor.clone(),
                            (bwd_dist, Some(current.clone()), Some(link)),
                        );
                        backward_queue.push_back(neighbor.clone());

                        // Check if we've met the forward search.
                        // Only accept meetings whose total path length ≤ max_depth.
                        if let Some((fwd_dist, _, _)) = forward_visited.get(&neighbor) {
                            let total = fwd_dist + bwd_dist;
                            if total <= max_depth
                                && best_meeting.as_ref().is_none_or(|&(_, best)| total < best)
                            {
                                best_meeting = Some((neighbor, total));
                            }
                        }
                    }
                }
            }
        }

        // After processing both frontiers at this depth, check for meeting
        if best_meeting.is_some() {
            break;
        }

        current_depth += 1;
    }

    // Reconstruct path if found
    match best_meeting {
        Some((mid, _total_dist)) => {
            let path = reconstruct_path(&forward_visited, &backward_visited, &mid);
            Ok(Some(path))
        }
        None => Ok(None),
    }
}

/// Reconstruct the path from forward and backward visited maps.
fn reconstruct_path(
    forward_visited: &HashMap<EntityRef, (usize, Option<EntityRef>, Option<Link>)>,
    backward_visited: &HashMap<EntityRef, (usize, Option<EntityRef>, Option<Link>)>,
    meeting_point: &EntityRef,
) -> Vec<PathNode> {
    // Build forward part: start -> meeting_point
    let mut forward_entities: Vec<EntityRef> = Vec::new();
    let mut forward_links: Vec<Option<Link>> = Vec::new();
    let mut current = meeting_point.clone();

    // Walk backwards from meeting point to start
    while let Some((_, parent, link)) = forward_visited.get(&current) {
        forward_entities.push(current.clone());
        forward_links.push(link.clone());
        match parent {
            Some(p) => current = p.clone(),
            None => break,
        }
    }

    // Reverse to get start -> meeting_point order
    forward_entities.reverse();
    forward_links.reverse();

    // Build backward part: meeting_point -> end
    let mut backward_entities: Vec<EntityRef> = Vec::new();
    let mut backward_links: Vec<Option<Link>> = Vec::new();

    // Start from meeting point, walk towards 'to'
    if let Some((_, Some(child), link)) = backward_visited.get(meeting_point) {
        backward_links.push(link.clone());
        current = child.clone();

        while let Some((_, next_child, link)) = backward_visited.get(&current) {
            backward_entities.push(current.clone());
            match next_child {
                Some(nc) => {
                    backward_links.push(link.clone());
                    current = nc.clone();
                }
                None => break,
            }
        }
        // Defensive: if the while loop exited because backward_visited
        // lacked an entry for `current` (shouldn't happen in a consistent
        // graph, but guards against any map skew), include `current` so
        // the target node is never silently dropped.
        if backward_entities.last() != Some(&current) {
            backward_entities.push(current.clone());
        }
    }

    // Combine into final path
    let mut path: Vec<PathNode> = Vec::new();

    // Add forward nodes
    for (i, entity) in forward_entities.iter().enumerate() {
        let link = if i == 0 {
            None // Start node has no inbound edge
        } else {
            forward_links.get(i).cloned().flatten()
        };

        path.push(PathNode {
            entity_id: entity.clone(),
            depth: i,
            via_link: link,
            path_weight: i as f64,
        });
    }

    // Add backward nodes (these come after meeting point)
    let base_depth = path.len();
    for (i, entity) in backward_entities.iter().enumerate() {
        let link = backward_links.get(i).cloned().flatten();
        path.push(PathNode {
            entity_id: entity.clone(),
            depth: base_depth + i,
            via_link: link,
            path_weight: (base_depth + i) as f64,
        });
    }

    path
}
