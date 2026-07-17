//! Change-set envelope — producer identity, captured at stage time.

use khive_types::Timestamp;
use serde::{Deserialize, Serialize};

/// The NDJSON-delta schema version this crate currently emits and accepts.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Change-set-wide producer identity and stage-time provenance.
///
/// See `crates/khive-changeset/docs/api/envelope.md` for commit-trailer behavior.
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
    /// Optional producer batch ID used verbatim for commit provenance; absent
    /// values let the committer derive a deterministic producer/time fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
}

impl Envelope {
    /// Constructs an envelope at [`CURRENT_SCHEMA_VERSION`] with no batch ID.
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
            batch_id: None,
        }
    }

    /// Attaches an opaque producer-assigned batch identifier.
    pub fn with_batch_id(mut self, batch_id: impl Into<String>) -> Self {
        self.batch_id = Some(batch_id.into());
        self
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

    #[test]
    fn batch_id_absent_by_default_and_not_serialized() {
        let env = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        assert_eq!(env.batch_id, None);
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            !json.contains("batch_id"),
            "absent batch_id must not appear on the wire at all (not even as null): {json}"
        );
    }

    #[test]
    fn batch_id_round_trips_when_present() {
        let env = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1))
            .with_batch_id("batch-123");
        let json = serde_json::to_string(&env).unwrap();
        let decoded: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, env);
        assert_eq!(decoded.batch_id.as_deref(), Some("batch-123"));
    }

    #[test]
    fn batch_id_round_trips_when_absent() {
        let env = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let json = serde_json::to_string(&env).unwrap();
        let decoded: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, env);
        assert_eq!(decoded.batch_id, None);
    }
}
