//! `Anchor` and `AnchorGraph`: in-memory causal provenance chains for credit assignment.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

// Rebuild from the public vectors for each traversal. Keep the first node for a
// repeated ID and preserve edge insertion order within each adjacency list.
struct TraversalIndex<'a> {
    nodes: HashMap<Uuid, &'a AnchorRef>,
    neighbors: HashMap<Uuid, Vec<Uuid>>,
}

impl<'a> TraversalIndex<'a> {
    fn new(graph: &'a AnchorGraph, reverse: bool) -> Self {
        let mut nodes = HashMap::new();
        for node in &graph.nodes {
            nodes.entry(node.id).or_insert(node);
        }

        let mut neighbors: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for (from, to, _) in &graph.edges {
            let (key, neighbor) = if reverse { (*to, *from) } else { (*from, *to) };
            neighbors.entry(key).or_default().push(neighbor);
        }
        Self { nodes, neighbors }
    }

    fn find_node(&self, id: Uuid) -> Option<&'a AnchorRef> {
        self.nodes.get(&id).copied()
    }

    fn neighbors(&self, id: Uuid) -> impl Iterator<Item = Uuid> + '_ {
        self.neighbors.get(&id).into_iter().flatten().copied()
    }
}

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
        if max_depth == 0 {
            return Ok(Vec::new());
        }
        let index = TraversalIndex::new(graph, false);

        let mut visited = std::collections::HashSet::new();
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();

        visited.insert(start.id);
        queue.push_back((start.id, 0usize));

        while let Some((current_id, depth)) = queue.pop_front() {
            if let Some(node) = index.find_node(current_id) {
                if current_id != start.id {
                    result.push(node.clone());
                }

                if depth < max_depth {
                    for next_id in index.neighbors(current_id) {
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
        if max_depth == 0 {
            return Ok(Vec::new());
        }
        let index = TraversalIndex::new(graph, true);

        let mut visited = std::collections::HashSet::new();
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();

        visited.insert(outcome.id);
        queue.push_back((outcome.id, 0usize, 1.0f64));

        while let Some((current_id, depth, weight)) = queue.pop_front() {
            if current_id != outcome.id {
                if let Some(node) = index.find_node(current_id) {
                    result.push((node.clone(), weight));
                }
            }

            if depth < max_depth {
                let predecessors: Vec<(Uuid, f64)> = index
                    .neighbors(current_id)
                    .filter(|id| visited.insert(*id))
                    .map(|id| (id, weight * 0.5))
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
#[path = "anchor_tests.rs"]
mod tests;
