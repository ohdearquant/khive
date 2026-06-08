//! Closed 10-value section type taxonomy (ADR-048).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

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
    References,
    Other,
}

impl SectionType {
    pub const ALL: [Self; 10] = [
        Self::Overview,
        Self::CoreModel,
        Self::BoundaryConditions,
        Self::Formalism,
        Self::OperationalGuidance,
        Self::Examples,
        Self::FailureModes,
        Self::ExpertLens,
        Self::References,
        Self::Other,
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
        "references",
        "other",
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
            Self::References => "references",
            Self::Other => "other",
        }
    }

    pub fn all() -> &'static [SectionType] {
        &Self::ALL
    }

    /// Parse from canonical snake_case or common heading aliases.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "overview" | "introduction" | "intro" | "context" | "motivation" | "background" => {
                Some(Self::Overview)
            }
            "core_model" | "model" | "mechanism" | "internals" | "structure" | "architecture" => {
                Some(Self::CoreModel)
            }
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
            "failure_modes" | "pitfalls" | "anti_patterns" | "antipatterns" | "gotchas"
            | "edge_cases" | "warnings" | "cautions" => Some(Self::FailureModes),
            "expert_lens" | "trade_offs" | "tradeoffs" | "advanced" | "nuances" | "insights"
            | "discussion" => Some(Self::ExpertLens),
            "references" | "reference" | "bibliography" | "related" | "see_also"
            | "further_reading" | "citations" | "links" => Some(Self::References),
            "other" | "misc" | "miscellaneous" | "notes" | "appendix" => Some(Self::Other),
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
            "references" => Ok(Self::References),
            "other" => Ok(Self::Other),
            _ => Err(format!("unknown SectionType: {s:?}")),
        }
    }
}
