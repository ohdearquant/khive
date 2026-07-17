//! The change-set: an envelope plus its ordered op-list, and NDJSON-delta codec.

use crate::envelope::{Envelope, CURRENT_SCHEMA_VERSION};
use crate::op::Op;

/// Envelope metadata plus an ordered list of staged operations.
///
/// Operation order is semantically load-bearing (a `link` may target an
/// earlier `create` ID) and is preserved exactly by the NDJSON codec.
/// See `crates/khive-changeset/docs/api/ndjson-codec.md` for the wire contract.
#[derive(Clone, Debug)]
pub struct ChangeSet {
    /// Change-set-wide stage-time provenance.
    pub envelope: Envelope,
    /// Operations in semantic application order.
    pub ops: Vec<Op>,
}

impl ChangeSet {
    /// Stores `envelope` and `ops` without reordering or validation.
    pub fn new(envelope: Envelope, ops: Vec<Op>) -> Self {
        Self { envelope, ops }
    }
}

/// In-memory NDJSON encode/decode errors; no variant represents I/O.
///
/// See `crates/khive-changeset/docs/api/ndjson-codec.md` for variant semantics.
#[derive(Debug, thiserror::Error)]
pub enum ChangeSetError {
    #[error("NDJSON-delta input is empty; expected an envelope header line")]
    Empty,
    #[error("line {line} is not valid JSON: {source}")]
    MalformedLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "envelope schema_version {found} is not supported (this crate emits and \
         accepts schema_version {expected})"
    )]
    UnsupportedSchemaVersion { found: u32, expected: u32 },
    #[error("failed to serialize change-set: {0}")]
    Serialize(#[source] serde_json::Error),
}

/// Encodes the envelope as line one and each op on a following line, in order.
///
/// The returned in-memory string ends every line, including the last, with `\n`.
///
/// # Errors
///
/// Returns [`ChangeSetError::Serialize`] if any envelope or op cannot serialize.
pub fn to_ndjson(changeset: &ChangeSet) -> Result<String, ChangeSetError> {
    let mut out = String::new();
    out.push_str(&serde_json::to_string(&changeset.envelope).map_err(ChangeSetError::Serialize)?);
    out.push('\n');
    for op in &changeset.ops {
        out.push_str(&serde_json::to_string(op).map_err(ChangeSetError::Serialize)?);
        out.push('\n');
    }
    Ok(out)
}

/// Decodes an envelope header and every following line as one ordered [`Op`].
///
/// Blank lines and unknown fields are errors; schema version must equal
/// [`CURRENT_SCHEMA_VERSION`].
///
/// # Errors
///
/// Returns [`ChangeSetError::Empty`], [`ChangeSetError::MalformedLine`], or
/// [`ChangeSetError::UnsupportedSchemaVersion`] according to the failed stage.
/// See `crates/khive-changeset/docs/api/ndjson-codec.md` for line semantics.
pub fn from_ndjson(input: &str) -> Result<ChangeSet, ChangeSetError> {
    let mut lines = input.lines().enumerate();
    let (_, first_line) = lines.next().ok_or(ChangeSetError::Empty)?;
    let envelope: Envelope = serde_json::from_str(first_line)
        .map_err(|source| ChangeSetError::MalformedLine { line: 1, source })?;
    if envelope.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(ChangeSetError::UnsupportedSchemaVersion {
            found: envelope.schema_version,
            expected: CURRENT_SCHEMA_VERSION,
        });
    }

    // Parse blank lines too, preserving “every post-header line is an op.”
    let mut ops = Vec::new();
    for (idx, line) in lines {
        let op: Op =
            serde_json::from_str(line).map_err(|source| ChangeSetError::MalformedLine {
                line: idx + 1,
                source,
            })?;
        ops.push(op);
    }
    Ok(ChangeSet { envelope, ops })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{CreateOp, CreateTarget, EntityCreateFields};
    use khive_types::{EntityKind, Id128, Namespace, Timestamp};

    fn sample_changeset() -> ChangeSet {
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let create = Op::Create(CreateOp {
            id: Id128::from_u128(1),
            namespace: Namespace::local(),
            target: CreateTarget::Entity(EntityCreateFields {
                entity_kind: EntityKind::Concept,
                entity_type: None,
                name: "X".into(),
                description: None,
                properties: Default::default(),
                tags: vec![],
            }),
        });
        ChangeSet::new(envelope, vec![create])
    }

    #[test]
    fn round_trips_envelope_and_ops() {
        let cs = sample_changeset();
        let text = to_ndjson(&cs).unwrap();
        let decoded = from_ndjson(&text).unwrap();
        assert_eq!(decoded.envelope, cs.envelope);
        assert_eq!(decoded.ops.len(), 1);
        let text2 = to_ndjson(&decoded).unwrap();
        assert_eq!(text, text2, "re-serialization must be byte-identical");
    }

    #[test]
    fn envelope_is_line_one() {
        let cs = sample_changeset();
        let text = to_ndjson(&cs).unwrap();
        let first_line = text.lines().next().unwrap();
        let parsed_env: Envelope = serde_json::from_str(first_line).unwrap();
        assert_eq!(parsed_env, cs.envelope);
    }

    #[test]
    fn envelope_batch_id_round_trips_through_ndjson() {
        let mut cs = sample_changeset();
        cs.envelope = cs.envelope.with_batch_id("batch-xyz");
        let text = to_ndjson(&cs).unwrap();
        let decoded = from_ndjson(&text).unwrap();
        assert_eq!(decoded.envelope.batch_id.as_deref(), Some("batch-xyz"));
        assert_eq!(decoded.envelope, cs.envelope);
    }

    #[test]
    fn envelope_without_batch_id_round_trips_through_ndjson() {
        let cs = sample_changeset();
        assert_eq!(cs.envelope.batch_id, None);
        let text = to_ndjson(&cs).unwrap();
        let decoded = from_ndjson(&text).unwrap();
        assert_eq!(decoded.envelope.batch_id, None);
        assert_eq!(decoded.envelope, cs.envelope);
    }

    #[test]
    fn empty_input_errors() {
        let result = from_ndjson("");
        assert!(matches!(result, Err(ChangeSetError::Empty)));
    }

    #[test]
    fn malformed_op_line_reports_correct_line_number() {
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let env_line = serde_json::to_string(&envelope).unwrap();
        let text = format!("{env_line}\n{{\"op\": \"create\", not json}}\n");
        let err = from_ndjson(&text).unwrap_err();
        match err {
            ChangeSetError::MalformedLine { line, .. } => assert_eq!(line, 2),
            other => panic!("expected MalformedLine, got {other:?}"),
        }
    }

    #[test]
    fn blank_line_between_ops_is_malformed() {
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let env_line = serde_json::to_string(&envelope).unwrap();
        let text = format!("{env_line}\n\n");
        let err = from_ndjson(&text).unwrap_err();
        match err {
            ChangeSetError::MalformedLine { line, .. } => assert_eq!(line, 2),
            other => panic!("expected MalformedLine, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let mut envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        envelope.schema_version = 99;
        let env_line = serde_json::to_string(&envelope).unwrap();
        let err = from_ndjson(&env_line).unwrap_err();
        assert!(matches!(
            err,
            ChangeSetError::UnsupportedSchemaVersion {
                found: 99,
                expected: CURRENT_SCHEMA_VERSION
            }
        ));
    }

    #[test]
    fn op_order_is_preserved() {
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let mk_create = |n: u128| {
            Op::Create(CreateOp {
                id: Id128::from_u128(n),
                namespace: Namespace::local(),
                target: CreateTarget::Entity(EntityCreateFields {
                    entity_kind: EntityKind::Concept,
                    entity_type: None,
                    name: format!("entity-{n}"),
                    description: None,
                    properties: Default::default(),
                    tags: vec![],
                }),
            })
        };
        let ops = vec![mk_create(3), mk_create(1), mk_create(2)];
        let cs = ChangeSet::new(envelope, ops);
        let text = to_ndjson(&cs).unwrap();
        let decoded = from_ndjson(&text).unwrap();
        let ids: Vec<u128> = decoded
            .ops
            .iter()
            .map(|op| match op {
                Op::Create(c) => c.id.to_u128(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, vec![3, 1, 2]);
    }
}
