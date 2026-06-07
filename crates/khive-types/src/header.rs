//! Common header for substrate records.

use crate::{Id128, Namespace, Timestamp};

/// Fields shared by every substrate record (Note, Entity, Event).
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Header {
    /// Unique identifier for this record.
    pub id: Id128,
    /// Namespace that owns and isolates this record.
    pub namespace: Namespace,
    /// Wall-clock time when this record was first created.
    pub created_at: Timestamp,
    /// Wall-clock time of the most recent mutation.
    pub updated_at: Timestamp,
}

impl Header {
    /// Construct a new header with `created_at` and `updated_at` both set to `now`.
    pub fn new(id: Id128, namespace: Namespace, now: Timestamp) -> Self {
        Self {
            id,
            namespace,
            created_at: now,
            updated_at: now,
        }
    }

    /// Advance `updated_at` to `now`, preserving `created_at`.
    pub fn touch(&mut self, now: Timestamp) {
        self.updated_at = now;
    }
}
