use serde::{Deserialize, Serialize};

use crate::GateValidationError;

/// Policy instructions attached to an allow; only `Audit` has v0 runtime handling.
///
/// `RateLimit` requires positive values but is not enforced in v0. See
/// `crates/khive-gate/docs/api/policy-types.md`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Obligation {
    Audit {
        tag: String,
    },
    RateLimit {
        window_secs: u64,
        max: u32,
    },
    /// Policy-specific arbitrary JSON; the struct form preserves internally tagged scalar values.
    Custom {
        value: serde_json::Value,
    },
}

/// Raw deserialization target for [`Obligation`] — validated via `TryFrom`.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RawObligation {
    Audit { tag: String },
    RateLimit { window_secs: u64, max: u32 },
    Custom { value: serde_json::Value },
}

impl TryFrom<RawObligation> for Obligation {
    type Error = GateValidationError;

    fn try_from(raw: RawObligation) -> Result<Self, Self::Error> {
        match raw {
            RawObligation::Audit { tag } => {
                if tag.is_empty() {
                    return Err(GateValidationError::EmptyAuditTag);
                }
                Ok(Obligation::Audit { tag })
            }
            RawObligation::RateLimit { window_secs, max } => {
                Obligation::try_rate_limit(window_secs, max)
            }
            RawObligation::Custom { value } => Ok(Obligation::Custom { value }),
        }
    }
}

impl<'de> Deserialize<'de> for Obligation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawObligation::deserialize(deserializer)?;
        Obligation::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl Obligation {
    /// Create a validated `RateLimit` obligation.
    /// Returns `Err` if `window_secs` or `max` is zero.
    pub fn try_rate_limit(window_secs: u64, max: u32) -> Result<Self, GateValidationError> {
        if window_secs == 0 {
            return Err(GateValidationError::ZeroRateLimitWindow);
        }
        if max == 0 {
            return Err(GateValidationError::ZeroRateLimitMax);
        }
        Ok(Self::RateLimit { window_secs, max })
    }

    /// Create a validated `RateLimit` obligation. Panics if `window_secs` or `max` is zero.
    pub fn rate_limit(window_secs: u64, max: u32) -> Self {
        Self::try_rate_limit(window_secs, max)
            .expect("Obligation::rate_limit: window_secs and max must be > 0")
    }
}
