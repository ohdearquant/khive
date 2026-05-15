//! Entity substrate — graph nodes with typed properties and links.

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;

use crate::{EdgeRelation, Header, Id128};

/// Taxonomy for entity classification in a research knowledge graph (ADR-001).
///
/// 6 kinds, chosen for agent reliability: agents classify these correctly
/// with unambiguous signals. Finer distinctions (algorithm vs technique,
/// model vs architecture) live in `properties` — they don't enable useful
/// queries with the 13-relation edge ontology and cause 20-30% misclassification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EntityKind {
    /// Algorithms, techniques, architectures, theories, models, research gaps.
    /// The default / residual bucket. Use `properties.type` for finer grain.
    #[default]
    Concept,
    /// Papers, preprints, technical reports, blog posts, books.
    /// Has: title, authors, year, venue, DOI/URL.
    Document,
    /// Benchmarks, corpora, evaluation sets.
    /// Has: task type, size, metrics, license.
    Dataset,
    /// Codebases, libraries, tools, frameworks.
    /// Has: language, repo URL, license.
    Project,
    /// Researchers, engineers, authors.
    Person,
    /// Labs, companies, institutions.
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

impl FromStr for EntityKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "concept" => Ok(Self::Concept),
            "document" | "doc" | "paper" => Ok(Self::Document),
            "dataset" | "data" | "benchmark" => Ok(Self::Dataset),
            "project" | "repo" | "crate" | "library" | "lib" => Ok(Self::Project),
            "person" | "author" | "researcher" => Ok(Self::Person),
            "org" | "organization" | "organisation" | "lab" | "company" => Ok(Self::Org),
            other => Err(alloc::format!(
                "unknown entity kind: {other:?}. Valid: concept | document | dataset | project | person | org"
            )),
        }
    }
}

/// A graph node with a type, display name, and key-value properties.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Entity {
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub header: Header,
    pub kind: EntityKind,
    pub name: String,
    pub description: Option<String>,
    pub properties: BTreeMap<String, PropertyValue>,
    pub tags: Vec<String>,
}

/// A directed, typed edge between two entities (or cross-substrate nodes).
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Link {
    pub id: Id128,
    pub source: Id128,
    pub target: Id128,
    pub relation: EdgeRelation,
    pub properties: BTreeMap<String, PropertyValue>,
    pub weight: f64,
}

/// Property values stored on entities, links, and notes.
///
/// Recursive: supports arrays and nested objects for free-form JSON properties
/// (e.g. `entity_ids[]`, `alternatives_considered[]` per ADR-019).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum PropertyValue {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Array(Vec<PropertyValue>),
    Object(BTreeMap<String, PropertyValue>),
    Null,
}

impl fmt::Display for PropertyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => f.write_str(s),
            Self::Integer(n) => write!(f, "{n}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::Boolean(b) => write!(f, "{b}"),
            Self::Array(arr) => write!(f, "[{} items]", arr.len()),
            Self::Object(obj) => write!(f, "{{{} keys}}", obj.len()),
            Self::Null => f.write_str("null"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Namespace, Timestamp};

    #[test]
    fn entity_with_properties() {
        let mut props = BTreeMap::new();
        props.insert("role".into(), PropertyValue::String("engineer".into()));
        props.insert("age".into(), PropertyValue::Integer(30));

        let entity = Entity {
            header: Header::new(
                Id128::from_u128(1),
                Namespace::default(),
                Timestamp::from_secs(1700000000),
            ),
            kind: EntityKind::Person,
            name: "Ocean".into(),
            description: None,
            properties: props,
            tags: alloc::vec![],
        };
        assert_eq!(entity.kind, EntityKind::Person);
        assert_eq!(entity.kind.name(), "person");
        assert_eq!(entity.properties.len(), 2);
    }

    #[test]
    fn entity_kind_default_is_concept() {
        assert_eq!(EntityKind::default(), EntityKind::Concept);
    }

    #[test]
    fn entity_kind_display_roundtrip() {
        for kind in EntityKind::ALL {
            let s = alloc::format!("{kind}");
            let parsed = EntityKind::from_str(&s).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn entity_kind_from_str_aliases() {
        assert_eq!(EntityKind::from_str("doc").unwrap(), EntityKind::Document);
        assert_eq!(EntityKind::from_str("paper").unwrap(), EntityKind::Document);
        assert_eq!(
            EntityKind::from_str("benchmark").unwrap(),
            EntityKind::Dataset
        );
        assert_eq!(EntityKind::from_str("repo").unwrap(), EntityKind::Project);
        assert_eq!(EntityKind::from_str("author").unwrap(), EntityKind::Person);
        assert_eq!(EntityKind::from_str("lab").unwrap(), EntityKind::Org);
    }

    #[test]
    fn entity_kind_from_str_case_insensitive() {
        assert_eq!(
            EntityKind::from_str("CONCEPT").unwrap(),
            EntityKind::Concept
        );
        assert_eq!(EntityKind::from_str("Person").unwrap(), EntityKind::Person);
    }

    #[test]
    fn entity_kind_from_str_unknown_errors() {
        let err = EntityKind::from_str("gadget").unwrap_err();
        assert!(err.contains("unknown entity kind"));
    }

    #[test]
    fn link_construction() {
        let link = Link {
            id: Id128::from_u128(100),
            source: Id128::from_u128(1),
            target: Id128::from_u128(2),
            relation: EdgeRelation::Extends,
            properties: BTreeMap::new(),
            weight: 1.0,
        };
        assert_eq!(link.relation, EdgeRelation::Extends);
    }
}
