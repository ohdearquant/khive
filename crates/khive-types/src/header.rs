//! Common header for substrate records.

use crate::{Id128, Namespace, Timestamp};

/// Fields shared by every substrate record (Note, Entity, Event).
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Header {
    pub id: Id128,
    pub namespace: Namespace,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Header {
    pub fn new(id: Id128, namespace: Namespace, now: Timestamp) -> Self {
        Self {
            id,
            namespace,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn touch(&mut self, now: Timestamp) {
        self.updated_at = now;
    }
}
