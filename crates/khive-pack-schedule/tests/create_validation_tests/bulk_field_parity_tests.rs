#[tokio::test]
async fn schedule_bulk_create_check_accepts_every_field_of_the_kg_bulk_entry() {
    let (registry, _rt) = build_registry();

    let items: Vec<serde_json::Value> = KG_BULK_ENTRY_ITEMS
        .iter()
        .map(|raw| serde_json::from_str(raw).expect("item is valid JSON"))
        .collect();

    let fields = kg_bulk_entry_field_names();
    assert!(
        fields.len() > 1,
        "no BulkCreateEntry fields were read: {fields:?}"
    );
    for field in &fields {
        assert!(
            items.iter().any(|item| item.get(field.as_str()).is_some()),
            "no KG_BULK_ENTRY_ITEMS item uses BulkCreateEntry field {field:?}: add one, and \
             accept the field in ScheduleBulkCreateEntryCheck"
        );
    }

    for item in items {
        let batch = serde_json::json!([item]);

        registry
            .dispatch("create", serde_json::json!({ "items": batch }))
            .await
            .unwrap_or_else(|e| panic!("sanity: the KG handler rejected {batch}: {e}"));

        schedule_bulk_create(&registry, &batch)
            .await
            .unwrap_or_else(|e| panic!("schedule.schedule rejected {batch}: {e}"));
    }
}
