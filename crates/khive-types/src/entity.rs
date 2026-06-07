//! Entity substrate — graph nodes with typed properties and links.

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;

use crate::{EdgeRelation, Header, Id128, Timestamp};

/// 8 closed base kinds for graph-node classification.
///
/// Governed subtype values live in `Entity::entity_type`; `properties` remain
/// metadata and must not carry ontology type strings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EntityKind {
    /// Algorithms, techniques, architectures, theories, models, research gaps.
    /// The default / residual bucket.
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
    /// Built artifacts: binaries, model checkpoints, Docker images, packages.
    Artifact,
    /// Running or deployable services: APIs, hosted endpoints, SaaS products.
    Service,
}

impl EntityKind {
    /// All 8 canonical entity kinds in taxonomy-table order.
    pub const ALL: [Self; 8] = [
        Self::Concept,
        Self::Document,
        Self::Dataset,
        Self::Project,
        Self::Person,
        Self::Org,
        Self::Artifact,
        Self::Service,
    ];

    /// Return the canonical lowercase string for this kind, as stored on the wire.
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
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// Canonical entity kind strings for the closed 8-kind taxonomy.
const ENTITY_KIND_VALID: &[&str] = &[
    "concept", "document", "dataset", "project", "person", "org", "artifact", "service",
];

impl FromStr for EntityKind {
    type Err = crate::error::UnknownVariant;

    /// Parse a string into an `EntityKind`.
    ///
    /// Accepts the 8 canonical kind names (case-insensitive) plus a set of
    /// convenience aliases to aid human-authored DSL requests (e.g. `"paper"`
    /// resolves to `Document`, `"repo"` to `Project`).
    ///
    /// **Note on subtype aliasing**: when `kind="paper"` is parsed here, only the
    /// base `EntityKind::Document` is returned.  Callers that need to preserve the
    /// `entity_type` subtoken must use the pack registry resolution path, which
    /// returns both the base kind and the subtype string.  `from_str` is
    /// intentionally base-kind-only for use in contexts where the subtype is
    /// carried separately (e.g. `Entity.entity_type`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "concept" => Ok(Self::Concept),
            "document" | "doc" | "paper" => Ok(Self::Document),
            "dataset" | "data" | "benchmark" => Ok(Self::Dataset),
            "project" | "repo" | "crate" | "library" | "lib" => Ok(Self::Project),
            "person" | "author" | "researcher" => Ok(Self::Person),
            "org" | "organization" | "organisation" | "lab" | "company" => Ok(Self::Org),
            "artifact" | "art" => Ok(Self::Artifact),
            "service" | "svc" => Ok(Self::Service),
            other => Err(crate::error::UnknownVariant::new(
                "entity_kind",
                other,
                ENTITY_KIND_VALID,
            )),
        }
    }
}

/// A graph node with a type, display name, and key-value properties.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Entity {
    /// Identity and namespace metadata shared by all substrate records.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub header: Header,
    /// Closed base kind that classifies this entity.
    pub kind: EntityKind,
    /// Pack-governed subtype token (e.g. `"paper"`, `"snapshot"`). Never stored
    /// raw in `properties` — queries compile this to `entities.entity_type = ?`.
    pub entity_type: Option<String>,
    /// Human-readable display name (required; must be non-empty).
    pub name: String,
    /// Optional long-form description of this entity.
    pub description: Option<String>,
    /// Arbitrary structured metadata as key-value pairs.
    pub properties: BTreeMap<String, PropertyValue>,
    /// Categorical labels for filtering and retrieval.
    pub tags: Vec<String>,
    /// Set when the entity is soft-deleted; absent means active.
    pub deleted_at: Option<Timestamp>,
}

/// A directed, typed edge between two entities (or cross-substrate nodes).
///
/// `weight` must be finite and in `[0.0, 1.0]`. When the `serde` feature is
/// enabled, deserialization rejects out-of-range or non-finite weights.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(into = "LinkRaw"))]
pub struct Link {
    /// Unique edge identifier.
    pub id: Id128,
    /// Namespace that owns and isolates this edge.
    pub namespace: String,
    /// Source node identifier.
    pub source: Id128,
    /// Target node identifier.
    pub target: Id128,
    /// Closed relation type that semantically describes this edge.
    pub relation: EdgeRelation,
    /// Arbitrary structured metadata attached to this edge.
    pub properties: BTreeMap<String, PropertyValue>,
    /// Numeric edge weight in the range [0.0, 1.0]; 1.0 means definitional strength.
    pub weight: f64,
    /// Wall-clock time when this edge was created.
    pub created_at: Timestamp,
    /// Wall-clock time of the most recent update.
    pub updated_at: Timestamp,
    /// Set when the edge is soft-deleted; absent means active.
    pub deleted_at: Option<Timestamp>,
}

impl Link {
    /// Return `true` if all numeric fields carry finite, domain-valid values.
    ///
    /// - `weight` must be finite and in `[0.0, 1.0]`.
    pub fn is_valid(&self) -> bool {
        self.weight.is_finite() && self.weight >= 0.0 && self.weight <= 1.0
    }
}

#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct LinkRaw {
    id: Id128,
    namespace: String,
    source: Id128,
    target: Id128,
    relation: EdgeRelation,
    properties: BTreeMap<String, PropertyValue>,
    weight: f64,
    created_at: Timestamp,
    updated_at: Timestamp,
    deleted_at: Option<Timestamp>,
}

#[cfg(feature = "serde")]
impl From<Link> for LinkRaw {
    fn from(l: Link) -> Self {
        Self {
            id: l.id,
            namespace: l.namespace,
            source: l.source,
            target: l.target,
            relation: l.relation,
            properties: l.properties,
            weight: l.weight,
            created_at: l.created_at,
            updated_at: l.updated_at,
            deleted_at: l.deleted_at,
        }
    }
}

#[cfg(feature = "serde")]
impl TryFrom<LinkRaw> for Link {
    type Error = String;

    fn try_from(raw: LinkRaw) -> Result<Self, Self::Error> {
        if !raw.weight.is_finite() {
            return Err(alloc::format!(
                "Link weight must be finite, got {}",
                raw.weight
            ));
        }
        if !(0.0..=1.0).contains(&raw.weight) {
            return Err(alloc::format!(
                "Link weight must be in [0.0, 1.0], got {}",
                raw.weight
            ));
        }
        Ok(Link {
            id: raw.id,
            namespace: raw.namespace,
            source: raw.source,
            target: raw.target,
            relation: raw.relation,
            properties: raw.properties,
            weight: raw.weight,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
            deleted_at: raw.deleted_at,
        })
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Link {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = LinkRaw::deserialize(deserializer)?;
        Link::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Property values stored on entities, links, and notes.
///
/// Recursive: supports arrays and nested objects for free-form JSON properties
/// (e.g. `entity_ids[]`, `alternatives_considered[]`).
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
    #[cfg(feature = "serde")]
    use alloc::string::ToString;

    #[test]
    fn entity_with_properties() {
        let mut props = BTreeMap::new();
        props.insert("role".into(), PropertyValue::String("engineer".into()));
        props.insert("age".into(), PropertyValue::Integer(30));

        let entity = Entity {
            header: Header::new(
                Id128::from_u128(1),
                Namespace::local(),
                Timestamp::from_secs(1700000000),
            ),
            kind: EntityKind::Person,
            entity_type: Some("researcher".into()),
            name: "Ocean".into(),
            description: None,
            properties: props,
            tags: alloc::vec![],
            deleted_at: None,
        };
        assert_eq!(entity.kind, EntityKind::Person);
        assert_eq!(entity.kind.name(), "person");
        assert_eq!(entity.entity_type.as_deref(), Some("researcher"));
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
        assert_eq!(EntityKind::from_str("art").unwrap(), EntityKind::Artifact);
        assert_eq!(EntityKind::from_str("svc").unwrap(), EntityKind::Service);
    }

    #[test]
    fn entity_kind_artifact_and_service_roundtrip() {
        assert_eq!(EntityKind::Artifact.name(), "artifact");
        assert_eq!(EntityKind::Service.name(), "service");
        assert_eq!(
            EntityKind::from_str("artifact").unwrap(),
            EntityKind::Artifact
        );
        assert_eq!(
            EntityKind::from_str("service").unwrap(),
            EntityKind::Service
        );
    }

    #[test]
    fn entity_kind_all_has_eight_variants() {
        assert_eq!(EntityKind::ALL.len(), 8);
        assert!(EntityKind::ALL.contains(&EntityKind::Artifact));
        assert!(EntityKind::ALL.contains(&EntityKind::Service));
    }

    #[test]
    fn entity_kind_unknown_valid_list_includes_new_kinds() {
        let err = EntityKind::from_str("gadget").unwrap_err();
        assert!(err.valid.contains(&"artifact"));
        assert!(err.valid.contains(&"service"));
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
        assert_eq!(err.domain, "entity_kind");
        assert_eq!(err.value, "gadget");
        assert!(err.valid.contains(&"concept"));
    }

    #[test]
    fn link_construction() {
        let ts = Timestamp::from_secs(1700000000);
        let link = Link {
            id: Id128::from_u128(100),
            namespace: "default".into(),
            source: Id128::from_u128(1),
            target: Id128::from_u128(2),
            relation: EdgeRelation::Extends,
            properties: BTreeMap::new(),
            weight: 1.0,
            created_at: ts,
            updated_at: ts,
            deleted_at: None,
        };
        assert_eq!(link.relation, EdgeRelation::Extends);
        assert!(link.is_valid());
    }

    #[test]
    fn link_is_valid_rejects_out_of_range() {
        let ts = Timestamp::from_secs(1700000000);
        let link = Link {
            id: Id128::from_u128(100),
            namespace: "default".into(),
            source: Id128::from_u128(1),
            target: Id128::from_u128(2),
            relation: EdgeRelation::Extends,
            properties: BTreeMap::new(),
            weight: 2.0,
            created_at: ts,
            updated_at: ts,
            deleted_at: None,
        };
        assert!(!link.is_valid());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn link_serde_rejects_weight_above_one() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000064",
            "namespace": "default",
            "source": "00000000-0000-0000-0000-000000000001",
            "target": "00000000-0000-0000-0000-000000000002",
            "relation": "extends",
            "properties": {},
            "weight": 2.0,
            "created_at": 1700000000000000_u64,
            "updated_at": 1700000000000000_u64,
            "deleted_at": null
        });
        let result: Result<Link, _> = serde_json::from_value(json);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("[0.0, 1.0]"),
            "error should mention range: {err}"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn link_serde_rejects_negative_weight() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000064",
            "namespace": "default",
            "source": "00000000-0000-0000-0000-000000000001",
            "target": "00000000-0000-0000-0000-000000000002",
            "relation": "extends",
            "properties": {},
            "weight": -0.1,
            "created_at": 1700000000000000_u64,
            "updated_at": 1700000000000000_u64,
            "deleted_at": null
        });
        let result: Result<Link, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn link_serde_accepts_valid_weight() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000064",
            "namespace": "default",
            "source": "00000000-0000-0000-0000-000000000001",
            "target": "00000000-0000-0000-0000-000000000002",
            "relation": "extends",
            "properties": {},
            "weight": 0.75,
            "created_at": 1700000000000000_u64,
            "updated_at": 1700000000000000_u64,
            "deleted_at": null
        });
        let link: Link = serde_json::from_value(json).expect("valid weight should deserialize");
        assert_eq!(link.weight, 0.75);
    }
}
