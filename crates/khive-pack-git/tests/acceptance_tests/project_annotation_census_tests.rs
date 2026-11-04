async fn project_annotation_checkpoint_snapshot(
    runtime: &KhiveRuntime,
    project_id: Uuid,
) -> (String, String) {
    let sql = runtime.sql();
    let row = sql
        .reader()
        .await
        .expect("checkpoint reader")
        .query_row(SqlStatement {
            sql: include_str!("../../sql/page_checkpoint_select.sql").into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text("issues".into()),
                SqlValue::Text("issues_checkpoint".into()),
            ],
            label: Some("test_project_annotation_checkpoint_snapshot".into()),
        })
        .await
        .expect("read checkpoint snapshot")
        .expect("checkpoint projection row");
    let text = |field| match row.get(field) {
        Some(SqlValue::Text(value)) => value.clone(),
        _ => panic!("expected seeded checkpoint text field {field}"),
    };
    (text("floor"), text("progress"))
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn project_annotation_census_reaches_notes_outside_the_remote_window() {
    let (runtime, token, registry) = fixture().await;
    let project_id = create(&registry, json!({"kind": "project", "name": "census-project"})).await;
    let other_project = create(&registry, json!({"kind": "project", "name": "other-project"})).await;
    let floor = "2026-01-02T00:00:00Z";

    let mut missing_ids = std::collections::BTreeSet::new();
    let mut acknowledged_id = None;
    for (number, kind, updated_at) in [
        (1, "issue", "2026-01-01T00:00:00Z"),
        (2, "pull_request", "2026-01-03T00:00:00Z"),
        (3, "issue", floor),
    ] {
        let id = create(
            &registry,
            json!({
                "kind": kind,
                "content": "",
                "properties": {
                    "number": number,
                    "title": format!("unlinked {kind} {number}"),
                    "updated_at": updated_at,
                    "project_id": project_id.to_string()
                }
            }),
        )
        .await;
        missing_ids.insert(id.to_string());
        if number == 3 {
            acknowledged_id = Some(id);
        }
    }
    let linked = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 4, "title": "linked", "project_id": project_id.to_string()},
            "annotates": [project_id.to_string()]
        }),
    )
    .await;
    create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 5, "title": "other project", "project_id": other_project.to_string()}
        }),
    )
    .await;

    let at_floor = std::collections::BTreeMap::from([(
        "3".to_string(),
        acknowledged_id.expect("at-floor fixture ID").to_string(),
    )]);
    let progress = json!({
        "version": 1,
        "namespace": token.namespace().as_str(),
        "floor": floor,
        "at_floor": at_floor,
        "undated": {}
    })
    .to_string();
    runtime
        .sql()
        .writer()
        .await
        .expect("checkpoint writer")
        .execute(SqlStatement {
            sql: include_str!("../../sql/page_checkpoint_with_floor_upsert.sql").into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text("issues_checkpoint".into()),
                SqlValue::Text(progress),
                SqlValue::Integer(khive_storage::now_micros()),
                SqlValue::Text("issues".into()),
                SqlValue::Text(floor.to_string()),
            ],
            label: Some("test_seed_project_annotation_checkpoint".into()),
        })
        .await
        .expect("seed acknowledged issue checkpoint");
    let before = project_annotation_checkpoint_snapshot(&runtime, project_id).await;

    // No remote or local history producer is needed: the census must run
    // even when this pass does not request any source walker.
    let report = run_ingest(
        &runtime,
        &token,
        &registry,
        IngestOptions {
            repo: PathBuf::from("."),
            expected_github_repo: None,
            project: project_id.to_string(),
            max_items: None,
            include: IngestInclude {
                commits: false,
                issues: false,
                pull_requests: false,
            },
        },
    )
    .await
    .expect("local-only ingest pass");
    let report = serde_json::to_value(report).expect("serialized ingest report");
    assert_eq!(
        report["missing_project_annotation_count"],
        json!(3),
        "the census must reach old, above-floor and acknowledged-at-floor notes"
    );
    let reported_ids = report["missing_project_annotation_note_ids"]
        .as_array()
        .expect("missing annotation IDs")
        .iter()
        .map(|id| id.as_str().expect("note ID string").to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(reported_ids, missing_ids);
    assert_eq!(report["project_annotation_notes_scanned"], json!(4));
    assert_eq!(report["project_annotation_census_complete"], json!(true));
    assert_eq!(report["done"], json!(true));
    assert_eq!(report["cursor_stalled"], json!(false));
    assert_eq!(report["issues_skipped_existing"], json!(0));
    assert_eq!(
        project_annotation_checkpoint_snapshot(&runtime, project_id).await,
        before,
        "the local census must not rewrite the remote floor or acknowledgement payload"
    );

    let neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": project_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("project annotations");
    let neighbors = neighbors.as_array().expect("annotation neighbor array");
    assert_eq!(neighbors.len(), 1, "the census must not repair any edge");
    assert_eq!(neighbors[0]["id"], json!(linked.to_string()));
}
