//! Pagination types for list operations.

use serde::{Deserialize, Serialize};

/// Offset-based pagination cursor for list operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageRequest {
    pub offset: u64,
    pub limit: u32,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
        }
    }
}

/// A paginated result slice with an optional total count.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: Option<u64>,
}
