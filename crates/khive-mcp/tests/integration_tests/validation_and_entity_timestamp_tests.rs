// ── ue-errors C1: unknown-kwarg rejection ────────────────────────────────────

/// `update(id=<uuid>, nonexistent_field="x")` must return `ok: false`, not
/// silently succeed (ue-errors C1).  The caller must be able to trust that
/// `ok: true` means the intended update was actually applied.
#[tokio::test]
async fn update_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create an entity to update.
    let entity = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="UpdateUnknownKwargTest")"#,
    )
    .await?;
    let id = entity["id"].as_str().unwrap();

    // Attempt update with an unknown kwarg.
    let result = call(
        &client,
        "request",
        json!({ "ops": format!(r#"update(id="{id}", nonexistent_field="x")"#) }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "update with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("nonexistent_field") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `remember(content="x", garbage_arg="y")` must return `ok: false` (ue-errors C1).
#[tokio::test]
async fn remember_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"memory.remember(content="test memory", garbage_arg="xyz")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "remember with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("garbage_arg") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// Known `remember` aliases (`salience`, `decay`, `source`) must still work
/// after deny_unknown_fields is applied (ue-errors C1 regression guard).
#[tokio::test]
async fn remember_aliases_still_accepted() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"memory.remember(content="alias test", salience=0.8, decay=0.005)"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "remember with aliases salience/decay must succeed; got: {first}"
    );
    Ok(())
}

// ── ADR-045 §5 handler invariant: ISO-8601 timestamps at MCP boundary ────────

/// Entity `create` must return ISO-8601 timestamps (not raw microsecond i64s).
///
/// Regression guard: the note create path was missing
/// normalize_entity_timestamps, causing `created_at`/`updated_at` to arrive
/// as integer microseconds. Fixed by wrapping the note create response with
/// normalize_entity_timestamps before remap_note_status.
#[tokio::test]
async fn entity_create_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="TimestampTest-Entity")"#,
    )
    .await?;

    let created_at = result["created_at"].as_str().unwrap_or("");
    let updated_at = result["updated_at"].as_str().unwrap_or("");
    assert!(
        !created_at.is_empty(),
        "entity create created_at must be a string, got: {:?}",
        result["created_at"]
    );
    // ISO-8601 strings start with 4-digit year
    assert!(
        created_at.starts_with("20"),
        "entity create created_at must be ISO-8601, got: {created_at:?}"
    );
    assert!(
        updated_at.starts_with("20"),
        "entity create updated_at must be ISO-8601, got: {updated_at:?}"
    );
    Ok(())
}

/// Note `create` must return ISO-8601 timestamps: the note path was missing
/// normalize_entity_timestamps before the MCP response.
#[tokio::test]
async fn note_create_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = ok_one(
        &client,
        r#"create(kind="note", content="timestamp test note")"#,
    )
    .await?;

    let created_at = result["created_at"].as_str().unwrap_or("");
    let updated_at = result["updated_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "note create created_at must be ISO-8601, got: {created_at:?}"
    );
    assert!(
        updated_at.starts_with("20"),
        "note create updated_at must be ISO-8601, got: {updated_at:?}"
    );
    Ok(())
}

/// Entity `get` (AlwaysVerbose) must return ISO-8601 timestamps.
#[tokio::test]
async fn entity_get_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    let created = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="TimestampGet-Entity")"#,
    )
    .await?;
    let id = created["id"].as_str().unwrap();

    let result = call(
        &client,
        "request",
        json!({"ops": format!(r#"get(id="{id}")"#)}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], true, "get must succeed: {first}");
    let entity = &first["result"];
    let created_at = entity["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "entity get created_at must be ISO-8601, got: {created_at:?}"
    );
    Ok(())
}

/// Entity `list` must return ISO-8601 timestamps across all items.
#[tokio::test]
async fn entity_list_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    // Ensure at least one entity exists.
    ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="TimestampList-Entity")"#,
    )
    .await?;

    let result = ok_one(&client, r#"list(kind="entity", limit=3)"#).await?;
    let items = result["items"]
        .as_array()
        .expect("list(kind=entity) returns an items envelope");
    assert!(!items.is_empty(), "list must return at least one entity");

    for item in items {
        let created_at = item["created_at"].as_str().unwrap_or("");
        assert!(
            created_at.starts_with("20"),
            "entity list created_at must be ISO-8601, got: {created_at:?} in {item}"
        );
    }
    Ok(())
}

/// Entity `update` must return ISO-8601 timestamps (the update response goes
/// through normalize_entity_timestamps before the presentation layer).
#[tokio::test]
async fn entity_update_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    let created = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="TimestampUpdate-Entity")"#,
    )
    .await?;
    let id = created["id"].as_str().unwrap();

    let result = ok_one(
        &client,
        &format!(r#"update(id="{id}", description="updated")"#),
    )
    .await?;

    let updated_at = result["updated_at"].as_str().unwrap_or("");
    assert!(
        updated_at.starts_with("20"),
        "entity update updated_at must be ISO-8601, got: {updated_at:?}"
    );
    Ok(())
}

// ── ue-errors C1 extension: unknown-kwarg rejection on additional verbs ───────

/// `recall(query="x", typo_kwarg="y")` must return `ok: false` (ue-errors C1).
#[tokio::test]
async fn recall_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"memory.recall(query="test", typo_kwarg="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "recall with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("typo_kwarg") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `list(kind="entity", typo_kwarg="y")` must return `ok: false` (ue-errors C1).
#[tokio::test]
async fn list_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"list(kind="entity", typo_kwarg="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "list with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("typo_kwarg") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}
