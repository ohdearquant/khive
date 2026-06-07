//! Graph traversal types.

use super::compat::{EntityRef, Link};
use serde::{Deserialize, Serialize};

/// Maximum traversal depth to prevent stack overflow and runaway queries.
pub const MAX_TRAVERSAL_DEPTH: usize = 20;

/// Maximum results per traversal to prevent memory exhaustion.
pub const MAX_TRAVERSAL_RESULTS: usize = 10_000;

/// Direction of edge traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Follow outgoing edges (source -> target).
    #[default]
    #[serde(alias = "Out")]
    Out,
    /// Follow incoming edges (target <- source).
    #[serde(alias = "In")]
    In,
    /// Follow edges in both directions.
    #[serde(alias = "Both")]
    Both,
}

/// A node in a traversal path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathNode {
    /// The entity at this position in the path.
    pub entity_id: EntityRef,
    /// Depth from the start node (0 = start node).
    pub depth: usize,
    /// The link that led to this node (None for start node).
    pub via_link: Option<Link>,
    /// Cumulative path weight (sum of edge weights).
    pub path_weight: f64,
}

impl PathNode {
    /// Create a new path node for the start position.
    pub fn start(entity_id: EntityRef) -> Self {
        Self {
            entity_id,
            depth: 0,
            via_link: None,
            path_weight: 0.0,
        }
    }

    /// Create a path node from an outgoing link.
    pub fn from_outgoing_link(link: Link, depth: usize, path_weight: f64) -> Self {
        Self {
            entity_id: link.target.clone(),
            depth,
            via_link: Some(link),
            path_weight,
        }
    }

    /// Create a path node from an incoming link.
    pub fn from_incoming_link(link: Link, depth: usize, path_weight: f64) -> Self {
        Self {
            entity_id: link.source.clone(),
            depth,
            via_link: Some(link),
            path_weight,
        }
    }
}

/// Options for graph traversal operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraversalOptions {
    /// Maximum depth to traverse (clamped to [`MAX_TRAVERSAL_DEPTH`]).
    pub max_depth: usize,
    /// Maximum number of nodes to return (clamped to [`MAX_TRAVERSAL_RESULTS`]).
    pub limit: Option<usize>,
    /// Direction to follow edges.
    pub direction: Direction,
    /// Filter by link relation types (None = all types).
    pub link_types: Option<Vec<String>>,
    /// Minimum edge weight to consider (for weighted traversal).
    pub min_weight: Option<f64>,
}

impl Default for TraversalOptions {
    fn default() -> Self {
        Self {
            max_depth: 3,
            limit: Some(MAX_TRAVERSAL_RESULTS),
            direction: Direction::Out,
            link_types: None,
            min_weight: None,
        }
    }
}

impl TraversalOptions {
    /// Create new options with specified max depth.
    pub fn new(max_depth: usize) -> Self {
        Self {
            max_depth: max_depth.min(MAX_TRAVERSAL_DEPTH),
            limit: Some(MAX_TRAVERSAL_RESULTS),
            ..Default::default()
        }
    }

    /// Set the maximum traversal depth.
    #[must_use]
    pub fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth.min(MAX_TRAVERSAL_DEPTH);
        self
    }

    /// Set traversal direction.
    #[must_use]
    pub fn with_direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }

    /// Filter to specific link relation types.
    #[must_use]
    pub fn with_link_types(mut self, types: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.link_types = Some(types.into_iter().map(Into::into).collect());
        self
    }

    /// Set maximum number of results.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit.min(MAX_TRAVERSAL_RESULTS));
        self
    }

    /// Set minimum edge weight threshold.
    #[must_use]
    pub fn with_min_weight(mut self, weight: f64) -> Self {
        self.min_weight = Some(weight);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_traversal_options_default() {
        let opts = TraversalOptions::default();
        assert_eq!(opts.max_depth, 3);
        assert_eq!(opts.direction, Direction::Out);
        assert!(opts.link_types.is_none());
        assert_eq!(opts.limit, Some(MAX_TRAVERSAL_RESULTS));
    }

    #[test]
    fn test_traversal_options_builder() {
        let opts = TraversalOptions::new(5)
            .with_direction(Direction::Both)
            .with_link_types(["contains", "references"])
            .with_limit(100)
            .with_min_weight(0.5);

        assert_eq!(opts.max_depth, 5);
        assert_eq!(opts.direction, Direction::Both);
        assert_eq!(
            opts.link_types,
            Some(vec!["contains".to_string(), "references".to_string()])
        );
        assert_eq!(opts.limit, Some(100));
        assert_eq!(opts.min_weight, Some(0.5));
    }

    #[test]
    fn test_traversal_options_clamping() {
        let opts = TraversalOptions::new(100);
        assert_eq!(opts.max_depth, MAX_TRAVERSAL_DEPTH);

        let opts = TraversalOptions::new(3).with_limit(100_000);
        assert_eq!(opts.limit, Some(MAX_TRAVERSAL_RESULTS));
    }

    #[test]
    fn test_path_node_start() {
        let entity = EntityRef::External("test".to_string());
        let node = PathNode::start(entity.clone());

        assert_eq!(node.entity_id, entity);
        assert_eq!(node.depth, 0);
        assert!(node.via_link.is_none());
        assert_eq!(node.path_weight, 0.0);
    }

    #[test]
    fn test_direction_default() {
        let dir = Direction::default();
        assert_eq!(dir, Direction::Out);
    }

    #[test]
    fn test_safety_constants() {
        assert_eq!(MAX_TRAVERSAL_DEPTH, 20);
        assert_eq!(MAX_TRAVERSAL_RESULTS, 10_000);
    }
}
