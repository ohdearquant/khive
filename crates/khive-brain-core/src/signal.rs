//! Feedback signal enums — pure types with no storage dependency.

use serde::{Deserialize, Serialize};

/// Feedback signal values for the `brain.feedback` verb.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSignal {
    Useful,
    NotUseful,
    Wrong,
}

/// Semantic event taxonomy for brain fold updates (issue #268).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackEventKind {
    ExplicitPositive,
    ExplicitNegative,
    ImplicitPositive,
    ImplicitNegative,
    Correction,
}

impl FeedbackEventKind {
    pub fn update_weight(&self) -> f64 {
        match self {
            FeedbackEventKind::Correction => 2.0,
            FeedbackEventKind::ExplicitPositive | FeedbackEventKind::ExplicitNegative => 1.5,
            FeedbackEventKind::ImplicitPositive | FeedbackEventKind::ImplicitNegative => 0.5,
        }
    }

    pub fn is_positive(&self) -> bool {
        matches!(
            self,
            FeedbackEventKind::ExplicitPositive | FeedbackEventKind::ImplicitPositive
        )
    }

    pub fn from_signal_str(s: &str) -> Option<Self> {
        match s {
            "explicit_positive" => Some(FeedbackEventKind::ExplicitPositive),
            "explicit_negative" => Some(FeedbackEventKind::ExplicitNegative),
            "implicit_positive" => Some(FeedbackEventKind::ImplicitPositive),
            "implicit_negative" => Some(FeedbackEventKind::ImplicitNegative),
            "correction" => Some(FeedbackEventKind::Correction),
            _ => None,
        }
    }
}
