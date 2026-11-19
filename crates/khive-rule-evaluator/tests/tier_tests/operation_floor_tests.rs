#[test]
fn every_touched_mutable_field_and_nullable_clear_must_be_admitted() {
    for (field, before, after) in [
        ("name", json!("Before"), json!("After")),
        ("description", json!("Before"), Value::Null),
        ("properties", json!({"old":true}), json!({"new":true})),
        ("tags", json!(["old"]), json!(["new"])),
        ("entity_type", json!("old"), Value::Null),
    ] {
        let mut patch = json!({"target":"entity"});
        patch[field] = after;
        let mut preimage = json!({"target":"entity"});
        preimage[field] = before;
        let op = update(patch, preimage);
        let config = format!("[tier]\ntier_1_entity_fields=['{field}']");
        assert_eq!(
            classify(&config, vec![op.clone()], &[])[0].tier,
            Tier::Tier1,
            "{field}"
        );
        let result = classify("[tier]\ntier_1_entity_fields=[]", vec![op], &[]);
        assert_eq!(result[0].tier, Tier::Tier2, "{field}");
        assert!(result[0].reason.contains(field));
    }
    let op = update(
        json!({"target":"entity", "name":"After", "description":null}),
        json!({"target":"entity", "name":"Before", "description":"Before"}),
    );
    assert_eq!(
        classify("[tier]\ntier_1_entity_fields=['name']", vec![op], &[])[0].tier,
        Tier::Tier2
    );
}

#[test]
fn weight_boundary_floor_and_configured_higher_threshold_hold() {
    assert_eq!(
        classify("", vec![link("contains", 0.7)], &[])[0].tier,
        Tier::Tier1
    );
    let below = classify(
        "[tier]\nminimum_weight=0.0",
        vec![link("contains", 0.69)],
        &[],
    );
    assert_eq!(below[0].tier, Tier::Tier2);
    assert_eq!(below[0].reason_ids, ["adr102.resulting_weight_below_0_7"]);
    let narrowed = classify(
        "[tier]\nminimum_weight=0.9",
        vec![link("contains", 0.8)],
        &[],
    );
    assert_eq!(narrowed[0].tier, Tier::Tier2);
    assert!(narrowed[0]
        .reason_ids
        .iter()
        .any(|id| id == "config.resulting_weight_below_threshold"));
}

#[test]
fn destructive_and_existing_edge_updates_stay_tier_two_when_admitted() {
    let config =
        "[tier]\ntier_1_operations=['create','link','update','delete','merge']\nminimum_weight=0.0";
    let ops = vec![
        delete("entity"),
        delete("note"),
        delete("edge"),
        merge(),
        update(
            json!({"target":"edge", "relation":"extends"}),
            json!({"target":"edge", "relation":"contains"}),
        ),
        update(
            json!({"target":"edge", "weight":1.0}),
            json!({"target":"edge", "weight":0.9}),
        ),
        update(
            json!({"target":"note", "content":"After"}),
            json!({"target":"note", "content":"Before"}),
        ),
    ];
    assert!(classify(config, ops, &[])
        .iter()
        .all(|decision| decision.tier == Tier::Tier2));
}
