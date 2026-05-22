//! HNSW (Hierarchical Navigable Small World) vector index.
//!
//! High-performance approximate nearest neighbor search with O(log N) complexity.
//! See ADR-003 for configuration and performance characteristics.
//!
//! # Usage
//!
//! ```rust,ignore
//! use khive_hnsw::{HnswConfig, HnswIndex, NodeId};
//!
//! // Create index with default config
//! let config = HnswConfig::default();
//! let mut index = HnswIndex::new(768);
//!
//! // Insert vectors
//! let id = NodeId::new([1u8; 16]);
//! index.insert(id, vec![0.1; 768])?;
//!
//! // Search for k nearest neighbors
//! let results = index.search(&query_vector, 10)?;
//! for (id, score) in results {
//!     println!("{}: {}", id, score);
//! }
//! ```
//!
//! # Algorithm
//!
//! HNSW builds a multi-layer graph where:
//! - Higher layers have fewer nodes (exponentially distributed)
//! - Search starts from top layer and descends
//! - Each layer uses greedy search to find nearest neighbors
//!
//! Reference: Malkov & Yashunin, "Efficient and robust approximate nearest
//! neighbor search using Hierarchical Navigable Small World graphs" (2018)

pub mod alias;
pub mod arena;
pub mod checkpoint;
mod config;
mod distance;
pub mod error;
mod index;
pub mod metrics;
mod node;
pub(crate) mod search_context;
mod stats;

#[cfg(test)]
mod tests;

// Re-export public types
#[cfg(feature = "checkpoint")]
pub use checkpoint::{HnswCheckpoint, HnswCheckpointStore};
pub use checkpoint::{HnswCheckpointConfig, HnswSnapshot};
pub use config::{DistanceMetric, HnswConfig};
pub use index::HnswIndex;
pub use search_context::HnswSearchContext;
pub use stats::{RebuildStats, TombstoneStats};

/// 128-bit opaque node identifier for HNSW entries.
///
/// Serializes as a 32-character lowercase hex string (compatible with the
/// snapshot format used by `HnswSnapshot`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId([u8; 16]);

impl NodeId {
    /// Create a `NodeId` from raw bytes.
    #[inline]
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Return the raw byte representation.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl std::fmt::Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeId(")?;
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl serde::Serialize for NodeId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut hex = String::with_capacity(32);
        for b in &self.0 {
            hex.push_str(&format!("{b:02x}"));
        }
        s.serialize_str(&hex)
    }
}

impl<'de> serde::Deserialize<'de> for NodeId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() != 32 {
            return Err(serde::de::Error::custom(format!(
                "NodeId hex string must be 32 chars, got {}",
                s.len()
            )));
        }
        let mut bytes = [0u8; 16];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = char::from(chunk[0])
                .to_digit(16)
                .ok_or_else(|| serde::de::Error::custom("invalid hex character"))?
                as u8;
            let lo = char::from(chunk[1])
                .to_digit(16)
                .ok_or_else(|| serde::de::Error::custom("invalid hex character"))?
                as u8;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(NodeId(bytes))
    }
}
