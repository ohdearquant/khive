//! Closed 8-value section type taxonomy (ADR-048).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Closed 8-value taxonomy of knowledge-atom section categories.
///
/// `references` and `other` were retired by the 2026-10-04 amendment to ADR-048.
/// They are not variants; [`SectionType::RETIRED_NAMES`] lists them, and
/// [`SectionType::RETIRED_SPELLINGS`] lists every stored spelling that names them, so
/// that code tolerating data written before the retirement recognises them in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionType {
    Overview,
    CoreModel,
    BoundaryConditions,
    Formalism,
    OperationalGuidance,
    Examples,
    FailureModes,
    ExpertLens,
}

impl SectionType {
    pub const ALL: [Self; 8] = [
        Self::Overview,
        Self::CoreModel,
        Self::BoundaryConditions,
        Self::Formalism,
        Self::OperationalGuidance,
        Self::Examples,
        Self::FailureModes,
        Self::ExpertLens,
    ];

    pub const NAMES: &'static [&'static str] = &[
        "overview",
        "core_model",
        "boundary_conditions",
        "formalism",
        "operational_guidance",
        "examples",
        "failure_modes",
        "expert_lens",
    ];

    /// Section type names retired by the 2026-10-04 amendment to ADR-048.
    ///
    /// Stored rows and recorded events written before the retirement may still
    /// carry these names. Read and replay paths drop or mark them; write paths
    /// refuse them as unknown, because they are not in [`SectionType::NAMES`].
    pub const RETIRED_NAMES: &'static [&'static str] = &["references", "other"];

    /// Every spelling that the heading alias table in force before the 2026-10-04
    /// amendment resolved to a retired type, paired with that type, in normalized form
    /// (see [`SectionType::normalize_name`]).
    ///
    /// Rows written before the retirement store any of these spellings, not only the
    /// two canonical names, so a retired row is recognised through this table after
    /// normalization and never by exact string comparison. None of these spellings
    /// parses as a current type.
    pub const RETIRED_SPELLINGS: &'static [(&'static str, &'static str)] = &[
        ("references", "references"),
        ("reference", "references"),
        ("bibliography", "references"),
        ("related", "references"),
        ("see_also", "references"),
        ("further_reading", "references"),
        ("citations", "references"),
        ("links", "references"),
        ("other", "other"),
        ("misc", "other"),
        ("miscellaneous", "other"),
        ("notes", "other"),
        ("appendix", "other"),
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::CoreModel => "core_model",
            Self::BoundaryConditions => "boundary_conditions",
            Self::Formalism => "formalism",
            Self::OperationalGuidance => "operational_guidance",
            Self::Examples => "examples",
            Self::FailureModes => "failure_modes",
            Self::ExpertLens => "expert_lens",
        }
    }

    pub fn all() -> &'static [SectionType] {
        &Self::ALL
    }

    /// Normalized form of a section type name or heading: Unicode whitespace trimmed
    /// (`str::trim`, as the heading alias table has always done), ASCII letters
    /// lowercased, and `-` and space mapped to `_`. The SQL form of the same rule
    /// (`trim` with every Unicode whitespace character listed, then `replace` and
    /// `lower`, which folds ASCII only) gives the same result for every stored value.
    pub fn normalize_name(name: &str) -> String {
        name.trim().to_ascii_lowercase().replace(['-', ' '], "_")
    }

    /// The retired type that `name` spells, if any: the canonical retired name paired
    /// with its normalized form in [`SectionType::RETIRED_SPELLINGS`].
    pub fn retired_type_of(name: &str) -> Option<&'static str> {
        let normalized = Self::normalize_name(name);
        Self::RETIRED_SPELLINGS
            .iter()
            .find(|(spelling, _)| *spelling == normalized)
            .map(|(_, retired)| *retired)
    }

    /// True when `name` spells a retired type (see [`SectionType::retired_type_of`]).
    pub fn is_retired_name(name: &str) -> bool {
        Self::retired_type_of(name).is_some()
    }

    /// Parse from canonical snake_case or common heading aliases, after
    /// [`SectionType::normalize_name`].
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match Self::normalize_name(s).as_str() {
            "overview" | "introduction" | "intro" | "context" | "motivation" | "background" => {
                Some(Self::Overview)
            }
            "core_model" | "model" | "mechanism" | "internals" | "structure" | "architecture"
            | "definition" => Some(Self::CoreModel),
            "boundary_conditions"
            | "when_to_use"
            | "scope"
            | "constraints"
            | "prerequisites"
            | "preconditions" => Some(Self::BoundaryConditions),
            "formalism" | "formal" | "theory" | "math" | "mathematics" | "theorems"
            | "algorithm" | "algorithms" | "proof" | "complexity" => Some(Self::Formalism),
            "operational_guidance"
            | "implementation"
            | "usage"
            | "how_to"
            | "steps"
            | "checklist"
            | "guide"
            | "guidance"
            | "practice"
            | "practices"
            | "best_practices" => Some(Self::OperationalGuidance),
            "examples" | "example" | "worked_examples" | "case_study" | "cases" | "demos"
            | "demo" => Some(Self::Examples),
            "failure_modes" | "pitfall" | "pitfalls" | "anti_patterns" | "antipatterns"
            | "gotchas" | "edge_cases" | "warnings" | "cautions" => Some(Self::FailureModes),
            "expert_lens" | "trade_offs" | "tradeoffs" | "advanced" | "nuances" | "insights"
            | "discussion" | "comparison" => Some(Self::ExpertLens),
            _ => None,
        }
    }
}

impl fmt::Display for SectionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SectionType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "overview" => Ok(Self::Overview),
            "core_model" => Ok(Self::CoreModel),
            "boundary_conditions" => Ok(Self::BoundaryConditions),
            "formalism" => Ok(Self::Formalism),
            "operational_guidance" => Ok(Self::OperationalGuidance),
            "examples" => Ok(Self::Examples),
            "failure_modes" => Ok(Self::FailureModes),
            "expert_lens" => Ok(Self::ExpertLens),
            _ => Err(format!("unknown SectionType: {s:?}")),
        }
    }
}
