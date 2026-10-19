//! Strict restore wire controls and the frozen version-one dispatch boundary.
use khive_changeset::{from_ndjson, to_ndjson, ChangeSet, ChangeSetError, Envelope, Op, RestoreOp};
use khive_types::Timestamp;
use serde_json::{json, Value};

fn record(substrate: &str) -> Value {
    let common = json!({
        "substrate": substrate,
        "id": "00000000-0000-0000-0000-000000000001",
        "namespace": "local",
        "created_at": 11,
        "updated_at": 29,
        "properties": {"original": true},
        "deleted_at": null
    });
    let mut value = common.as_object().unwrap().clone();
    let fields = match substrate {
        "entity" => {
            json!({"kind":"concept", "name":"Original", "entity_type":"topic", "description":"body", "tags":["original"]})
        }
        "note" => {
            json!({"kind":"observation", "status":"active", "content":"Original note", "tags":["original"], "salience":0.625, "decay_factor":0.02, "expires_at":100})
        }
        "edge" => {
            json!({"source":"00000000-0000-0000-0000-000000000002", "target":"00000000-0000-0000-0000-000000000003", "relation":"extends", "weight":0.625})
        }
        _ => unreachable!(),
    };
    value.extend(fields.as_object().unwrap().clone());
    Value::Object(value)
}

fn operation(substrate: &str) -> Value {
    json!({"op":"restore", "target_id":"00000000-0000-0000-0000-000000000001", "record":record(substrate)})
}

fn envelope() -> Envelope {
    Envelope::new("restore-test", "test", Timestamp::from_secs(1))
}

#[test]
fn restores_roundtrip_all_substrates_without_dropping_any_record_field() {
    for substrate in ["entity", "note", "edge"] {
        let value = operation(substrate);
        let op: Op = serde_json::from_value(value.clone()).unwrap();
        let Op::Restore(restore) = &op else {
            panic!("wrong operation")
        };
        assert_eq!(
            serde_json::to_value(restore.record()).unwrap(),
            value["record"]
        );
        let cs = ChangeSet::new(envelope(), vec![op]);
        let encoded = to_ndjson(&cs).unwrap();
        let decoded = from_ndjson(&encoded).unwrap();
        assert_eq!(to_ndjson(&decoded).unwrap(), encoded);
    }
}

#[test]
fn restore_rejects_unknown_missing_mismatched_and_tombstoned_fields() {
    for substrate in ["entity", "note", "edge"] {
        let base = operation(substrate);
        let mut variants = Vec::new();
        let mut value = base.clone();
        value["unexpected"] = json!(true);
        variants.push(value);
        let mut value = base.clone();
        value["record"]["unexpected"] = json!(true);
        variants.push(value);
        let mut value = base.clone();
        value.as_object_mut().unwrap().remove("record");
        variants.push(value);
        let mut value = base.clone();
        value["record"]
            .as_object_mut()
            .unwrap()
            .remove("created_at");
        variants.push(value);
        let mut value = base.clone();
        value["target_id"] = json!("00000000-0000-0000-0000-000000000009");
        variants.push(value);
        let mut value = base;
        value["record"]["deleted_at"] = json!(42);
        variants.push(value);
        for value in variants {
            assert!(
                serde_json::from_value::<Op>(value.clone()).is_err(),
                "accepted {value}"
            );
        }
    }
}

#[test]
fn restore_reuses_numeric_mirrors_and_refuses_unrestorable_note_status() {
    for (substrate, field, invalid) in [
        ("note", "salience", json!(1.5)),
        ("note", "decay_factor", json!(-0.1)),
        ("edge", "weight", json!(1.5)),
        ("note", "status", json!("archived")),
        ("note", "status", json!("deleted")),
    ] {
        let mut value = operation(substrate);
        value["record"][field] = invalid;
        assert!(serde_json::from_value::<Op>(value).is_err());
    }
    let Op::Restore(restore) = serde_json::from_value(operation("edge")).unwrap() else {
        unreachable!()
    };
    let mut record = restore.record().clone();
    let khive_changeset::DeletePreimage::Edge(row) = &mut record else {
        unreachable!()
    };
    row.weight = f64::NAN;
    assert!(RestoreOp::new(restore.target_id(), record).is_err());
}

#[test]
fn version_two_refuses_legacy_headers_and_cannot_be_encoded_as_version_one() {
    let op: Op = serde_json::from_value(operation("entity")).unwrap();
    let mut cs = ChangeSet::new(envelope(), vec![op]);
    let encoded = to_ndjson(&cs).unwrap();
    assert_eq!(cs.envelope.schema_version, 2);
    assert!(matches!(
        legacy_v1::from_ndjson(&encoded),
        Err(ChangeSetError::UnsupportedSchemaVersion {
            found: 2,
            expected: 1
        })
    ));
    cs.envelope.schema_version = 1;
    assert!(matches!(
        to_ndjson(&cs),
        Err(ChangeSetError::UnsupportedSchemaVersion {
            found: 1,
            expected: 2
        })
    ));
    let legacy = format!(
        "{}\nnot even an operation\n",
        serde_json::to_string(&cs.envelope).unwrap()
    );
    assert!(matches!(
        from_ndjson(&legacy),
        Err(ChangeSetError::UnsupportedSchemaVersion {
            found: 1,
            expected: 2
        })
    ));
}

// Freeze the v1 dispatch function: version refusal occurs before any op parser
// can run. Op itself is irrelevant for the tested unsupported-v2 header.
mod legacy_v1 {
    use khive_changeset::{ChangeSet, ChangeSetError, Envelope, Op};
    const CURRENT_SCHEMA_VERSION: u32 = 1;
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
}
