//! Edge relation types for the closed ontology defined in ADR-002 / ADR-021.

extern crate alloc;
use alloc::string::String;
use core::fmt;
use core::str::FromStr;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The 6 structural categories that group the 13 canonical edge relations.
///
/// Exposed via [`EdgeRelation::category`] for query planners and UI rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EdgeCategory {
    /// Composition: `contains`, `part_of`, `instance_of`
    Structure,
    /// Intellectual lineage: `extends`, `variant_of`, `introduced_by`, `supersedes`
    Derivation,
    /// Build/runtime needs: `depends_on`, `enables`
    Dependency,
    /// Code ↔ concept: `implements`
    Implementation,
    /// Peer relationships: `competes_with`, `composed_with`
    Lateral,
    /// Cross-substrate annotation: `annotates`
    Annotation,
}

/// Closed set of 13 canonical edge relations (ADR-002, ADR-021).
///
/// No `Default` — every edge requires an explicit relation.
/// Wire format: snake_case strings (e.g. `"part_of"`, `"introduced_by"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EdgeRelation {
    // Structure
    Contains,
    PartOf,
    InstanceOf,
    // Derivation
    Extends,
    VariantOf,
    IntroducedBy,
    Supersedes,
    // Dependency
    DependsOn,
    Enables,
    // Implementation
    Implements,
    // Lateral
    CompetesWith,
    ComposedWith,
    // Annotation
    Annotates,
}

impl EdgeRelation {
    /// All 13 canonical relations in ADR-002 table order.
    pub const ALL: [Self; 13] = [
        Self::Contains,
        Self::PartOf,
        Self::InstanceOf,
        Self::Extends,
        Self::VariantOf,
        Self::IntroducedBy,
        Self::Supersedes,
        Self::DependsOn,
        Self::Enables,
        Self::Implements,
        Self::CompetesWith,
        Self::ComposedWith,
        Self::Annotates,
    ];

    /// The category this relation belongs to.
    pub const fn category(&self) -> EdgeCategory {
        match self {
            Self::Contains | Self::PartOf | Self::InstanceOf => EdgeCategory::Structure,
            Self::Extends | Self::VariantOf | Self::IntroducedBy | Self::Supersedes => {
                EdgeCategory::Derivation
            }
            Self::DependsOn | Self::Enables => EdgeCategory::Dependency,
            Self::Implements => EdgeCategory::Implementation,
            Self::CompetesWith | Self::ComposedWith => EdgeCategory::Lateral,
            Self::Annotates => EdgeCategory::Annotation,
        }
    }

    /// Canonical snake_case name as stored in the database.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::PartOf => "part_of",
            Self::InstanceOf => "instance_of",
            Self::Extends => "extends",
            Self::VariantOf => "variant_of",
            Self::IntroducedBy => "introduced_by",
            Self::Supersedes => "supersedes",
            Self::DependsOn => "depends_on",
            Self::Enables => "enables",
            Self::Implements => "implements",
            Self::CompetesWith => "competes_with",
            Self::ComposedWith => "composed_with",
            Self::Annotates => "annotates",
        }
    }
}

impl fmt::Display for EdgeRelation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

const EDGE_RELATION_VALID: &[&str] = &[
    "contains", "part_of", "instance_of", "extends", "variant_of", "introduced_by",
    "supersedes", "depends_on", "enables", "implements", "competes_with", "composed_with",
    "annotates",
];

impl FromStr for EdgeRelation {
    type Err = crate::error::UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalised: String = s
            .chars()
            .map(|c| {
                if c == '-' {
                    '_'
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();

        match normalised.as_str() {
            "contains" => Ok(Self::Contains),
            "part_of" | "partof" => Ok(Self::PartOf),
            "instance_of" | "instanceof" => Ok(Self::InstanceOf),
            "extends" => Ok(Self::Extends),
            "variant_of" | "variantof" => Ok(Self::VariantOf),
            "introduced_by" | "introducedby" => Ok(Self::IntroducedBy),
            "supersedes" => Ok(Self::Supersedes),
            "depends_on" | "dependson" => Ok(Self::DependsOn),
            "enables" => Ok(Self::Enables),
            "implements" => Ok(Self::Implements),
            "competes_with" | "competeswith" => Ok(Self::CompetesWith),
            "composed_with" | "composedwith" => Ok(Self::ComposedWith),
            "annotates" => Ok(Self::Annotates),
            _ => Err(crate::error::UnknownVariant::new("edge_relation", s, EDGE_RELATION_VALID)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn all_has_thirteen_variants() {
        assert_eq!(EdgeRelation::ALL.len(), 13);
    }

    #[test]
    fn display_roundtrip_for_all() {
        for relation in EdgeRelation::ALL {
            let s = relation.to_string();
            let parsed: EdgeRelation = s.parse().expect("display output should re-parse");
            assert_eq!(parsed, relation);
        }
    }

    #[test]
    fn from_str_case_insensitive() {
        assert_eq!(
            "Extends".parse::<EdgeRelation>().unwrap(),
            EdgeRelation::Extends
        );
        assert_eq!(
            "extends".parse::<EdgeRelation>().unwrap(),
            EdgeRelation::Extends
        );
        assert_eq!(
            "EXTENDS".parse::<EdgeRelation>().unwrap(),
            EdgeRelation::Extends
        );
    }

    #[test]
    fn from_str_hyphen_tolerant() {
        assert_eq!(
            "part_of".parse::<EdgeRelation>().unwrap(),
            EdgeRelation::PartOf
        );
        assert_eq!(
            "part-of".parse::<EdgeRelation>().unwrap(),
            EdgeRelation::PartOf
        );
        assert_eq!(
            "partof".parse::<EdgeRelation>().unwrap(),
            EdgeRelation::PartOf
        );

        assert_eq!(
            "introduced_by".parse::<EdgeRelation>().unwrap(),
            EdgeRelation::IntroducedBy
        );
        assert_eq!(
            "introduced-by".parse::<EdgeRelation>().unwrap(),
            EdgeRelation::IntroducedBy
        );
    }

    #[test]
    fn from_str_unknown_returns_error_with_list() {
        let err = "related_to".parse::<EdgeRelation>().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("related_to"),
            "error should mention the bad input"
        );
        assert!(
            msg.contains("contains"),
            "error should list valid relations"
        );
        assert!(msg.contains("annotates"), "error should list all 13");
    }

    #[test]
    fn category_returns_correct_group() {
        assert_eq!(EdgeRelation::Contains.category(), EdgeCategory::Structure);
        assert_eq!(EdgeRelation::PartOf.category(), EdgeCategory::Structure);
        assert_eq!(EdgeRelation::InstanceOf.category(), EdgeCategory::Structure);

        assert_eq!(EdgeRelation::Extends.category(), EdgeCategory::Derivation);
        assert_eq!(EdgeRelation::VariantOf.category(), EdgeCategory::Derivation);
        assert_eq!(
            EdgeRelation::IntroducedBy.category(),
            EdgeCategory::Derivation
        );
        assert_eq!(
            EdgeRelation::Supersedes.category(),
            EdgeCategory::Derivation
        );

        assert_eq!(EdgeRelation::DependsOn.category(), EdgeCategory::Dependency);
        assert_eq!(EdgeRelation::Enables.category(), EdgeCategory::Dependency);

        assert_eq!(
            EdgeRelation::Implements.category(),
            EdgeCategory::Implementation
        );

        assert_eq!(EdgeRelation::CompetesWith.category(), EdgeCategory::Lateral);
        assert_eq!(EdgeRelation::ComposedWith.category(), EdgeCategory::Lateral);

        assert_eq!(EdgeRelation::Annotates.category(), EdgeCategory::Annotation);
    }

    #[test]
    fn all_categories_covered() {
        let mut cats = alloc::vec::Vec::new();
        for r in EdgeRelation::ALL {
            let c = r.category();
            if !cats.contains(&c) {
                cats.push(c);
            }
        }
        assert_eq!(cats.len(), 6, "all 6 categories must be represented");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_snake_case_roundtrip() {
        let rel = EdgeRelation::IntroducedBy;
        let json = serde_json::to_string(&rel).unwrap();
        assert_eq!(json, "\"introduced_by\"");
        let parsed: EdgeRelation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rel);
    }
}
