//! BFS, DFS, and shortest-path graph traversal over the `LinkStore` trait.

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
