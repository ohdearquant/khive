use super::*;

#[test]
fn strict_entity_rejects_unknown_field() {
    let json = serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "namespace": "local",
        "created_at": 1,
        "updated_at": 1,
        "kind": "concept",
        "entity_type": null,
        "name": "x",
        "description": null,
        "properties": {},
        "tags": [],
        "deleted_at": null,
        "unexpected": true
    });
    let result: Result<StrictEntity, _> = serde_json::from_value(json);
    assert!(result.is_err());
}

#[test]
fn strict_link_rejects_out_of_range_weight() {
    let json = serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "namespace": "local",
        "source": "00000000-0000-0000-0000-000000000002",
        "target": "00000000-0000-0000-0000-000000000003",
        "relation": "extends",
        "properties": {},
        "weight": 1.5,
        "created_at": 1,
        "updated_at": 1,
        "deleted_at": null
    });
    let strict: StrictLink = serde_json::from_value(json).unwrap();
    let result: Result<Link, _> = strict.try_into();
    assert!(result.is_err());
}

#[test]
fn strict_note_rejects_out_of_range_salience() {
    let json = serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "namespace": "local",
        "created_at": 1,
        "updated_at": 1,
        "kind": "observation",
        "status": "active",
        "content": "x",
        "properties": {},
        "tags": [],
        "salience": 1.5,
        "decay_factor": null,
        "expires_at": null,
        "deleted_at": null
    });
    let strict: StrictNote = serde_json::from_value(json).unwrap();
    let result: Result<Note, _> = strict.try_into();
    assert!(result.is_err());
}
