/// A committed atomic unit's success output must
/// carry a canonical-shaped `result` per op (ADR-099 D4), not just
/// `{ok, tool, op_index}`. Exercises all five v1-admissible verbs in one
/// unit and asserts the relevant field for each:
/// updated name for `update`, the deleted marker for `delete`, edge
/// fields for `link`, and the transition/completion shape for the two
/// gtd verbs.
#[tokio::test]
async fn atomic_success_results_carry_canonical_shaped_result_per_op() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let khive_cfg = KhiveConfig::default();

    // `transition_task_id` and `complete_task_id` are DELIBERATELY two
    // separate tasks, not one task chained through both verbs: every
    // op's prepare pass reads state BEFORE the atomic unit applies any
    // statement (ADR-099 D1 — prepare is async/read-only, commit is the
    // one synchronous pass), so a `gtd.transition` and a `gtd.complete`
    // on the SAME task in the SAME unit would race against each other's
    // as-yet-uncommitted write, not compose sequentially.
    let (entity_id, doomed_id, source_id, target_id, transition_task_id, complete_task_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            concat!(
                r#"[create(kind="concept", name="ResultUpdate"), "#,
                r#"create(kind="concept", name="ResultDelete"), "#,
                r#"create(kind="concept", name="ResultLinkSource"), "#,
                r#"create(kind="concept", name="ResultLinkTarget")]"#,
            ),
        )
        .await;
        let id = |i: usize| {
            resp["results"][i]["result"]["id"]
                .as_str()
                .expect("id")
                .to_string()
        };
        let resp = dispatch_json(
            &server,
            r#"gtd.assign(title="ResultTransitionTask", status="next")"#,
        )
        .await;
        let transition_task_id = resp["results"][0]["result"]["full_id"]
            .as_str()
            .expect("task full_id")
            .to_string();
        let resp = dispatch_json(
            &server,
            r#"gtd.assign(title="ResultCompleteTask", status="active")"#,
        )
        .await;
        let complete_task_id = resp["results"][0]["result"]["full_id"]
            .as_str()
            .expect("task full_id")
            .to_string();
        (
            id(0),
            id(1),
            id(2),
            id(3),
            transition_task_id,
            complete_task_id,
        )
    };

    let ops = vec![
        atomic_op(
            "update",
            serde_json::json!({"id": entity_id, "name": "ResultUpdate-renamed"}),
        ),
        atomic_op("delete", serde_json::json!({"id": doomed_id})),
        atomic_op(
            "link",
            serde_json::json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": "extends",
            }),
        ),
        atomic_op(
            "gtd.transition",
            serde_json::json!({"id": transition_task_id, "status": "active"}),
        ),
        atomic_op(
            "gtd.complete",
            serde_json::json!({"id": complete_task_id, "result": "shipped"}),
        ),
    ];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("all five v1-admissible verbs must commit as one unit");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let results = envelope["results"].as_array().expect("results array");
    assert_eq!(results.len(), 5, "envelope: {envelope}");

    assert_eq!(
        results[0]["result"]["name"], "ResultUpdate-renamed",
        "update result must carry the updated name: {envelope}"
    );

    assert_eq!(
        results[1]["result"]["deleted"], true,
        "delete result: {envelope}"
    );
    assert_eq!(
        results[1]["result"]["id"], doomed_id,
        "delete result must echo the caller's id: {envelope}"
    );

    assert_eq!(
        results[2]["result"]["relation"], "extends",
        "link result must carry the edge's relation: {envelope}"
    );
    assert_eq!(
        results[2]["result"]["source_id"], source_id,
        "link result must carry source_id: {envelope}"
    );
    assert_eq!(
        results[2]["result"]["target_id"], target_id,
        "link result must carry target_id: {envelope}"
    );

    assert_eq!(
        results[3]["result"]["transitioned"], true,
        "gtd.transition result: {envelope}"
    );
    assert_eq!(
        results[3]["result"]["to"], "active",
        "gtd.transition result must carry the new status: {envelope}"
    );

    assert_eq!(
        results[4]["result"]["completed"], true,
        "gtd.complete result: {envelope}"
    );
    assert_eq!(
        results[4]["result"]["to"], "done",
        "gtd.complete result must carry the terminal status: {envelope}"
    );
}
