use serde::{Deserialize, Serialize};

use crate::{GateValidationError, Obligation};

/// Gate decision: allow (with optional obligations) or deny (with reason).
///
/// `Deny` requires a non-empty `reason`. Enforced at construction and
/// deserialization.
#[derive(Clone, Debug, Serialize)]
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

/// Raw deserialization target for [`GateDecision`] — validated via `TryFrom`.
#[derive(Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
enum RawGateDecision {
    Allow {
        #[serde(default)]
        obligations: Vec<Obligation>,
    },
    Deny {
        reason: String,
    },
}

impl TryFrom<RawGateDecision> for GateDecision {
    type Error = GateValidationError;

    fn try_from(raw: RawGateDecision) -> Result<Self, Self::Error> {
        match raw {
            RawGateDecision::Allow { obligations } => Ok(GateDecision::Allow { obligations }),
            RawGateDecision::Deny { reason } => {
                if reason.is_empty() {
                    return Err(GateValidationError::EmptyDenyReason);
                }
                Ok(GateDecision::Deny { reason })
            }
        }
    }
}

impl<'de> Deserialize<'de> for GateDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawGateDecision::deserialize(deserializer)?;
        GateDecision::try_from(raw).map_err(serde::de::Error::custom)
    }
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

    /// Create a `Deny` decision. Returns `Err` if `reason` is empty.
    pub fn try_deny(reason: impl Into<String>) -> Result<Self, GateValidationError> {
        let reason = reason.into();
        if reason.is_empty() {
            return Err(GateValidationError::EmptyDenyReason);
        }
        Ok(Self::Deny { reason })
    }

    /// Returns a `Deny` with the given reason. Panics if `reason` is empty.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::try_deny(reason).expect("GateDecision::deny: reason must not be empty")
    }

    /// Returns `true` when the decision is `Allow`.
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}
