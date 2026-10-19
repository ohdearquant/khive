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
