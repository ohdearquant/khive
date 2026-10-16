/// Atomic `update(kind=...)` resolves through the same kind expectation as
/// `delete(kind=...)`: a mismatching kind is a NotFound-shaped rejection that
/// leaves the record unchanged, an event/proposal kind is refused by name with
/// the update verb in the message, and a matching kind commits.
#[tokio::test]
async fn atomic_update_rejects_kind_mismatch_and_unsupported_kind_and_accepts_matching_kind() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let khive_cfg = KhiveConfig::default();

    let entity_id = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"create(kind="concept", name="UpdateKindOriginal")"#,
        )
        .await;
        resp["results"][0]["result"]["id"]
            .as_str()
            .expect("id")
            .to_string()
    };

    // (a) kind mismatch: entity, caller says "note" — rejected, name unchanged.
    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": entity_id, "kind": "note", "name": "UpdateKindMismatch"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("update(kind=\"note\") on an entity must be rejected");
    assert!(
        format!("{err:#}").contains("not found"),
        "expected a NotFound-shaped rejection, error: {err:#}"
    );

    // (b) event kind: refused by name, and the message names the update verb.
    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": entity_id, "kind": "event", "name": "UpdateKindEvent"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("update(kind=\"event\") must be refused under --atomic");
    assert!(
        format!("{err:#}").contains("not supported under --atomic update"),
        "expected the update verb in the refusal, error: {err:#}"
    );

    let server = isolated_server(&db_path);
    let resp = dispatch_json(&server, &format!(r#"get(id="{entity_id}")"#)).await;
    assert_eq!(
        resp["results"][0]["result"]["name"], "UpdateKindOriginal",
        "name must be unchanged after both rejections: {resp}"
    );

    // (c) matching kind: commits and the name changes.
    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": entity_id, "kind": "entity", "name": "UpdateKindMatching"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("update(kind=\"entity\") on an entity must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
    let server = isolated_server(&db_path);
    let resp = dispatch_json(&server, &format!(r#"get(id="{entity_id}")"#)).await;
    assert_eq!(
        resp["results"][0]["result"]["name"], "UpdateKindMatching",
        "name must change after a matching-kind update: {resp}"
    );
}
