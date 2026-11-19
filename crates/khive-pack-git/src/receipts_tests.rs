use super::*;

async fn runtime() -> KhiveRuntime {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let mut writer = rt.sql().writer().await.expect("writer");
    for sql in [
        RECEIPTS_TABLE_SQL,
        RECEIPTS_ACTOR_INDEX_SQL,
        RECEIPTS_SESSION_INDEX_SQL,
    ] {
        writer
            .execute(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: Some("git_receipts_test_schema".into()),
            })
            .await
            .expect("receipt schema");
    }
    drop(writer);
    rt
}

fn receipt() -> Receipt {
    let mut receipt = Receipt::new(
        "local",
        "actor:a",
        "git.commit",
        "/repo",
        json!({"branch": "work", "expected_head": "1".repeat(40)}),
        json!({"decision": "allow", "source": "git_write.allowed", "id": 0}),
        json!({"decision": "allow", "source": "policy", "id": "policy-a"}),
    );
    receipt.session_id = Some("session-a".into());
    receipt
}

#[tokio::test]
async fn unknown_candidate_is_durable_and_terminal_settlement_cannot_be_overwritten() {
    let rt = runtime().await;
    let mut pending = receipt();
    insert(&rt, &pending).await.expect("insert unknown");
    let original = load_owned(&rt, "local", "actor:a", &pending.id)
        .await
        .expect("load unknown");
    assert_eq!(original.disposition, Disposition::Unknown);
    assert_eq!(original.finished_at, None);
    assert_eq!(original.to_value(), pending.to_value());

    pending.result = json!({"sha": "2".repeat(40), "receipt_id": pending.id});
    persist(&rt, &pending)
        .await
        .expect("persist prospective sha");
    let mut settled = load_owned(&rt, "local", "actor:a", &pending.id)
        .await
        .expect("load candidate");
    assert_eq!(settled.result, pending.result);
    settled.disposition = Disposition::Committed;
    settled.finished_at = Some(khive_storage::now_micros());
    persist(&rt, &settled).await.expect("settle committed");

    assert!(persist(&rt, &pending).await.is_err());
    assert!(persist(&rt, &settled).await.is_err());
    let durable = load_owned(&rt, "local", "actor:a", &pending.id)
        .await
        .expect("load settled");
    assert_eq!(durable.to_value(), settled.to_value());
    assert!(matches!(
        load_owned(&rt, "local", "actor:b", &pending.id).await,
        Err(RuntimeError::NotFound(_))
    ));
    assert!(matches!(
        load_owned(&rt, "other", "actor:a", &pending.id).await,
        Err(RuntimeError::NotFound(_))
    ));
}

#[tokio::test]
async fn pages_use_insertion_order_and_exact_lookahead_with_owner_filters() {
    let rt = runtime().await;
    let mut first = receipt();
    first.started_at = 200;
    insert(&rt, &first).await.expect("first");
    for (namespace, actor, repo, session) in [
        ("other", "actor:a", "/repo", "session-a"),
        ("local", "actor:b", "/repo", "session-a"),
        ("local", "actor:a", "/other", "session-a"),
        ("local", "actor:a", "/repo", "session-b"),
    ] {
        let mut excluded = receipt();
        excluded.namespace = namespace.into();
        excluded.actor = actor.into();
        excluded.repo = repo.into();
        excluded.session_id = Some(session.into());
        insert(&rt, &excluded).await.expect("excluded receipt");
    }
    let mut second = receipt();
    second.started_at = 100;
    insert(&rt, &second).await.expect("second");
    let page = list_owned(
        &rt,
        "local",
        "actor:a",
        Some("/repo"),
        Some("session-a"),
        1,
        0,
    )
    .await
    .expect("first page");
    assert_eq!(page.receipts.len(), 1);
    assert_eq!(page.receipts[0].id, first.id);
    assert_eq!(page.next_offset, Some(1));

    let mut third = receipt();
    third.started_at = 50;
    insert(&rt, &third)
        .await
        .expect("appended after first page");
    let page = list_owned(
        &rt,
        "local",
        "actor:a",
        Some("/repo"),
        Some("session-a"),
        2,
        1,
    )
    .await
    .expect("last page");
    assert_eq!(page.receipts.len(), 2);
    assert_eq!(page.receipts[0].id, second.id);
    assert_eq!(page.receipts[1].id, third.id);
    assert_eq!(page.next_offset, None);
    let all = list_owned(&rt, "local", "actor:a", None, None, 10, 0)
        .await
        .expect("optional filters");
    assert_eq!(all.receipts.len(), 5);
    assert_eq!(all.next_offset, None);
}

#[tokio::test]
async fn malformed_stored_row_fails_the_page_instead_of_disappearing() {
    let rt = runtime().await;
    let good = receipt();
    let bad = receipt();
    insert(&rt, &good).await.expect("good");
    insert(&rt, &bad).await.expect("bad fixture");
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "UPDATE git_receipts SET policy = '[]' WHERE id = ?1".into(),
            params: vec![SqlValue::Text(bad.id.clone())],
            label: Some("git_receipts_test_corrupt".into()),
        })
        .await
        .expect("corrupt shape with valid JSON");
    drop(writer);
    assert!(list_owned(&rt, "local", "actor:a", None, None, 10, 0)
        .await
        .is_err());
    assert!(load_owned(&rt, "local", "actor:a", &bad.id).await.is_err());
    assert!(load_owned(&rt, "local", "actor:a", &good.id).await.is_ok());
}

#[tokio::test]
async fn writes_reject_terminal_insert_and_changed_operation_identity() {
    let rt = runtime().await;
    let mut pending = receipt();
    let mut terminal = pending.clone();
    terminal.disposition = Disposition::NotCommitted;
    terminal.finished_at = Some(terminal.started_at);
    assert!(insert(&rt, &terminal).await.is_err());
    insert(&rt, &pending).await.expect("initial unknown");
    assert!(insert(&rt, &pending).await.is_err());
    pending.inputs["branch"] = json!("different");
    assert!(persist(&rt, &pending).await.is_err());
    let durable = load_owned(&rt, "local", "actor:a", &pending.id)
        .await
        .expect("original receipt");
    assert_eq!(durable.inputs["branch"], "work");
    assert_eq!(durable.disposition, Disposition::Unknown);
}
