//! Note substrate — temporal-referential records used throughout khive.

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::entity::PropertyValue;
use crate::{Header, Timestamp};

/// Lifecycle status of a note. Cross-cutting across all note kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum NoteStatus {
    #[default]
    Active,
    Archived,
    Deleted,
}

impl NoteStatus {
    /// Return the canonical lowercase string for this status, as stored on the wire.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
        }
    }
}

impl fmt::Display for NoteStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl core::str::FromStr for NoteStatus {
    type Err = crate::error::UnknownVariant;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "active" => Ok(Self::Active),
            "archived" => Ok(Self::Archived),
            "deleted" => Ok(Self::Deleted),
            other => Err(crate::error::UnknownVariant::new(
                "note_status",
                other,
                &["active", "archived", "deleted"],
            )),
        }
    }
}

/// A note record — temporal-referential content plus free-form properties.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Note {
    /// Identity and namespace metadata shared by all substrate records.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub header: Header,
    /// Pack-declared kind string (e.g. `"observation"`, `"task"`, `"memory"`).
    pub kind: String,
    /// Cross-cutting lifecycle status.
    pub status: NoteStatus,
    /// Main textual body of the note.
    pub content: String,
    /// Arbitrary structured metadata as key-value pairs.
    pub properties: BTreeMap<String, PropertyValue>,
    /// Categorical labels for filtering and retrieval.
    pub tags: Vec<String>,
    /// Retrieval priority weight in [0.0, 1.0]; higher values surface the note sooner.
    pub salience: Option<f64>,
    /// Exponential decay rate applied to salience over time; 0.0 means no decay.
    pub decay_factor: Option<f64>,
    /// Optional expiry timestamp after which the note is treated as inactive.
    pub expires_at: Option<Timestamp>,
    /// Set when the note is soft-deleted; absent means active.
    pub deleted_at: Option<Timestamp>,
}

impl Note {
    /// Return `true` if all numeric fields carry finite, domain-valid values.
    ///
    /// - `salience`, if present, must be finite and in `[0.0, 1.0]`.
    /// - `decay_factor`, if present, must be finite and non-negative.
    pub fn is_valid(&self) -> bool {
        let salience_ok = self
            .salience
            .map(|s| s.is_finite() && (0.0..=1.0).contains(&s))
            .unwrap_or(true);
        let decay_ok = self
            .decay_factor
            .map(|d| d.is_finite() && d >= 0.0)
            .unwrap_or(true);
        salience_ok && decay_ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Id128, Namespace};

    fn test_header() -> Header {
        Header::new(
            Id128::from_u128(1),
            Namespace::local(),
            Timestamp::from_secs(1700000000),
        )
    }

    #[test]
    fn note_construction() {
        let note = Note {
            header: test_header(),
            kind: String::from("decision"),
            status: NoteStatus::Active,
            content: String::from("Use BGE-base for multilingual corpus"),
            properties: BTreeMap::new(),
            tags: alloc::vec!["retrieval".into()],
            salience: Some(0.8),
            decay_factor: Some(0.01),
            expires_at: None,
            deleted_at: None,
        };
        assert_eq!(note.kind, "decision");
        assert_eq!(note.tags.len(), 1);
    }

    #[test]
    fn note_construction_uses_pack_owned_kind_string() {
        let note = Note {
            header: test_header(),
            kind: String::from("decision"),
            status: NoteStatus::Active,
            content: String::from("test"),
            properties: BTreeMap::new(),
            tags: alloc::vec![],
            salience: None,
            decay_factor: None,
            expires_at: None,
            deleted_at: None,
        };
        assert_eq!(note.kind, "decision");
    }

    #[test]
    fn note_status_deleted_roundtrip() {
        use core::str::FromStr;
        assert_eq!(
            NoteStatus::from_str("deleted").unwrap(),
            NoteStatus::Deleted
        );
        assert_eq!(NoteStatus::Deleted.name(), "deleted");
    }
}
