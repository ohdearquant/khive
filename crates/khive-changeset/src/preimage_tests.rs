use std::collections::BTreeMap;

use khive_types::{
    EdgeRelation, EntityKind, Header, Id128, Namespace, NoteStatus, PropertyValue, Timestamp,
};
use serde_json::{json, Value};

use super::*;

fn entity() -> Entity {
    Entity {
        header: Header::new(
            Id128::from_u128(1),
            Namespace::local(),
            Timestamp::from_micros(1_000_001),
        ),
        kind: EntityKind::Concept,
        entity_type: Some("term".into()),
        name: "original".into(),
        description: Some("description".into()),
        properties: BTreeMap::from([
            (
                "integer".into(),
                PropertyValue::Integer(9_007_199_254_740_993),
            ),
            ("nullable".into(), PropertyValue::Null),
            (
                "nested".into(),
                PropertyValue::Object(BTreeMap::from([(
                    "kept".into(),
                    PropertyValue::Boolean(true),
                )])),
            ),
        ]),
        tags: vec!["one".into(), "two".into()],
        deleted_at: None,
    }
}

fn note() -> Note {
    Note {
        header: entity().header,
        kind: "observation".into(),
        status: NoteStatus::Active,
        content: "original content".into(),
        properties: entity().properties,
        tags: entity().tags,
        salience: Some(0.5),
        decay_factor: Some(0.125),
        expires_at: None,
        deleted_at: None,
    }
}

fn edge() -> Link {
    Link {
        id: Id128::from_u128(1),
        namespace: "local".into(),
        source: Id128::from_u128(2),
        target: Id128::from_u128(3),
        relation: EdgeRelation::Extends,
        properties: entity().properties,
        weight: 0.75,
        created_at: Timestamp::from_micros(1_000_001),
        updated_at: Timestamp::from_micros(1_000_001),
        deleted_at: None,
    }
}

fn update(substrate: &str, field: &str, captured: Value) -> UpdateOp {
    let mut fields = json!({"target": substrate});
    fields[field] = captured;
    serde_json::from_value(json!({
        "target_id": Id128::from_u128(1),
        "patch": fields,
        "preimage": fields,
    }))
    .unwrap()
}

fn mismatch(result: Result<(), PreimageMismatch>, field: &str) {
    assert_eq!(
        result.unwrap_err(),
        PreimageMismatch {
            field_path: field.into()
        }
    );
}

#[test]
fn every_entity_update_field_is_compared_without_other_fields() {
    let original = entity();
    for (field, changed) in [
        ("name", json!("later name")),
        ("description", Value::Null),
        ("properties", json!({"other": true})),
        ("tags", json!(["two", "one"])),
        ("entity_type", Value::Null),
    ] {
        let mut value = serde_json::to_value(&original).unwrap();
        let op = update("entity", field, value[field].clone());
        assert_eq!(
            check_update(&op, CanonicalRecordRef::Entity(&original)),
            Ok(())
        );
        value[field] = changed;
        let changed = serde_json::from_value(value).unwrap();
        mismatch(
            check_update(&op, CanonicalRecordRef::Entity(&changed)),
            field,
        );
    }
    let op = update("entity", "name", json!(original.name));
    let mut live = original;
    live.description = Some("concurrent description".into());
    live.header.updated_at = Timestamp::MAX;
    live.properties.clear();
    assert_eq!(check_update(&op, CanonicalRecordRef::Entity(&live)), Ok(()));
}

#[test]
fn every_note_update_field_is_compared_without_other_fields() {
    let original = note();
    for (field, changed) in [
        ("content", json!("later content")),
        ("salience", Value::Null),
        ("decay_factor", Value::Null),
        ("properties", json!({})),
        ("tags", json!(["two", "one"])),
    ] {
        let mut value = serde_json::to_value(&original).unwrap();
        let op = update("note", field, value[field].clone());
        assert_eq!(
            check_update(&op, CanonicalRecordRef::Note(&original)),
            Ok(())
        );
        value[field] = changed;
        let changed = serde_json::from_value(value).unwrap();
        mismatch(check_update(&op, CanonicalRecordRef::Note(&changed)), field);
    }
    let op = update("note", "content", json!(original.content));
    let mut live = original;
    live.salience = None;
    live.status = NoteStatus::Archived;
    assert_eq!(check_update(&op, CanonicalRecordRef::Note(&live)), Ok(()));
}

#[test]
fn edge_comparison_retains_weight_precision_and_ignores_untouched_fields() {
    let original = edge();
    let op = update("edge", "weight", json!(original.weight));
    let mut live = original.clone();
    live.weight = f64::from_bits(live.weight.to_bits() + 1);
    assert_eq!(original.weight as f32, live.weight as f32);
    mismatch(check_update(&op, CanonicalRecordRef::Edge(&live)), "weight");
    let op = update("edge", "relation", json!(original.relation));
    live.relation = EdgeRelation::Supersedes;
    mismatch(
        check_update(&op, CanonicalRecordRef::Edge(&live)),
        "relation",
    );
    live.relation = original.relation;
    live.source = Id128::from_u128(99);
    assert_eq!(check_update(&op, CanonicalRecordRef::Edge(&live)), Ok(()));
}

#[test]
fn nullable_capture_and_an_absent_field_are_different() {
    let mut live = entity();
    live.description = None;
    let captured_null = update("entity", "description", Value::Null);
    assert_eq!(
        check_update(&captured_null, CanonicalRecordRef::Entity(&live)),
        Ok(())
    );
    live.description = Some(String::new());
    mismatch(
        check_update(&captured_null, CanonicalRecordRef::Entity(&live)),
        "description",
    );
    let untouched = update("entity", "name", json!(live.name));
    assert_eq!(
        check_update(&untouched, CanonicalRecordRef::Entity(&live)),
        Ok(())
    );
    let mut live = note();
    live.salience = None;
    live.decay_factor = None;
    for field in ["salience", "decay_factor"] {
        let op = update("note", field, Value::Null);
        assert_eq!(check_update(&op, CanonicalRecordRef::Note(&live)), Ok(()));
    }
}

#[test]
fn properties_compare_the_whole_typed_map_not_patch_keys_or_display_values() {
    let original = entity();
    let mut op: Value = serde_json::to_value(update(
        "entity",
        "properties",
        serde_json::to_value(&original.properties).unwrap(),
    ))
    .unwrap();
    op["patch"]["properties"] = json!({"replacement": true});
    let op: UpdateOp = serde_json::from_value(op).unwrap();
    let mut live = original.clone();
    live.properties
        .insert("unmentioned".into(), PropertyValue::Null);
    mismatch(
        check_update(&op, CanonicalRecordRef::Entity(&live)),
        "properties",
    );
    live = original.clone();
    live.properties.insert(
        "integer".into(),
        PropertyValue::Float(9_007_199_254_740_992.0),
    );
    mismatch(
        check_update(&op, CanonicalRecordRef::Entity(&live)),
        "properties",
    );
    live = original;
    live.properties.insert(
        "nested".into(),
        PropertyValue::Object(BTreeMap::from([(
            "kept".into(),
            PropertyValue::Boolean(false),
        )])),
    );
    mismatch(
        check_update(&op, CanonicalRecordRef::Entity(&live)),
        "properties",
    );
}

#[test]
fn update_rejects_another_record_or_substrate_without_disclosing_values() {
    let live = entity();
    let op = update("entity", "name", json!(live.name));
    let mut other = live.clone();
    other.header.id = Id128::from_u128(999);
    mismatch(check_update(&op, CanonicalRecordRef::Entity(&other)), "id");
    mismatch(
        check_update(&op, CanonicalRecordRef::Note(&note())),
        "substrate",
    );
    other.header.id = live.header.id;
    other.name = "sensitive later name".into();
    let error = check_update(&op, CanonicalRecordRef::Entity(&other)).unwrap_err();
    assert_eq!(error.to_string(), "captured preimage differs at name");
    assert!(!format!("{error:?}").contains("sensitive"));
}

#[test]
fn entity_delete_compares_every_captured_field_except_updated_at() {
    let original = entity();
    let op = DeleteOp {
        target_id: original.header.id,
        hard: false,
        preimage: DeletePreimage::Entity(Box::new(original.clone())),
    };
    let mut live = original.clone();
    live.header.updated_at = Timestamp::MAX;
    assert_eq!(check_delete(&op, CanonicalRecordRef::Entity(&live)), Ok(()));
    for (field, changed) in [
        ("id", json!(Id128::from_u128(99))),
        ("namespace", json!("other")),
        ("created_at", json!(1_000_002_u64)),
        ("kind", json!("dataset")),
        ("entity_type", Value::Null),
        ("name", json!("changed")),
        ("description", Value::Null),
        ("properties", json!({})),
        ("tags", json!(["two", "one"])),
        ("deleted_at", json!(2_000_001_u64)),
    ] {
        let mut value = serde_json::to_value(&original).unwrap();
        value[field] = changed;
        let live = serde_json::from_value(value).unwrap();
        mismatch(check_delete(&op, CanonicalRecordRef::Entity(&live)), field);
    }
}

#[test]
fn note_delete_compares_every_captured_field_except_updated_at() {
    let original = note();
    let op = DeleteOp {
        target_id: original.header.id,
        hard: true,
        preimage: DeletePreimage::Note(Box::new(original.clone())),
    };
    let mut live = original.clone();
    live.header.updated_at = Timestamp::MAX;
    assert_eq!(check_delete(&op, CanonicalRecordRef::Note(&live)), Ok(()));
    for (field, changed) in [
        ("id", json!(Id128::from_u128(99))),
        ("namespace", json!("other")),
        ("created_at", json!(1_000_002_u64)),
        ("kind", json!("insight")),
        ("status", json!("archived")),
        ("content", json!("changed")),
        ("properties", json!({})),
        ("tags", json!(["two", "one"])),
        ("salience", Value::Null),
        ("decay_factor", Value::Null),
        ("expires_at", json!(2_000_001_u64)),
        ("deleted_at", json!(2_000_001_u64)),
    ] {
        let mut value = serde_json::to_value(&original).unwrap();
        value[field] = changed;
        let live = serde_json::from_value(value).unwrap();
        mismatch(check_delete(&op, CanonicalRecordRef::Note(&live)), field);
    }
}

#[test]
fn edge_delete_compares_every_captured_field_except_updated_at() {
    let original = edge();
    let op = DeleteOp {
        target_id: original.id,
        hard: true,
        preimage: DeletePreimage::Edge(Box::new(original.clone())),
    };
    let mut live = original.clone();
    live.updated_at = Timestamp::MAX;
    assert_eq!(check_delete(&op, CanonicalRecordRef::Edge(&live)), Ok(()));
    for (field, changed) in [
        ("id", json!(Id128::from_u128(99))),
        ("namespace", json!("other")),
        ("source", json!(Id128::from_u128(98))),
        ("target", json!(Id128::from_u128(97))),
        ("relation", json!("supersedes")),
        ("properties", json!({})),
        (
            "weight",
            json!(f64::from_bits(original.weight.to_bits() + 1)),
        ),
        ("created_at", json!(1_000_002_u64)),
        ("deleted_at", json!(2_000_001_u64)),
    ] {
        let mut value = serde_json::to_value(&original).unwrap();
        value[field] = changed;
        let live = serde_json::from_value(value).unwrap();
        mismatch(check_delete(&op, CanonicalRecordRef::Edge(&live)), field);
    }
}

#[test]
fn delete_rejects_wrong_substrate_and_unchecked_target_capture_disagreement() {
    let live = entity();
    let op = DeleteOp {
        target_id: Id128::from_u128(999),
        hard: false,
        preimage: DeletePreimage::Entity(Box::new(live.clone())),
    };
    mismatch(check_delete(&op, CanonicalRecordRef::Entity(&live)), "id");
    mismatch(
        check_delete(&op, CanonicalRecordRef::Edge(&edge())),
        "substrate",
    );
    let mut captured = live.clone();
    captured.header.id = op.target_id;
    let op = DeleteOp {
        target_id: live.header.id,
        hard: false,
        preimage: DeletePreimage::Entity(Box::new(captured)),
    };
    mismatch(check_delete(&op, CanonicalRecordRef::Entity(&live)), "id");
}
