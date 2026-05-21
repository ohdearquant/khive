//! Objective evaluation context

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Context for objective evaluation
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectiveContext {
    /// Evaluation time
    pub as_of: DateTime<Utc>,
    /// Maximum candidates to consider
    pub max_candidates: Option<usize>,
    /// Minimum score threshold
    pub min_score: Option<f64>,
    /// Extra context data
    #[serde(default)]
    pub extra: serde_json::Value,
}

impl ObjectiveContext {
    /// Create a new context with current time
    pub fn new() -> Self {
        Self {
            as_of: Utc::now(),
            ..Default::default()
        }
    }

    /// Create context for a specific time
    pub fn at(time: DateTime<Utc>) -> Self {
        Self {
            as_of: time,
            ..Default::default()
        }
    }

    /// Set maximum candidates
    pub fn with_max_candidates(mut self, n: usize) -> Self {
        self.max_candidates = Some(n);
        self
    }

    /// Set minimum score threshold
    pub fn with_min_score(mut self, score: f64) -> Self {
        self.min_score = Some(score);
        self
    }

    /// Set extra context
    pub fn with_extra(mut self, extra: serde_json::Value) -> Self {
        self.extra = extra;
        self
    }
}
