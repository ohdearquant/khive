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
/// Returns [`ChangeSetError::UnsupportedSchemaVersion`] for a stale envelope, or
/// [`ChangeSetError::Serialize`] if any envelope or op cannot serialize.
pub fn to_ndjson(changeset: &ChangeSet) -> Result<String, ChangeSetError> {
    if changeset.envelope.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(ChangeSetError::UnsupportedSchemaVersion {
            found: changeset.envelope.schema_version,
            expected: CURRENT_SCHEMA_VERSION,
        });
    }
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
    reject_repeated_members(first_line)
        .map_err(|source| ChangeSetError::MalformedLine { line: 1, source })?;
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
        reject_repeated_members(line).map_err(|source| ChangeSetError::MalformedLine {
            line: idx + 1,
            source,
        })?;
        let op: Op =
            serde_json::from_str(line).map_err(|source| ChangeSetError::MalformedLine {
                line: idx + 1,
                source,
            })?;
        ops.push(op);
    }
    Ok(ChangeSet { envelope, ops })
}

/// Refuses a line in which any JSON object, at any depth, names the same member twice.
///
/// The typed decode keeps only the last of a repeated name, so a caller's scan of the
/// decoded values would never see the earlier copies while the stored text still holds
/// them. Names are compared after JSON unescaping, and the error names a position only,
/// never the member or its value.
fn reject_repeated_members(line: &str) -> Result<(), serde_json::Error> {
    serde_json::from_str::<UniqueMembers>(line).map(|_| ())
}

struct UniqueMembers;

impl<'de> serde::Deserialize<'de> for UniqueMembers {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueMembersVisitor)
    }
}

struct UniqueMembersVisitor;

impl<'de> serde::de::Visitor<'de> for UniqueMembersVisitor {
    type Value = UniqueMembers;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<UniqueMembers, E> {
        Ok(UniqueMembers)
    }

    fn visit_i64<E>(self, _: i64) -> Result<UniqueMembers, E> {
        Ok(UniqueMembers)
    }

    fn visit_u64<E>(self, _: u64) -> Result<UniqueMembers, E> {
        Ok(UniqueMembers)
    }

    fn visit_f64<E>(self, _: f64) -> Result<UniqueMembers, E> {
        Ok(UniqueMembers)
    }

    fn visit_str<E>(self, _: &str) -> Result<UniqueMembers, E> {
        Ok(UniqueMembers)
    }

    fn visit_unit<E>(self) -> Result<UniqueMembers, E> {
        Ok(UniqueMembers)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> Result<UniqueMembers, A::Error> {
        while seq.next_element::<UniqueMembers>()?.is_some() {}
        Ok(UniqueMembers)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> Result<UniqueMembers, A::Error> {
        let mut seen = std::collections::HashSet::new();
        while let Some(name) = map.next_key::<String>()? {
            if !seen.insert(name) {
                return Err(serde::de::Error::custom("an object repeats a member name"));
            }
            map.next_value::<UniqueMembers>()?;
        }
        Ok(UniqueMembers)
    }
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

    /// One sample create, its properties holding `first` then `second` under one name.
    fn create_line_with_repeated_property(first: &str, second: &str) -> String {
        let id = uuid_text(7);
        format!(
            "{{\"op\":\"create\",\"id\":\"{id}\",\"namespace\":\"local\",\"target\":{{\
             \"kind\":\"entity\",\"entity_kind\":\"concept\",\"name\":\"X\",\
             \"properties\":{{\"{first}\":\"kept-out\",\"{second}\":\"benign\"}}}}}}"
        )
    }

    fn uuid_text(n: u128) -> String {
        serde_json::to_value(Id128::from_u128(n))
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn env_line() -> String {
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        serde_json::to_string(&envelope).unwrap()
    }

    #[test]
    fn a_distinct_property_map_decodes() {
        let text = format!(
            "{}\n{}\n",
            env_line(),
            create_line_with_repeated_property("label", "other")
        );
        let decoded = from_ndjson(&text).expect("distinct names decode");
        match &decoded.ops[0] {
            Op::Create(CreateOp {
                target: CreateTarget::Entity(fields),
                ..
            }) => assert_eq!(fields.properties.len(), 2),
            other => panic!("expected an entity create, got {other:?}"),
        }
    }

    #[test]
    fn a_repeated_nested_member_is_malformed_and_the_error_names_no_bytes() {
        let text = format!(
            "{}\n{}\n",
            env_line(),
            create_line_with_repeated_property("label", "label")
        );
        let err = from_ndjson(&text).unwrap_err();
        let shown = err.to_string();
        match err {
            ChangeSetError::MalformedLine { line, .. } => assert_eq!(line, 2),
            other => panic!("expected MalformedLine, got {other:?}"),
        }
        assert!(shown.contains("repeats a member name"), "{shown}");
        assert!(!shown.contains("label"), "{shown}");
        assert!(!shown.contains("kept-out"), "{shown}");
        assert!(!shown.contains("benign"), "{shown}");
    }

    #[test]
    fn member_names_are_compared_after_unescaping() {
        // `label` decodes to `label`, so the two spellings are one member name.
        let text = format!(
            "{}\n{}\n",
            env_line(),
            create_line_with_repeated_property("\\u006cabel", "label")
        );
        assert!(matches!(
            from_ndjson(&text),
            Err(ChangeSetError::MalformedLine { line: 2, .. })
        ));
    }

    #[test]
    fn a_repeated_top_level_or_envelope_member_is_malformed() {
        let op = create_line_with_repeated_property("label", "other");
        let repeated_op = op.replacen(
            "{\"op\":\"create\",",
            "{\"op\":\"create\",\"op\":\"create\",",
            1,
        );
        assert_ne!(repeated_op, op);
        let text = format!("{}\n{repeated_op}\n", env_line());
        assert!(matches!(
            from_ndjson(&text),
            Err(ChangeSetError::MalformedLine { line: 2, .. })
        ));

        let env = env_line();
        let repeated_env = env.replacen('{', "{\"schema_version\":1,", 1);
        assert!(env.contains("\"schema_version\""), "{env}");
        let text = format!("{repeated_env}\n{op}\n");
        assert!(matches!(
            from_ndjson(&text),
            Err(ChangeSetError::MalformedLine { line: 1, .. })
        ));
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
