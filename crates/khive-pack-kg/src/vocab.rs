//! KG-pack vocabulary — pack-owned entity and note vocabulary.
//!
//! Entity kind validation now uses `khive_types::EntityKind` directly.
//! The runtime accepts any String — validation is the pack's responsibility.

use core::fmt;
use std::string::String;

use khive_types::UnknownVariant;

/// Closed taxonomy for entity classification (ADR-001).
///
/// `Resource` is the 9th kind added in ADR-048: actionable content agents
/// consume — atoms, domains, skills, tools. Distinct from `Concept` which
/// models abstract ideas and their graph relationships.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EntityKind {
    #[default]
    Concept,
    Document,
    Dataset,
    Project,
    Person,
    Org,
    Artifact,
    Service,
    /// Actionable content agents consume (ADR-048): atoms, domains, skills,
    /// tools, templates, prompts, runbooks.
    Resource,
}

impl EntityKind {
    pub const ALL: [Self; 9] = [
        Self::Concept,
        Self::Document,
        Self::Dataset,
        Self::Project,
        Self::Person,
        Self::Org,
        Self::Artifact,
        Self::Service,
        Self::Resource,
    ];

    pub const NAMES: &'static [&'static str] = &[
        "concept", "document", "dataset", "project", "person", "org", "artifact", "service",
        "resource",
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Concept => "concept",
            Self::Document => "document",
            Self::Dataset => "dataset",
            Self::Project => "project",
            Self::Person => "person",
            Self::Org => "org",
            Self::Artifact => "artifact",
            Self::Service => "service",
            Self::Resource => "resource",
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
            "artifact" => Ok(Self::Artifact),
            "service" => Ok(Self::Service),
            "resource" | "atom" | "runbook" | "template" | "prompt" | "skill" | "tool" => {
                Ok(Self::Resource)
            }
            other => Err(UnknownVariant::new("entity_kind", other, Self::NAMES)),
        }
    }
}

/// KG pack note kinds. Public note kind validation is canonical-only per ADR-013.
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
            "observation" => Ok(Self::Observation),
            "insight" => Ok(Self::Insight),
            "question" => Ok(Self::Question),
            "decision" => Ok(Self::Decision),
            "reference" => Ok(Self::Reference),
            other => Err(UnknownVariant::new("note_kind", other, Self::NAMES)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn note_kind_roundtrip() {
        for kind in NoteKind::ALL {
            let parsed = NoteKind::from_str(kind.name()).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn note_kind_aliases_rejected() {
        // Aliases were removed per ADR-013 — only canonical names are accepted.
        assert!(NoteKind::from_str("obs").is_err());
        assert!(NoteKind::from_str("finding").is_err());
        assert!(NoteKind::from_str("q").is_err());
        assert!(NoteKind::from_str("choice").is_err());
        assert!(NoteKind::from_str("ref").is_err());
        assert!(NoteKind::from_str("citation").is_err());
    }

    #[test]
    fn entity_kind_roundtrip_all() {
        for kind in EntityKind::ALL {
            let parsed = EntityKind::from_str(kind.name()).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn entity_kind_resource_aliases() {
        // ADR-048: aliases for resource entity_type values.
        for alias in ["atom", "runbook", "template", "prompt", "skill", "tool"] {
            let parsed = EntityKind::from_str(alias).unwrap();
            assert_eq!(parsed, EntityKind::Resource, "alias {alias:?} must map to Resource");
        }
    }

    #[test]
    fn entity_kind_names_length() {
        assert_eq!(EntityKind::ALL.len(), EntityKind::NAMES.len());
        assert_eq!(EntityKind::ALL.len(), 9);
    }
}
