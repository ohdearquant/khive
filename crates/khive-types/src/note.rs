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
///
/// When present, `salience` must be finite and in `[0.0, 1.0]`, and
/// `decay_factor` must be finite and non-negative. When the `serde` feature
/// is enabled, deserialization rejects out-of-range values.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
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

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Note {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct NoteRaw {
            #[serde(flatten)]
            header: Header,
            kind: String,
            status: NoteStatus,
            content: String,
            properties: BTreeMap<String, PropertyValue>,
            tags: Vec<String>,
            salience: Option<f64>,
            decay_factor: Option<f64>,
            expires_at: Option<Timestamp>,
            deleted_at: Option<Timestamp>,
        }

        let raw = NoteRaw::deserialize(deserializer)?;

        if let Some(s) = raw.salience {
            if !s.is_finite() {
                return Err(serde::de::Error::custom(alloc::format!(
                    "Note salience must be finite, got {s}"
                )));
            }
            if !(0.0..=1.0).contains(&s) {
                return Err(serde::de::Error::custom(alloc::format!(
                    "Note salience must be in [0.0, 1.0], got {s}"
                )));
            }
        }
        if let Some(d) = raw.decay_factor {
            if !d.is_finite() {
                return Err(serde::de::Error::custom(alloc::format!(
                    "Note decay_factor must be finite, got {d}"
                )));
            }
            if d < 0.0 {
                return Err(serde::de::Error::custom(alloc::format!(
                    "Note decay_factor must be non-negative, got {d}"
                )));
            }
        }

        Ok(Note {
            header: raw.header,
            kind: raw.kind,
            status: raw.status,
            content: raw.content,
            properties: raw.properties,
            tags: raw.tags,
            salience: raw.salience,
            decay_factor: raw.decay_factor,
            expires_at: raw.expires_at,
            deleted_at: raw.deleted_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Id128, Namespace};
    #[cfg(feature = "serde")]
    use alloc::string::ToString;

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

    #[test]
    fn note_is_valid_checks_salience_range() {
        let mut note = Note {
            header: test_header(),
            kind: String::from("observation"),
            status: NoteStatus::Active,
            content: String::from("test"),
            properties: BTreeMap::new(),
            tags: alloc::vec![],
            salience: Some(1.5),
            decay_factor: None,
            expires_at: None,
            deleted_at: None,
        };
        assert!(!note.is_valid());
        note.salience = Some(0.5);
        assert!(note.is_valid());
    }

    #[test]
    fn note_is_valid_checks_decay_non_negative() {
        let mut note = Note {
            header: test_header(),
            kind: String::from("observation"),
            status: NoteStatus::Active,
            content: String::from("test"),
            properties: BTreeMap::new(),
            tags: alloc::vec![],
            salience: None,
            decay_factor: Some(-0.1),
            expires_at: None,
            deleted_at: None,
        };
        assert!(!note.is_valid());
        note.decay_factor = Some(0.01);
        assert!(note.is_valid());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn note_serde_rejects_salience_above_one() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "namespace": "local",
            "created_at": 1700000000000000_u64,
            "updated_at": 1700000000000000_u64,
            "kind": "observation",
            "status": "active",
            "content": "test",
            "properties": {},
            "tags": [],
            "salience": 1.5,
            "decay_factor": null,
            "expires_at": null,
            "deleted_at": null
        });
        let result: Result<Note, _> = serde_json::from_value(json);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("[0.0, 1.0]"),
            "error should mention range: {err}"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn note_serde_rejects_negative_decay() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "namespace": "local",
            "created_at": 1700000000000000_u64,
            "updated_at": 1700000000000000_u64,
            "kind": "observation",
            "status": "active",
            "content": "test",
            "properties": {},
            "tags": [],
            "salience": null,
            "decay_factor": -0.5,
            "expires_at": null,
            "deleted_at": null
        });
        let result: Result<Note, _> = serde_json::from_value(json);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-negative"),
            "error should mention non-negative: {err}"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn note_serde_accepts_valid_values() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "namespace": "local",
            "created_at": 1700000000000000_u64,
            "updated_at": 1700000000000000_u64,
            "kind": "decision",
            "status": "active",
            "content": "test content",
            "properties": {},
            "tags": ["tag1"],
            "salience": 0.8,
            "decay_factor": 0.01,
            "expires_at": null,
            "deleted_at": null
        });
        let note: Note = serde_json::from_value(json).expect("valid note should deserialize");
        assert_eq!(note.salience, Some(0.8));
        assert_eq!(note.decay_factor, Some(0.01));
    }
}
