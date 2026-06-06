//! `Anchor` and `AnchorGraph`: in-memory causal provenance chains for credit assignment.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::FoldError;

/// A reference to an anchor (a source of truth for a claim or artifact).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct AnchorRef {
    /// Unique identifier for this anchor.
    pub id: Uuid,
    /// Kind of anchor: "paper", "book", "web", "record", "domain", "composite", ...
    pub kind: String,
    /// Optional stable identifier within the kind (e.g., DOI, ISBN, URL, record ID).
    pub stable_id: Option<String>,
}

/// A graph of anchors, forming a causal chain.
///
/// Callers persist this across sessions. Brain navigates but doesn't own.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct AnchorGraph {
    /// All anchor nodes in this graph.
    pub nodes: Vec<AnchorRef>,
    /// Edges: (from_id, to_id, relation) where relation is e.g. "derives_from", "uses", "contradicts".
    pub edges: Vec<(Uuid, Uuid, String)>,
}

impl AnchorGraph {
    /// Create an empty anchor graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node to the graph.
    pub fn add_node(&mut self, anchor: AnchorRef) {
        self.nodes.push(anchor);
    }

    /// Add an edge to the graph.
    pub fn add_edge(&mut self, from: Uuid, to: Uuid, relation: impl Into<String>) {
        self.edges.push((from, to, relation.into()));
    }

    /// Find a node by its ID.
    pub fn find_node(&self, id: Uuid) -> Option<&AnchorRef> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Get outgoing edges from a node.
    pub fn outgoing(&self, from: Uuid) -> impl Iterator<Item = (Uuid, &str)> {
        self.edges
            .iter()
            .filter(move |(f, _, _)| *f == from)
            .map(|(_, to, rel)| (*to, rel.as_str()))
    }

    /// Get incoming edges to a node.
    pub fn incoming(&self, to: Uuid) -> impl Iterator<Item = (Uuid, &str)> {
        self.edges
            .iter()
            .filter(move |(_, t, _)| *t == to)
            .map(|(from, _, rel)| (*from, rel.as_str()))
    }
}

/// The Anchor primitive.
pub trait Anchor: Send + Sync {
    /// Trace the causal chain from a starting anchor to its sources.
    fn trace(
        &self,
        graph: &AnchorGraph,
        start: &AnchorRef,
        max_depth: usize,
    ) -> Result<Vec<AnchorRef>, FoldError>;

    /// Reverse trace: given an outcome anchor, find the anchors that contributed.
    fn credit(
        &self,
        graph: &AnchorGraph,
        outcome: &AnchorRef,
        max_depth: usize,
    ) -> Result<Vec<(AnchorRef, f64)>, FoldError>;
}

/// A BFS-based anchor implementation.
///
/// Traces the causal chain by following forward edges (for `trace`) or
/// backward edges (for `credit`) up to `max_depth` hops.
#[derive(Debug, Clone, Copy, Default)]
pub struct BfsAnchor;

impl Anchor for BfsAnchor {
    fn trace(
        &self,
        graph: &AnchorGraph,
        start: &AnchorRef,
        max_depth: usize,
    ) -> Result<Vec<AnchorRef>, FoldError> {
        if graph.find_node(start.id).is_none() {
            return Err(FoldError::AnchorNotFound(start.id.to_string()));
        }

        let mut visited = std::collections::HashSet::new();
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();

        visited.insert(start.id);
        queue.push_back((start.id, 0usize));

        while let Some((current_id, depth)) = queue.pop_front() {
            if let Some(node) = graph.find_node(current_id) {
                if current_id != start.id {
                    result.push(node.clone());
                }

                if depth < max_depth {
                    for (next_id, _rel) in graph.outgoing(current_id) {
                        if visited.insert(next_id) {
                            queue.push_back((next_id, depth + 1));
                        }
                    }
                }
            }
        }

        Ok(result)
    }

    fn credit(
        &self,
        graph: &AnchorGraph,
        outcome: &AnchorRef,
        max_depth: usize,
    ) -> Result<Vec<(AnchorRef, f64)>, FoldError> {
        if graph.find_node(outcome.id).is_none() {
            return Err(FoldError::AnchorNotFound(outcome.id.to_string()));
        }

        let mut visited = std::collections::HashSet::new();
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();

        visited.insert(outcome.id);
        queue.push_back((outcome.id, 0usize, 1.0f64));

        while let Some((current_id, depth, weight)) = queue.pop_front() {
            if current_id != outcome.id {
                if let Some(node) = graph.find_node(current_id) {
                    result.push((node.clone(), weight));
                }
            }

            if depth < max_depth {
                let predecessors: Vec<(Uuid, f64)> = graph
                    .incoming(current_id)
                    .filter(|(id, _)| visited.insert(*id))
                    .map(|(id, _)| (id, weight * 0.5))
                    .collect();

                for (pred_id, pred_weight) in predecessors {
                    queue.push_back((pred_id, depth + 1, pred_weight));
                }
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ref(id: u128, kind: &str) -> AnchorRef {
        AnchorRef {
            id: Uuid::from_u128(id),
            kind: kind.to_string(),
            stable_id: None,
        }
    }

    #[test]
    fn test_anchor_ref_fields() {
        let r = AnchorRef {
            id: Uuid::new_v4(),
            kind: "paper".into(),
            stable_id: Some("doi:10.1234/x".into()),
        };
        assert_eq!(r.kind, "paper");
        assert!(r.stable_id.is_some());
    }

    #[test]
    fn test_anchor_graph_add_and_find() {
        let mut graph = AnchorGraph::new();
        let a = make_ref(1, "record");
        let b = make_ref(2, "source");
        graph.add_node(a.clone());
        graph.add_node(b.clone());
        graph.add_edge(a.id, b.id, "derives_from");

        assert!(graph.find_node(a.id).is_some());
        assert!(graph.find_node(Uuid::nil()).is_none());
    }

    #[test]
    fn test_bfs_anchor_trace_not_found() {
        let graph = AnchorGraph::new();
        let unknown = make_ref(99, "unknown");
        let err = BfsAnchor.trace(&graph, &unknown, 5).unwrap_err();
        assert!(matches!(err, FoldError::AnchorNotFound(_)));
    }

    #[test]
    fn test_bfs_anchor_trace_chain() {
        let mut graph = AnchorGraph::new();
        let a = make_ref(1, "record");
        let b = make_ref(2, "source");
        let c = make_ref(3, "paper");
        graph.add_node(a.clone());
        graph.add_node(b.clone());
        graph.add_node(c.clone());
        graph.add_edge(a.id, b.id, "derives_from");
        graph.add_edge(b.id, c.id, "uses");

        let chain = BfsAnchor.trace(&graph, &a, 5).unwrap();
        assert_eq!(chain.len(), 2);
        assert!(chain.iter().any(|r| r.id == b.id));
        assert!(chain.iter().any(|r| r.id == c.id));
    }

    #[test]
    fn test_bfs_anchor_trace_max_depth() {
        let mut graph = AnchorGraph::new();
        let nodes: Vec<AnchorRef> = (1..=5).map(|i| make_ref(i, "node")).collect();
        for n in &nodes {
            graph.add_node(n.clone());
        }
        for i in 0..4 {
            graph.add_edge(nodes[i].id, nodes[i + 1].id, "next");
        }

        // With max_depth=1, only one hop from start
        let chain = BfsAnchor.trace(&graph, &nodes[0], 1).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, nodes[1].id);
    }

    #[test]
    fn test_bfs_anchor_credit_not_found() {
        let graph = AnchorGraph::new();
        let unknown = make_ref(99, "unknown");
        let err = BfsAnchor.credit(&graph, &unknown, 5).unwrap_err();
        assert!(matches!(err, FoldError::AnchorNotFound(_)));
    }

    #[test]
    fn test_bfs_anchor_credit_basic() {
        let mut graph = AnchorGraph::new();
        let source = make_ref(1, "paper");
        let intermediate = make_ref(2, "record");
        let outcome = make_ref(3, "composition");
        graph.add_node(source.clone());
        graph.add_node(intermediate.clone());
        graph.add_node(outcome.clone());
        // edges point forward: source → intermediate → outcome
        graph.add_edge(source.id, intermediate.id, "uses");
        graph.add_edge(intermediate.id, outcome.id, "derives_from");

        let credits = BfsAnchor.credit(&graph, &outcome, 5).unwrap();
        assert!(!credits.is_empty());
        // intermediate should be credited with weight > 0
        let inter_credit = credits.iter().find(|(r, _)| r.id == intermediate.id);
        assert!(inter_credit.is_some());
        assert!(inter_credit.unwrap().1 > 0.0f64);
    }

    #[test]
    fn credit_weights_are_f64() {
        let mut graph = AnchorGraph::new();
        let source = make_ref(10, "source");
        let outcome = make_ref(11, "outcome");
        graph.add_node(source.clone());
        graph.add_node(outcome.clone());
        graph.add_edge(source.id, outcome.id, "causes");

        let credits: Vec<(AnchorRef, f64)> = BfsAnchor.credit(&graph, &outcome, 2).unwrap();
        assert!(!credits.is_empty());
        let w: f64 = credits[0].1;
        assert!(w > 0.0f64 && w <= 1.0f64);
    }
}
