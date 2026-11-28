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

fn record_json(keys: serde_json::Value, aliases: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "namespace": "local",
        "created_at": 1,
        "updated_at": 1,
        "kind": "concept",
        "name": "x",
        "keys": keys,
        "aliases": aliases
    })
}

#[test]
fn an_entity_record_naming_a_key_or_an_alias_twice_is_refused() {
    use serde_json::json;
    let error = serde_json::from_value::<EntityRecord>(record_json(json!(["k1", "k1"]), json!([])))
        .unwrap_err()
        .to_string();
    assert!(error.contains("keys name \"k1\" more than once"), "{error}");
    // One alias twice is refused whatever the origins.
    let twice = json!([
        {"alias": "nick", "origin": "caller"},
        {"alias": "nick", "origin": "rename"}
    ]);
    let error = serde_json::from_value::<EntityRecord>(record_json(json!([]), twice.clone()))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("aliases name \"nick\" more than once"),
        "{error}"
    );
    // A delete or restore preimage and both sides of a merge read the record the same way.
    let mut preimage = record_json(json!(["k1", "k1"]), json!([]));
    preimage["substrate"] = "entity".into();
    let error = serde_json::from_value::<crate::DeletePreimage>(preimage)
        .unwrap_err()
        .to_string();
    assert!(error.contains("keys name \"k1\" more than once"), "{error}");
    for (into, from) in [
        (
            record_json(json!([]), twice.clone()),
            record_json(json!([]), json!([])),
        ),
        (
            record_json(json!([]), json!([])),
            record_json(json!([]), twice),
        ),
    ] {
        let merge = json!({"into": into, "from": from, "incident_edges": []});
        let error = serde_json::from_value::<crate::MergePreimage>(merge)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("aliases name \"nick\" more than once"),
            "{error}"
        );
    }
    // Distinct names, including two that differ only by case, are held.
    let held = serde_json::from_value::<EntityRecord>(record_json(
        json!(["k1", "K1"]),
        json!([
            {"alias": "nick", "origin": "caller"},
            {"alias": "Nick", "origin": "caller"}
        ]),
    ))
    .unwrap();
    assert_eq!(held.keys.len(), 2);
    assert_eq!(held.aliases.len(), 2);
}
