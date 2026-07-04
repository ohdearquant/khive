//! Pagination types for list operations.

use serde::{Deserialize, Serialize};

/// Raw deserialization target for [`PageRequest`].
#[derive(Deserialize)]
struct PageRequestRaw {
    offset: u64,
    limit: u32,
}

impl TryFrom<PageRequestRaw> for PageRequest {
    type Error = String;

    fn try_from(raw: PageRequestRaw) -> Result<Self, Self::Error> {
        if raw.offset > i64::MAX as u64 {
            return Err(format!(
                "PageRequest: offset must be <= i64::MAX, got {}",
                raw.offset
            ));
        }
        Ok(Self {
            offset: raw.offset,
            limit: raw.limit,
        })
    }
}

/// Offset-based pagination cursor for list operations. Deserialization rejects
/// `offset > i64::MAX` (STORAGE-AUD-003), since SQLite backends narrow offset
/// to `i64`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "PageRequestRaw")]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// STORAGE-AUD-003 / #485: offset > i64::MAX must be rejected by serde
    /// deserialization instead of silently narrowing to a negative i64 at the
    /// SQLite boundary.
    #[test]
    fn page_offset_over_i64max_rejected() {
        let raw = serde_json::json!({
            "offset": (i64::MAX as u64) + 1,
            "limit": 50,
        });
        let result: Result<PageRequest, _> = serde_json::from_value(raw);
        assert!(
            result.is_err(),
            "offset > i64::MAX must be rejected, got {result:?}"
        );
    }

    #[test]
    fn page_offset_at_i64max_accepted() {
        let raw = serde_json::json!({
            "offset": i64::MAX as u64,
            "limit": 50,
        });
        let result: Result<PageRequest, _> = serde_json::from_value(raw);
        assert!(result.is_ok(), "offset == i64::MAX must be accepted");
    }
}
