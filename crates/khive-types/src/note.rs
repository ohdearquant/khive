//! Note substrate — temporal-referential records (ADR-004, ADR-019).

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::entity::PropertyValue;
use crate::{Header, Timestamp};

/// Closed taxonomy for note classification (ADR-019).
///
/// 5 kinds covering the cognitive functions an agent performs while researching.
/// Closed and exhaustive — adding a sixth requires a new ADR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum NoteKind {
    /// An empirical capture — what was noticed or measured.
    #[default]
    Observation,
    /// An analytical or synthetic conclusion drawn from observations.
    Insight,
    /// An open inquiry, research direction, or unknown.
    Question,
    /// A committed choice with rationale.
    Decision,
    /// An external pointer with context (paper, URL, citation note).
    Reference,
}

impl NoteKind {
    pub const ALL: [Self; 5] = [
        Self::Observation,
        Self::Insight,
        Self::Question,
        Self::Decision,
        Self::Reference,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::Insight => "insight",
            Self::Question => "question",
            Self::Decision => "decision",
            Self::Reference => "reference",
        }
    }
}

impl fmt::Display for NoteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl core::str::FromStr for NoteKind {
    type Err = alloc::string::String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "observation" | "obs" => Ok(Self::Observation),
            "insight" | "finding" => Ok(Self::Insight),
            "question" | "q" => Ok(Self::Question),
            "decision" | "choice" => Ok(Self::Decision),
            "reference" | "ref" | "citation" => Ok(Self::Reference),
            other => Err(alloc::format!(
                "unknown note kind: {other:?}. Valid: observation | insight | question | decision | reference"
            )),
        }
    }
}

/// Lifecycle status of a note. Cross-cutting across all note kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum NoteStatus {
    #[default]
    Active,
    Archived,
}

impl NoteStatus {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }
}

impl fmt::Display for NoteStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A note record — temporal-referential content plus free-form properties.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Note {
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub header: Header,
    pub kind: NoteKind,
    pub status: NoteStatus,
    pub content: String,
    pub properties: BTreeMap<String, PropertyValue>,
    pub tags: Vec<String>,
    pub salience: f64,
    pub decay_factor: f64,
    pub expires_at: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Id128, Namespace};

    fn test_header() -> Header {
        Header::new(
            Id128::from_u128(1),
            Namespace::default(),
            Timestamp::from_secs(1700000000),
        )
    }

    #[test]
    fn note_kind_all_have_names() {
        for kind in NoteKind::ALL {
            assert!(!kind.name().is_empty());
        }
    }

    #[test]
    fn note_kind_default_is_observation() {
        assert_eq!(NoteKind::default(), NoteKind::Observation);
    }

    #[test]
    fn note_kind_display_roundtrip() {
        use core::str::FromStr;
        for kind in NoteKind::ALL {
            let s = alloc::format!("{kind}");
            let parsed = NoteKind::from_str(&s).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn note_kind_from_str_case_insensitive() {
        use core::str::FromStr;
        assert_eq!(
            NoteKind::from_str("OBSERVATION").unwrap(),
            NoteKind::Observation
        );
        assert_eq!(NoteKind::from_str("Insight").unwrap(), NoteKind::Insight);
    }

    #[test]
    fn note_kind_from_str_aliases() {
        use core::str::FromStr;
        assert_eq!(NoteKind::from_str("obs").unwrap(), NoteKind::Observation);
        assert_eq!(NoteKind::from_str("finding").unwrap(), NoteKind::Insight);
        assert_eq!(NoteKind::from_str("q").unwrap(), NoteKind::Question);
        assert_eq!(NoteKind::from_str("choice").unwrap(), NoteKind::Decision);
        assert_eq!(NoteKind::from_str("ref").unwrap(), NoteKind::Reference);
        assert_eq!(NoteKind::from_str("citation").unwrap(), NoteKind::Reference);
    }

    #[test]
    fn note_kind_from_str_unknown_errors() {
        use core::str::FromStr;
        let err = NoteKind::from_str("garbage").unwrap_err();
        assert!(err.contains("unknown note kind"));
        assert!(err.contains("observation"));
    }

    #[test]
    fn note_construction() {
        let note = Note {
            header: test_header(),
            kind: NoteKind::Decision,
            status: NoteStatus::Active,
            content: String::from("Use BGE-base for multilingual corpus"),
            properties: BTreeMap::new(),
            tags: alloc::vec!["retrieval".into()],
            salience: 0.8,
            decay_factor: 0.01,
            expires_at: None,
        };
        assert_eq!(note.kind, NoteKind::Decision);
        assert_eq!(note.tags.len(), 1);
    }
}
