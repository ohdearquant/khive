//! Helper functions for graph traversal.

#[cfg(test)]
use super::compat::LinkId;
use super::compat::{EntityRef, Link, LinkStore, StorageContext};
use khive_score::DeterministicScore;

use crate::error::{Result, RetrievalError};

use super::types::Direction;

/// Extract edge weight from link properties.
///
/// Returns the `weight` property if present, otherwise defaults to 1.0.
pub fn get_edge_weight(link: &Link) -> f64 {
    link.properties
        .as_ref()
        .and_then(|props| props.get("weight"))
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0)
}

/// Check if a link matches the type filter.
///
/// Returns `true` if:
/// - The filter is `None` (all types match)
/// - The link's relation is in the filter list
pub fn matches_link_type(link: &Link, filter: &Option<Vec<String>>) -> bool {
    match filter {
        None => true,
        Some(types) => types.iter().any(|t| t == &link.relation),
    }
}

/// Get neighbor links based on direction.
///
/// # Arguments
///
/// * `store` - The link store to query
/// * `ctx` - Storage context for namespace isolation
/// * `entity` - The entity to get neighbors for
/// * `direction` - Which direction to follow edges
///
/// # Returns
///
/// Vector of links in the specified direction(s).
pub async fn get_neighbors<S: LinkStore>(
    store: &S,
    ctx: &StorageContext,
    entity: &EntityRef,
    direction: &Direction,
) -> Result<Vec<Link>> {
    let links =
        match direction {
            Direction::Out => store
                .outgoing(ctx, entity)
                .await
                .map_err(|e| RetrievalError::GraphTraversal(format!("link store error: {e}"))),
            Direction::In => store
                .incoming(ctx, entity)
                .await
                .map_err(|e| RetrievalError::GraphTraversal(format!("link store error: {e}"))),
            Direction::Both => {
                let mut out = store.outgoing(ctx, entity).await.map_err(|e| {
                    RetrievalError::GraphTraversal(format!("link store error: {e}"))
                })?;
                let incoming = store.incoming(ctx, entity).await.map_err(|e| {
                    RetrievalError::GraphTraversal(format!("link store error: {e}"))
                })?;
                out.extend(incoming);
                Ok(out)
            }
        };

    links
}

/// Convert graph depth to proximity score.
///
/// Closer nodes (lower depth) get higher scores. This enables fusion with
/// vector and keyword search results via RRF.
///
/// # Arguments
///
/// * `depth` - Distance from the start node (0 = start node itself)
/// * `max_depth` - Maximum traversal depth configured
///
/// # Returns
///
/// A `DeterministicScore` in range [0.0, 1.0]:
/// - depth=0 → 1.0 (at start node)
/// - depth=max_depth → 0.0 (maximum distance)
///
/// # Edge Cases
///
/// When `max_depth = 0`:
/// - depth=0 → 1.0 (only start node is reachable)
/// - depth>0 → 0.0 (should not occur, but handled safely)
///
/// # Proof Correspondence
///
/// This function maintains the invariant:
/// - `proximity_nonneg`: Result is always >= 0
/// - `proximity_bounded`: Result is always <= 1.0
/// - `proximity_mono`: Higher depth → lower score (monotonically decreasing)
pub fn proximity_score(depth: usize, max_depth: usize) -> DeterministicScore {
    // Guard against division by zero
    if max_depth == 0 {
        // At max_depth=0, only the start node (depth=0) is reachable
        return DeterministicScore::from_f64(if depth == 0 { 1.0 } else { 0.0 });
    }
    // Closer = higher score (inverse relationship)
    let proximity = 1.0 - (depth as f64 / max_depth as f64);
    DeterministicScore::from_f64(proximity)
}

/// Get the neighbor entity from a link based on traversal direction and current node.
///
/// # Arguments
///
/// * `link` - The link to extract neighbor from
/// * `current` - The current entity we're traversing from
/// * `direction` - The traversal direction
///
/// # Returns
///
/// `Some(neighbor)` when `current` is a valid endpoint for the given direction,
/// or `None` when `current` is not an endpoint of the link. Callers must handle
/// `None` by skipping the link; the old signature that always returned an entity
/// silently promoted unrelated nodes in `Direction::Both` when `current` was
/// neither source nor target (e.g., due to a buggy backend).
pub fn get_neighbor_entity(
    link: &Link,
    current: &EntityRef,
    direction: &Direction,
) -> Option<EntityRef> {
    match direction {
        Direction::Out if &link.source == current => Some(link.target.clone()),
        Direction::In if &link.target == current => Some(link.source.clone()),
        Direction::Both if &link.source == current => Some(link.target.clone()),
        Direction::Both if &link.target == current => Some(link.source.clone()),
        // current is not an endpoint of this link — skip it.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_link_type() {
        let link = Link::new(
            LinkId::NIL,
            EntityRef::External("a".to_string()),
            EntityRef::External("b".to_string()),
            "contains",
        );

        // No filter matches all
        assert!(matches_link_type(&link, &None));

        // Matching type
        assert!(matches_link_type(
            &link,
            &Some(vec!["contains".to_string()])
        ));

        // Non-matching type
        assert!(!matches_link_type(
            &link,
            &Some(vec!["references".to_string()])
        ));

        // Multiple types, one matches
        assert!(matches_link_type(
            &link,
            &Some(vec!["references".to_string(), "contains".to_string()])
        ));
    }

    #[test]
    fn test_get_edge_weight() {
        // No properties = default weight 1.0
        let link = Link::new(
            LinkId::NIL,
            EntityRef::External("a".to_string()),
            EntityRef::External("b".to_string()),
            "test",
        );
        assert_eq!(get_edge_weight(&link), 1.0);

        // With weight property
        let link_with_weight = Link::with_properties(
            LinkId::NIL,
            EntityRef::External("a".to_string()),
            EntityRef::External("b".to_string()),
            "test",
            serde_json::json!({"weight": 2.5}),
        );
        assert_eq!(get_edge_weight(&link_with_weight), 2.5);
    }

    #[test]
    fn test_get_neighbor_entity() {
        let source = EntityRef::External("source".to_string());
        let target = EntityRef::External("target".to_string());
        let link = Link::new(LinkId::NIL, source.clone(), target.clone(), "test");

        // Outgoing from source: return Some(target)
        assert_eq!(
            get_neighbor_entity(&link, &source, &Direction::Out),
            Some(target.clone())
        );

        // Incoming from target: return Some(source)
        assert_eq!(
            get_neighbor_entity(&link, &target, &Direction::In),
            Some(source.clone())
        );

        // Both from source: return Some(target) (other end)
        assert_eq!(
            get_neighbor_entity(&link, &source, &Direction::Both),
            Some(target.clone())
        );

        // Both from target: return Some(source) (other end)
        assert_eq!(
            get_neighbor_entity(&link, &target, &Direction::Both),
            Some(source.clone())
        );
    }

    #[test]
    fn test_get_neighbor_entity_unrelated_node_returns_none() {
        let source = EntityRef::External("source".to_string());
        let target = EntityRef::External("target".to_string());
        let unrelated = EntityRef::External("unrelated".to_string());
        let link = Link::new(LinkId::NIL, source.clone(), target.clone(), "test");

        // Outgoing from an unrelated node: link.source != unrelated → None
        assert_eq!(
            get_neighbor_entity(&link, &unrelated, &Direction::Out),
            None,
            "Out direction: current must be source; unrelated node must return None"
        );

        // Incoming from an unrelated node: link.target != unrelated → None
        assert_eq!(
            get_neighbor_entity(&link, &unrelated, &Direction::In),
            None,
            "In direction: current must be target; unrelated node must return None"
        );

        // Both from an unrelated node: current is neither source nor target → None
        assert_eq!(
            get_neighbor_entity(&link, &unrelated, &Direction::Both),
            None,
            "Both direction: unrelated node must return None"
        );
    }

    #[test]
    fn test_proximity_score_normal() {
        // At start node (depth=0)
        let score = proximity_score(0, 5);
        assert!((score.to_f64() - 1.0).abs() < f64::EPSILON);

        // At max depth
        let score = proximity_score(5, 5);
        assert!((score.to_f64() - 0.0).abs() < f64::EPSILON);

        // Midway
        let score = proximity_score(2, 4);
        assert!((score.to_f64() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_proximity_score_max_depth_zero() {
        // Edge case: max_depth = 0, depth = 0 (only valid case)
        let score = proximity_score(0, 0);
        assert!((score.to_f64() - 1.0).abs() < f64::EPSILON);

        // Edge case: max_depth = 0, depth > 0 (should not occur, but handled safely)
        let score = proximity_score(1, 0);
        assert!((score.to_f64() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_proximity_score_monotonic() {
        // Scores should decrease as depth increases
        let max_depth = 10;
        let mut prev_score = f64::MAX;

        for depth in 0..=max_depth {
            let score = proximity_score(depth, max_depth).to_f64();
            assert!(
                score <= prev_score,
                "Score should be monotonically decreasing"
            );
            prev_score = score;
        }
    }

    #[test]
    fn test_proximity_score_bounded() {
        // All scores should be in [0.0, 1.0]
        for max_depth in [0, 1, 5, 10, 100] {
            for depth in 0..=max_depth {
                let score = proximity_score(depth, max_depth).to_f64();
                assert!(score >= 0.0, "Score should be >= 0");
                assert!(score <= 1.0, "Score should be <= 1");
            }
        }
    }
}
