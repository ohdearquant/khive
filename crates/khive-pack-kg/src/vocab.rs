//! KG-pack vocabulary — closed enums for the 6 entity kinds and 5 note kinds.
//!
//! These enums validate and canonicalize kind strings at the pack boundary.
//! The runtime accepts any String — validation is the pack's responsibility.

use core::fmt;
use std::string::String;

use khive_types::UnknownVariant;

/// Closed taxonomy for entity classification (ADR-001).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EntityKind {
    #[default]
    Concept,
    Document,
    Dataset,
    Project,
    Person,
    Org,
}

impl EntityKind {
    pub const ALL: [Self; 6] = [
        Self::Concept,
        Self::Document,
        Self::Dataset,
        Self::Project,
        Self::Person,
        Self::Org,
    ];

    pub const NAMES: &'static [&'static str] =
        &["concept", "document", "dataset", "project", "person", "org"];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Concept => "concept",
            Self::Document => "document",
            Self::Dataset => "dataset",
            Self::Project => "project",
            Self::Person => "person",
            Self::Org => "org",
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl From<EntityKind> for String {
    fn from(k: EntityKind) -> Self {
        String::from(k.name())
    }
}

impl std::str::FromStr for EntityKind {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "concept" => Ok(Self::Concept),
            "document" | "doc" | "paper" => Ok(Self::Document),
            "dataset" | "data" | "benchmark" => Ok(Self::Dataset),
            "project" | "repo" | "crate" | "library" | "lib" => Ok(Self::Project),
            "person" | "author" | "researcher" => Ok(Self::Person),
            "org" | "organization" | "organisation" | "lab" | "company" => Ok(Self::Org),
            other => Err(UnknownVariant::new("entity_kind", other, Self::NAMES)),
        }
    }
}

/// Closed taxonomy for note classification (ADR-019).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NoteKind {
    #[default]
    Observation,
    Insight,
    Question,
    Decision,
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

    pub const NAMES: &'static [&'static str] = &[
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
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

impl From<NoteKind> for String {
    fn from(k: NoteKind) -> Self {
        String::from(k.name())
    }
}

impl std::str::FromStr for NoteKind {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "observation" | "obs" => Ok(Self::Observation),
            "insight" | "finding" => Ok(Self::Insight),
            "question" | "q" => Ok(Self::Question),
            "decision" | "choice" => Ok(Self::Decision),
            "reference" | "ref" | "citation" => Ok(Self::Reference),
            other => Err(UnknownVariant::new("note_kind", other, Self::NAMES)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn entity_kind_roundtrip() {
        for kind in EntityKind::ALL {
            let parsed = EntityKind::from_str(kind.name()).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn entity_kind_aliases() {
        assert_eq!(EntityKind::from_str("paper").unwrap(), EntityKind::Document);
        assert_eq!(EntityKind::from_str("repo").unwrap(), EntityKind::Project);
        assert_eq!(EntityKind::from_str("lab").unwrap(), EntityKind::Org);
    }

    #[test]
    fn entity_kind_unknown_errors_with_valid_list() {
        let err = EntityKind::from_str("gadget").unwrap_err();
        assert_eq!(err.domain, "entity_kind");
        assert!(err.valid.contains(&"concept"));
    }

    #[test]
    fn note_kind_roundtrip() {
        for kind in NoteKind::ALL {
            let parsed = NoteKind::from_str(kind.name()).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn note_kind_aliases() {
        assert_eq!(NoteKind::from_str("obs").unwrap(), NoteKind::Observation);
        assert_eq!(NoteKind::from_str("ref").unwrap(), NoteKind::Reference);
    }
}
