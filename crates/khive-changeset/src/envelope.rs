//! Change-set envelope — producer identity, captured at stage time.

use khive_types::Timestamp;
use serde::{Deserialize, Serialize};

/// The NDJSON-delta schema version this crate currently emits and accepts.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Change-set-level metadata block. Carries producer identity and model
/// family; individual ops never reference it and stay producer-agnostic.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// NDJSON-delta format version this envelope was staged under.
    pub schema_version: u32,
    /// Opaque producer identity token (interactive agent id, pipeline name, ...).
    pub producer: String,
    /// Opaque producer model family token, read by the cross-family review gate.
    pub producer_model_family: String,
    /// Wall-clock time the change-set was staged, supplied by the caller.
    pub staged_at: Timestamp,
}

impl Envelope {
    /// Construct an envelope stamped with [`CURRENT_SCHEMA_VERSION`].
    pub fn new(
        producer: impl Into<String>,
        producer_model_family: impl Into<String>,
        staged_at: Timestamp,
    ) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            producer: producer.into(),
            producer_model_family: producer_model_family.into(),
            staged_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_stamps_current_schema_version() {
        let env = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        assert_eq!(env.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(env.producer, "agent:test");
        assert_eq!(env.producer_model_family, "family:sonnet");
    }

    #[test]
    fn rejects_unknown_field() {
        let json = serde_json::json!({
            "schema_version": 1,
            "producer": "agent:test",
            "producer_model_family": "family:sonnet",
            "staged_at": 1_000_000_u64,
            "unexpected": "surprise"
        });
        let result: Result<Envelope, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }
}
