//! HNSW (Hierarchical Navigable Small World) vector index.

pub mod alias;
pub mod arena;
pub mod checkpoint;
mod config;
mod distance;
pub mod error;
mod index;
pub mod metrics;
mod node;
mod node_id;
pub mod search_context;
mod stats;

// Re-export public types
#[cfg(feature = "checkpoint")]
pub use checkpoint::{HnswCheckpoint, HnswCheckpointStore};
pub use checkpoint::{HnswCheckpointConfig, HnswSnapshot};
pub use config::{DistanceMetric, HnswConfig};
pub use index::HnswIndex;
pub use node_id::NodeId;
pub use search_context::HnswSearchContext;
pub use stats::{RebuildStats, TombstoneStats};
