//! Edge relation types for the closed ontology used throughout khive.

extern crate alloc;
use alloc::string::String;
use core::fmt;
use core::str::FromStr;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The 10 semantic categories that group the 20 canonical edge relations.
///
/// Exposed via [`EdgeRelation::category`] for query planners and UI rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EdgeCategory {
    /// Composition, reference and location: `contains`, `part_of`, `instance_of`, `links_to`,
    /// `located_in`
    Structure,
    /// Intellectual lineage: `extends`, `variant_of`, `introduced_by`, `supersedes`
    Derivation,
    /// Data/artifact origin: `derived_from`
    Provenance,
    /// Time ordering: `precedes`
    Temporal,
    /// Build/runtime needs: `depends_on`, `enables`
    Dependency,
    /// Code ↔ concept: `implements`
    Implementation,
    /// Peer relationships: `competes_with`, `composed_with`
    Lateral,
    /// Cross-substrate annotation: `annotates`
    Annotation,
    /// Evidence for/against a claim: `supports`, `refutes`
    Epistemic,
    /// Current whole or partial ownership: `owns`
    Ownership,
}

/// Closed set of 20 canonical edge relations.
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
    LinksTo,
    LocatedIn,
    // Derivation
    Extends,
    VariantOf,
    IntroducedBy,
    Supersedes,
    // Provenance
    DerivedFrom,
    // Temporal
    Precedes,
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
    // Epistemic
    Supports,
    Refutes,
    // Ownership
    Owns,
}

impl EdgeRelation {
    /// All 20 canonical relations in ontology-table order.
    pub const ALL: [Self; 20] = [
        Self::Contains,
        Self::PartOf,
        Self::InstanceOf,
        Self::LinksTo,
        Self::LocatedIn,
        Self::Extends,
        Self::VariantOf,
        Self::IntroducedBy,
        Self::Supersedes,
        Self::DerivedFrom,
        Self::Precedes,
        Self::DependsOn,
        Self::Enables,
        Self::Implements,
        Self::CompetesWith,
        Self::ComposedWith,
        Self::Annotates,
        Self::Supports,
        Self::Refutes,
        Self::Owns,
    ];

    /// Valid snake_case names for all 20 canonical relations.
    pub const VALID_NAMES: &'static [&'static str] = &[
        "contains",
        "part_of",
        "instance_of",
        "links_to",
        "located_in",
        "extends",
        "variant_of",
        "introduced_by",
        "supersedes",
        "derived_from",
        "precedes",
        "depends_on",
        "enables",
        "implements",
        "competes_with",
        "composed_with",
        "annotates",
        "supports",
        "refutes",
        "owns",
    ];

    /// `true` for symmetric relations: edge direction has no semantic meaning.
    pub const fn is_symmetric(&self) -> bool {
        matches!(self, Self::CompetesWith | Self::ComposedWith)
    }

    /// Order the endpoints of an edge so a symmetric relation has one stored form.
    ///
    /// For symmetric relations (`competes_with`, `composed_with`) the smaller
    /// endpoint comes first, so A→B and B→A collapse into a single canonical row.
    /// Every other relation, and a symmetric pair that is already ordered or has
    /// equal endpoints, is returned as given.
    ///
    /// Generic over `T: Ord` because this crate does not depend on a UUID type;
    /// for UUIDs the order is the byte order of the id.
    pub fn canonical_endpoints<T: Ord>(&self, source: T, target: T) -> (T, T) {
        if self.is_symmetric() && target < source {
            (target, source)
        } else {
            (source, target)
        }
    }

    /// The category this relation belongs to.
    pub const fn category(&self) -> EdgeCategory {
        match self {
            Self::Contains | Self::PartOf | Self::InstanceOf | Self::LinksTo | Self::LocatedIn => {
                EdgeCategory::Structure
            }
            Self::Extends | Self::VariantOf | Self::IntroducedBy | Self::Supersedes => {
                EdgeCategory::Derivation
            }
            Self::DerivedFrom => EdgeCategory::Provenance,
            Self::Precedes => EdgeCategory::Temporal,
            Self::DependsOn | Self::Enables => EdgeCategory::Dependency,
            Self::Implements => EdgeCategory::Implementation,
            Self::CompetesWith | Self::ComposedWith => EdgeCategory::Lateral,
            Self::Annotates => EdgeCategory::Annotation,
            Self::Supports | Self::Refutes => EdgeCategory::Epistemic,
            Self::Owns => EdgeCategory::Ownership,
        }
    }

    /// Canonical snake_case name as stored in the database.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::PartOf => "part_of",
            Self::InstanceOf => "instance_of",
            Self::LinksTo => "links_to",
            Self::LocatedIn => "located_in",
            Self::Extends => "extends",
            Self::VariantOf => "variant_of",
            Self::IntroducedBy => "introduced_by",
            Self::Supersedes => "supersedes",
            Self::DerivedFrom => "derived_from",
            Self::Precedes => "precedes",
            Self::DependsOn => "depends_on",
            Self::Enables => "enables",
            Self::Implements => "implements",
            Self::CompetesWith => "competes_with",
            Self::ComposedWith => "composed_with",
            Self::Annotates => "annotates",
            Self::Supports => "supports",
            Self::Refutes => "refutes",
            Self::Owns => "owns",
        }
    }
}

impl fmt::Display for EdgeRelation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EdgeRelation {
    type Err = crate::error::UnknownVariant;

    /// Parse a string into an `EdgeRelation`.
    ///
    /// Accepts the 20 canonical relation names (case-insensitive, with hyphens
    /// normalised to underscores) and also squashed forms that omit the separator
    /// (e.g. `"partof"`, `"derivedfrom"`).  The squashed forms exist for ergonomic
    /// DSL entry; they are **not** stored on the wire, which always uses the
    /// canonical snake_case form produced by [`EdgeRelation::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut normalised = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '-' | '_' => normalised.push('_'),
                c if c.is_ascii_alphanumeric() => normalised.push(c.to_ascii_lowercase()),
                _ => {
                    return Err(crate::error::UnknownVariant::new(
                        "edge_relation",
                        s,
                        Self::VALID_NAMES,
                    ));
                }
            }
        }

        match normalised.as_str() {
            "contains" => Ok(Self::Contains),
            "part_of" | "partof" => Ok(Self::PartOf),
            "instance_of" | "instanceof" => Ok(Self::InstanceOf),
            "links_to" | "linksto" => Ok(Self::LinksTo),
            "located_in" | "locatedin" => Ok(Self::LocatedIn),
            "extends" => Ok(Self::Extends),
            "variant_of" | "variantof" => Ok(Self::VariantOf),
            "introduced_by" | "introducedby" => Ok(Self::IntroducedBy),
            "supersedes" => Ok(Self::Supersedes),
            "derived_from" | "derivedfrom" => Ok(Self::DerivedFrom),
            "precedes" => Ok(Self::Precedes),
            "depends_on" | "dependson" => Ok(Self::DependsOn),
            "enables" => Ok(Self::Enables),
            "implements" => Ok(Self::Implements),
            "competes_with" | "competeswith" => Ok(Self::CompetesWith),
            "composed_with" | "composedwith" => Ok(Self::ComposedWith),
            "annotates" => Ok(Self::Annotates),
            "supports" => Ok(Self::Supports),
            "refutes" => Ok(Self::Refutes),
            "owns" => Ok(Self::Owns),
            _ => Err(crate::error::UnknownVariant::new(
                "edge_relation",
                s,
                Self::VALID_NAMES,
            )),
        }
    }
}

#[cfg(test)]
#[path = "edge_tests.rs"]
mod tests;
