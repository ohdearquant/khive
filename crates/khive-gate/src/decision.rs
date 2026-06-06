use serde::{Deserialize, Serialize};

use crate::Obligation;

// ---------- Decision ----------

/// Outcome of a gate check: either allow (with optional obligations) or deny (with a reason).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GateDecision {
    Allow {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        obligations: Vec<Obligation>,
    },
    Deny {
        reason: String,
    },
}

impl GateDecision {
    /// Returns an unconditional `Allow` with no obligations.
    pub fn allow() -> Self {
        Self::Allow {
            obligations: Vec::new(),
        }
    }

    /// Returns an `Allow` with the given policy obligations attached.
    pub fn allow_with(obligations: Vec<Obligation>) -> Self {
        Self::Allow { obligations }
    }

    /// Returns a `Deny` with the given human-readable reason.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    /// Returns `true` when the decision is `Allow`.
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}
