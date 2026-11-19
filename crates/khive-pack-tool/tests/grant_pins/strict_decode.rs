use super::*;

async fn granted_with_deny(tool: &str) -> (Fixture, String) {
    let f = Fixture::new();
    let registered = f.register(tool).await;
    let id = f.request(tool).await;
    let grant = f.grant(&id).await;
    assert_eq!(grant["registry_id"], registered["full_id"]);
    f.call(
        "tool.policy",
        json!({"actor":"agent:requester", "tool":tool, "decision":"deny"}),
    )
    .await;
    let baseline = f.check(tool).await;
    assert_eq!(baseline["decision"], "allow");
    assert_eq!(baseline["source"], "grant");
    assert_eq!(baseline["grant_id"], id);
    (f, id)
}

async fn replace_column(f: &Fixture, id: &str, column: &str, value: SqlValue, storage_class: &str) {
    f.write(
        &format!("UPDATE tool_grants SET {column}=?1 WHERE id=?2"),
        vec![value, SqlValue::Text(id.into())],
    )
    .await;
    let rows =
        f.rt.sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement {
                sql: format!("SELECT typeof({column}) AS storage_class FROM tool_grants"),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text("storage_class").unwrap(), storage_class);
}

fn assert_decode_error(error: RuntimeError, id: Option<&str>, column: &str) {
    let message = error.to_string();
    assert!(message.contains("tool_grants"), "{message}");
    assert!(
        message.contains(&format!("SQL column {column}")),
        "{message}"
    );
    if let Some(id) = id {
        assert!(message.contains(id), "{message}");
    }
}

async fn assert_expiry_refusal(f: &Fixture, id: &str, tool: &str) {
    let error = f
        .registry
        .dispatch(
            "tool.check",
            json!({"tool":tool, "actor":"agent:requester"}),
        )
        .await
        .expect_err("a selected malformed expiry must not allow or fall through to deny");
    assert_decode_error(error, Some(id), "expires_at");
}

#[tokio::test]
async fn selected_grant_rejects_text_expiry_instead_of_allowing_forever() {
    let tool = "strict-text-expiry";
    let (f, id) = granted_with_deny(tool).await;
    replace_column(
        &f,
        &id,
        "expires_at",
        SqlValue::Text("never".into()),
        "text",
    )
    .await;
    assert_expiry_refusal(&f, &id, tool).await;
}

#[tokio::test]
async fn selected_grant_rejects_blob_expiry_instead_of_allowing_forever() {
    let tool = "strict-blob-expiry";
    let (f, id) = granted_with_deny(tool).await;
    replace_column(
        &f,
        &id,
        "expires_at",
        SqlValue::Blob(b"never".to_vec()),
        "blob",
    )
    .await;
    assert_expiry_refusal(&f, &id, tool).await;
}

#[tokio::test]
async fn selected_grant_rejects_future_real_expiry_instead_of_truncating() {
    let tool = "strict-real-expiry";
    let (f, id) = granted_with_deny(tool).await;
    let future = (khive_storage::now_micros() + 60_000_000) as f64 + 0.5;
    assert!(future.fract() > 0.0);
    replace_column(&f, &id, "expires_at", SqlValue::Float(future), "real").await;
    assert_expiry_refusal(&f, &id, tool).await;
}

#[tokio::test]
async fn malformed_inactive_grant_timestamps_cannot_fall_through() {
    for (column, value) in [
        ("expires_at", -1.5),
        ("expires_at", 9223372036854775808.0),
        ("decided_at", 1.5),
        ("invalidated_at", 1.5),
    ] {
        let tool = "strict-inactive-real";
        let (f, id) = granted_with_deny(tool).await;
        replace_column(&f, &id, column, SqlValue::Float(value), "real").await;
        let error = f
            .registry
            .dispatch(
                "tool.check",
                json!({"tool": tool, "actor": "agent:requester"}),
            )
            .await
            .expect_err("matching malformed grant must not fall through");
        assert_decode_error(error, Some(&id), column);
    }
}

#[tokio::test]
async fn malformed_matching_history_precedes_active_grant_but_unrelated_actor_does_not() {
    let tool = "strict-history-real";
    let (f, id) = granted_with_deny(tool).await;
    f.write(
        "INSERT INTO tool_grants (id, namespace, actor, tool, status, requested_at, expires_at, registry_id, definition_digest, invalidated_by_registry_id, invalidated_at) SELECT 'malformed-history', namespace, actor, tool, 'revoked', requested_at, -1.5, registry_id, definition_digest, NULL, NULL FROM tool_grants WHERE id=?1",
        vec![SqlValue::Text(id.clone())],
    ).await;
    let error = f
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": tool, "actor": "agent:requester"}),
        )
        .await
        .expect_err("malformed matching history must not be hidden by the active grant");
    assert_decode_error(error, Some("malformed-history"), "expires_at");

    f.write(
        "UPDATE tool_grants SET actor=?1 WHERE id=?2",
        vec![
            SqlValue::Text("agent:other".into()),
            SqlValue::Text("malformed-history".into()),
        ],
    )
    .await;
    let decision = f.check(tool).await;
    assert_eq!(decision["decision"], "allow");
    assert_eq!(decision["grant_id"], id);

    f.write(
        "UPDATE tool_grants SET actor=?1, namespace=?2 WHERE id=?3",
        vec![
            SqlValue::Text("agent:requester".into()),
            SqlValue::Text("other".into()),
            SqlValue::Text("malformed-history".into()),
        ],
    )
    .await;
    let decision = f.check(tool).await;
    assert_eq!(decision["decision"], "allow");
    assert_eq!(decision["grant_id"], id);
}

#[tokio::test]
async fn valid_inactive_grants_do_not_allow() {
    for (column, value, class) in [
        ("status", SqlValue::Text("revoked".into()), "text"),
        ("invalidated_at", SqlValue::Integer(1), "integer"),
        (
            "definition_digest",
            SqlValue::Text("different".into()),
            "text",
        ),
    ] {
        let tool = "strict-valid-inactive";
        let (f, id) = granted_with_deny(tool).await;
        replace_column(&f, &id, column, value, class).await;
        let decision = f.check(tool).await;
        assert_eq!(decision["decision"], "deny", "{column}");
        assert_eq!(decision["source"], "policy", "{column}");
    }
}

#[tokio::test]
async fn active_grant_precedes_malformed_matching_policy() {
    let tool = "strict-grant-first";
    let (f, id) = granted_with_deny(tool).await;
    f.write(
        "UPDATE tool_policy SET updated_at=?1 WHERE actor=?2 AND tool=?3",
        vec![
            SqlValue::Float(1.5),
            SqlValue::Text("agent:requester".into()),
            SqlValue::Text(tool.into()),
        ],
    )
    .await;
    let rows = f.rt.sql().reader().await.unwrap().query_all(SqlStatement {
        sql: "SELECT id, typeof(updated_at) AS storage_class FROM tool_policy WHERE actor=?1 AND tool=?2".into(),
        params: vec![SqlValue::Text("agent:requester".into()), SqlValue::Text(tool.into())],
        label: None,
    }).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text("storage_class").unwrap(), "real");
    let policy_id = rows[0].text("id").unwrap().to_owned();
    let decision = f.check(tool).await;
    assert_eq!(decision["decision"], "allow");
    assert_eq!(decision["source"], "grant");
    assert_eq!(decision["grant_id"], id);

    f.write(
        "UPDATE tool_grants SET status=?1 WHERE id=?2",
        vec![SqlValue::Text("revoked".into()), SqlValue::Text(id.clone())],
    )
    .await;
    let error = f
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": tool, "actor": "agent:requester"}),
        )
        .await
        .expect_err("revoked grant must expose the malformed policy")
        .to_string();
    assert!(
        error.contains(&policy_id) && error.contains("updated_at"),
        "{error}"
    );
}

#[tokio::test]
async fn valid_expiries_and_absent_grants_keep_existing_decisions() {
    let tool = "strict-expiry-controls";
    let (f, id) = granted_with_deny(tool).await;
    replace_column(&f, &id, "expires_at", SqlValue::Null, "null").await;
    let perpetual = f.check(tool).await;
    assert_eq!(perpetual["decision"], "allow");
    assert_eq!(perpetual["source"], "grant");
    assert_eq!(perpetual["grant_id"], id);
    assert!(perpetual["expires_at"].is_null());

    let future = khive_storage::now_micros() + 60_000_000;
    replace_column(&f, &id, "expires_at", SqlValue::Integer(future), "integer").await;
    let active = f.check(tool).await;
    assert_eq!(active["decision"], "allow");
    assert_eq!(active["source"], "grant");
    assert_eq!(active["grant_id"], id);
    assert_eq!(active["expires_at"], khive_runtime::micros_to_iso(future));

    replace_column(&f, &id, "expires_at", SqlValue::Integer(1), "integer").await;
    let expired = f.check(tool).await;
    assert_eq!(expired["decision"], "deny");
    assert_eq!(expired["source"], "policy");
    assert!(expired["grant_id"].is_null());

    f.register("strict-no-grant").await;
    f.call(
        "tool.policy",
        json!({"actor":"agent:requester", "tool":"strict-no-grant", "decision":"deny"}),
    )
    .await;
    let absent = f.check("strict-no-grant").await;
    assert_eq!(absent["decision"], "deny");
    assert_eq!(absent["source"], "policy");
    assert!(absent["grant_id"].is_null());
}

#[tokio::test]
async fn selected_grant_rejects_required_time_instead_of_default_allow() {
    let f = Fixture::new();
    let tool = "strict-required-time";
    let registered = f.register(tool).await;
    let pin = RegistryPin::from_entity(&f.entity(&registered["full_id"]).await).unwrap();
    // Exercise the public decision consumer's read default without changing the registry pin.
    let baseline = khive_pack_tool::policy::decide(
        &f.rt,
        "local",
        "agent:requester",
        tool,
        Some("read"),
        Some(&pin),
    )
    .await
    .unwrap();
    assert_eq!(baseline.decision, "allow");
    assert_eq!(baseline.source, "default");
    let id = f.request(tool).await;
    let grant = f.grant(&id).await;
    assert_eq!(grant["registry_id"], registered["full_id"]);
    assert_eq!(grant["definition_digest"], pin.definition_digest());
    replace_column(
        &f,
        &id,
        "requested_at",
        SqlValue::Text("not-a-time".into()),
        "text",
    )
    .await;
    let error = khive_pack_tool::policy::decide(
        &f.rt,
        "local",
        "agent:requester",
        tool,
        Some("read"),
        Some(&pin),
    )
    .await
    .expect_err("a selected malformed required field must not fall through to read-default allow");
    assert_decode_error(error, Some(&id), "requested_at");
}

#[tokio::test]
async fn selected_grant_rejects_blob_id_instead_of_policy_fallthrough() {
    let tool = "strict-blob-id";
    let (f, id) = granted_with_deny(tool).await;
    replace_column(
        &f,
        &id,
        "id",
        SqlValue::Blob(id.as_bytes().to_vec()),
        "blob",
    )
    .await;
    let error = f
        .registry
        .dispatch(
            "tool.check",
            json!({"tool":tool, "actor":"agent:requester"}),
        )
        .await
        .expect_err("a selected unreadable id must not silently fall through to policy");
    assert_decode_error(error, None, "id");
}
