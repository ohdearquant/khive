//! Graph traversal algorithms for relationship-aware retrieval.
//!
//! This module provides BFS, DFS, and shortest path algorithms for exploring
//! the knowledge graph. All algorithms operate on the `LinkStore` trait from
//! khive-db, enabling relationship-aware retrieval pipelines.
//!
//! # Algorithm Selection Guide
//!
//! | Use Case | Algorithm | Function |
//! |----------|-----------|----------|
//! | Explore neighbors | BFS | [`bfs_traverse`] |
//! | Find shortest path | Bidirectional BFS | [`find_shortest_path`] |
//! | Deep exploration | DFS | [`dfs_traverse`] |
//!
//! # Architecture (ADR-004)
//!
//! ```text
//! khive-db                    khive-retrieval
//! +-----------------+         +----------------------+
//! | LinkStore trait |  <---   | Traversal algorithms |
//! | EntityRef, Link |         | PathNode, Direction  |
//! | StorageContext  |         | TraversalOptions     |
//! +-----------------+         +----------------------+
//! ```
//!
//! # RETRIEVAL-09: Audit Logging for Graph Operations
//!
//! **Current state**: Graph traversal algorithms do NOT emit audit logs.
//!
//! **Design decision**: Audit logging is the responsibility of the caller
//! (typically khive-api or middleware layer), not the retrieval algorithms.
//! This keeps the traversal code focused and testable.
//!
//! **What callers should log**:
//!
//! | Event | Context to Capture |
//! |-------|-------------------|
//! | Traversal start | start_node, direction, max_depth, link_types |
//! | Traversal complete | nodes_visited, paths_found, duration_ms |
//! | Depth limit hit | node_at_limit, depth |
//! | Result limit hit | total_candidates, returned_count |
//!
//! **Future work**: If audit logging moves into the retrieval layer, add
//! a `TraversalObserver` trait for pluggable logging without coupling to
//! a specific logging framework.
//!
//! # Safety Limits
//!
//! All algorithms enforce safety limits to prevent runaway traversals:
//! - [`MAX_TRAVERSAL_DEPTH`]: Maximum hops from start (20)
//! - [`MAX_TRAVERSAL_RESULTS`]: Maximum nodes returned (10,000)
//!
//! # Example
//!
//! ```ignore
//! use khive_retrieval::graph::{bfs_traverse, find_shortest_path, TraversalOptions, Direction};
//! use khive_db::{LinkStore, StorageContext};
//!
//! // BFS exploration
//! let options = TraversalOptions::new(3)
//!     .with_direction(Direction::Out)
//!     .with_link_types(["contains", "references"]);
//!
//! let neighbors = bfs_traverse(&store, &ctx, start_ref, &options).await?;
//!
//! // Find shortest path
//! if let Some(path) = find_shortest_path(&store, &ctx, from, to, 10).await? {
//!     println!("Path length: {} hops", path.len() - 1);
//! }
//! ```
//!
//! See [ADR-004](../docs/ADR-004-graph-traversal.md) for algorithm specification.

mod bfs;
mod compat;
mod dfs;
/// Helper functions for graph traversal (proximity scoring, neighbor extraction, etc.).
pub mod helpers;
mod shortest;
mod types;

// INLINE TEST JUSTIFICATION: tests access `compat::test_context` and `compat::MockLinkStore`
// through the module-private `use super::compat::*` import; the graph-legacy feature gate
// means these types are not re-exported publicly, so test coverage cannot live in tests/.
#[cfg(test)]
mod tests;

// Re-export compat types (legacy graph API shims)
pub use compat::{test_context, EntityRef, Link, LinkId, LinkStore, MockLinkStore, StorageContext};

// Re-export public types
pub use types::{
    Direction, PathNode, TraversalOptions, MAX_TRAVERSAL_DEPTH, MAX_TRAVERSAL_RESULTS,
};

// Re-export direction variants for convenience
pub use types::Direction::{Both, In, Out};

// Re-export traversal algorithms
pub use bfs::bfs_traverse;
pub use dfs::dfs_traverse;
pub use shortest::find_shortest_path;
