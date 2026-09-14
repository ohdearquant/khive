//! Pagination types for list operations.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Immutable insertion-sequence boundary for record-list pagination.
///
/// `sequence` is assigned by the storage backend when an id is first inserted.
/// It is strictly increasing, never reused, and remains fixed across updates or
/// soft deletion. `id` is retained as the public continuation value; it is not
/// part of the storage ordering key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekCursor {
    pub sequence: i64,
    pub id: Uuid,
}

/// One keyset page plus the boundary needed to continue it.
#[derive(Clone, Debug)]
pub struct SeekPage<T> {
    pub items: Vec<T>,
    pub next_after: Option<SeekCursor>,
}

impl<T> Default for SeekPage<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next_after: None,
        }
    }
}

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

/// A count whose work and reported value are bounded by an explicit cap.
///
/// `saturated` distinguishes an exact count equal to `cap` from a population
/// larger than the cap. When it is true, `count == cap` is a lower bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundedCount {
    pub count: u64,
    pub cap: u64,
    pub saturated: bool,
}

/// The common response metadata for an operation that bounds a caller's
/// requested limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LimitReport {
    pub requested_limit: u32,
    pub effective_limit: u32,
    pub limit_clamped: bool,
}

impl LimitReport {
    pub fn new(requested_limit: u32, effective_limit: u32) -> Self {
        Self {
            requested_limit,
            effective_limit,
            limit_clamped: requested_limit > effective_limit,
        }
    }

    pub fn value(self) -> serde_json::Value {
        serde_json::to_value(self).expect("LimitReport serialization cannot fail")
    }
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

    #[test]
    fn limit_report_has_one_shared_wire_shape() {
        assert_eq!(
            LimitReport::new(201, 200).value(),
            serde_json::json!({
                "requested_limit": 201,
                "effective_limit": 200,
                "limit_clamped": true,
            })
        );
        assert_eq!(
            LimitReport::new(2, 2).value(),
            serde_json::json!({
                "requested_limit": 2,
                "effective_limit": 2,
                "limit_clamped": false,
            })
        );
    }
}
