use serde_json::{json, Value};

#[tokio::test]
async fn entity_update_response_names_committed_unfinished_indexes() {
    let rt = khive_runtime::KhiveRuntime::memory().unwrap();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    let registry = builder.build().unwrap();
    rt.install_edge_rules(registry.all_edge_rules());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let pack = crate::KgPack::new(rt.clone());
    let entity = khive_storage::Entity::new("local", "concept", "before pending response");
    rt.entities(&token)
        .unwrap()
        .upsert_entity(entity.clone())
        .await
        .unwrap();
    rt.text(&token).unwrap();
    let mut writer = rt.sql().writer().await.unwrap();
    writer
        .execute_script(
            "CREATE TRIGGER fail_pending_fts BEFORE INSERT ON fts_entities_rowids \
         BEGIN SELECT RAISE(ABORT, 'forced FTS map failure'); END;"
                .into(),
        )
        .await
        .unwrap();
    drop(writer);
    let response = pack
        .handle_update(
            &token,
            json!({
                "id": entity.id, "kind": "entity", "name": "committed response name"
            }),
            &registry,
        )
        .await
        .expect("index failure retains the committed entity result");
    assert_eq!(response["id"], entity.id.to_string());
    assert_eq!(response["name"], "committed response name");
    assert_eq!(response["indexing"]["status"], "unfinished");
    assert_eq!(
        response["indexing"]["unfinished"][0]["target"]["kind"],
        "fts"
    );
    assert_eq!(
        response["indexing"]["unfinished"][0]["stage"],
        "index_publish"
    );
    assert_eq!(
        rt.entities(&token)
            .unwrap()
            .get_entity(entity.id)
            .await
            .unwrap()
            .unwrap()
            .version,
        entity.version + 1
    );
    let mut reader = rt.sql().reader().await.unwrap();
    let row = reader
        .query_row(khive_storage::SqlStatement::new(
            "SELECT targets_json FROM pending_entity_indexes WHERE entity_id=?1",
            vec![khive_storage::SqlValue::Text(entity.id.to_string())],
        ))
        .await
        .unwrap()
        .unwrap();
    let targets: Value = serde_json::from_str(row.text("targets_json").unwrap()).unwrap();
    assert_eq!(targets[0]["done"], false);
}
