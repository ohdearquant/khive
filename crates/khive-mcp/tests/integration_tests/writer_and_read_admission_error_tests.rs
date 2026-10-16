/// A writer task that exhausts SQLite's busy timeout before entering its
/// request transaction has not invoked the operation and is therefore safe
/// to retry. Keep that proof structured through the MCP boundary.
#[tokio::test]
async fn writer_task_busy_survives_storage_runtime_and_mcp_wire() -> anyhow::Result<()> {
    let client = connect_error_inject().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "writer_task_busy()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];

    assert_eq!(first["ok"], false, "expected op failure: {first}");
    assert_eq!(
        first["error"],
        serde_json::json!({
            "kind": "unavailable",
            "code": "writer_task_begin_busy",
            "stage": "writer_task_begin_busy",
            "domain_disposition": "unknown",
            "message": "storage: writer task could not begin within 175ms because SQLite remained busy; request was not executed",
            "retryable": true,
            "timeout_ms": 175,
            "capability": serde_json::Value::Null,
            "operation": "writer_task_begin",
            "scope": serde_json::Value::Null,
            "retry_after_ms": serde_json::Value::Null,
        }),
        "BEGIN contention must remain distinguishable and safely retryable"
    );

    Ok(())
}

/// Request finality and writer-task liveness are independent. A COMMIT error
/// followed by verified rollback is safe from duplicate effects and keeps the
/// writer alive; a failed rollback is terminal and ambiguous. Preserve both
/// facts as structured MCP fields rather than forcing clients to parse text.
#[tokio::test]
async fn writer_task_finality_survives_storage_runtime_and_mcp_wire() -> anyhow::Result<()> {
    let client = connect_error_inject().await?;

    let rolled_back = call(
        &client,
        "request",
        serde_json::json!({"ops": "writer_task_rolled_back()"}),
    )
    .await?;
    let rolled_back_body: serde_json::Value = serde_json::from_str(&first_text(&rolled_back))?;
    assert_eq!(
        rolled_back_body["results"][0]["error"],
        serde_json::json!({
            "kind": "storage",
            "code": "writer_task_request_failed",
            "stage": "writer_task_request_failed",
            "domain_disposition": "unknown",
            "message": "storage: writer task request failed (request_state=transaction_rolled_back): pool failure during writer_task_commit: commit refused",
            "retryable": true,
            "request_state": "transaction_rolled_back",
            "task_terminated": false,
        })
    );

    let unknown = call(
        &client,
        "request",
        serde_json::json!({"ops": "writer_task_side_effects_unknown()"}),
    )
    .await?;
    let unknown_body: serde_json::Value = serde_json::from_str(&first_text(&unknown))?;
    assert_eq!(
        unknown_body["results"][0]["error"],
        serde_json::json!({
            "kind": "storage",
            "code": "writer_task_terminated",
            "stage": "writer_task_terminated",
            "domain_disposition": "unknown",
            "message": "storage: writer task terminated (request_state=side_effects_unknown)",
            "retryable": false,
            "request_state": "side_effects_unknown",
            "task_terminated": true,
        })
    );

    Ok(())
}

/// A bounded storage-admission wait (reader/writer handle slot or pooled
/// reader checkout) that elapsed acquired nothing and started nothing, so it
/// must cross the MCP boundary as a retryable `unavailable` — never as the
/// caller's invalid input.
#[tokio::test]
async fn storage_admission_timeout_survives_storage_runtime_and_mcp_wire() -> anyhow::Result<()> {
    let client = connect_error_inject().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "storage_admission_timeout()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];

    assert_eq!(first["ok"], false, "expected op failure: {first}");
    assert_eq!(
        first["error"],
        serde_json::json!({
            "kind": "unavailable",
            "code": "storage_admission_timeout",
            "stage": "storage_admission_timeout",
            "domain_disposition": "unknown",
            "message": "storage: admission timeout during sql_bridge.writer_handle after 30000ms",
            "retryable": true,
            "timeout_ms": 30_000,
            "capability": serde_json::Value::Null,
            "operation": "sql_bridge.writer_handle",
            "scope": serde_json::Value::Null,
            "retry_after_ms": serde_json::Value::Null,
        }),
        "an admission deadline that expired before acquisition must stay a retryable resource failure on the wire"
    );

    Ok(())
}

#[tokio::test]
async fn storage_admission_pool_identity_survives_the_mcp_wire() -> anyhow::Result<()> {
    let client = connect_error_inject().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "storage_admission_timeout_with_pool()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"], false,
        "expected pool admission refusal: {first}"
    );
    assert_eq!(
        first["error"],
        serde_json::json!({
            "kind": "unavailable",
            "code": "storage_admission_timeout",
            "stage": "storage_admission_timeout",
            "domain_disposition": "unknown",
            "message": "storage: admission timeout during sql_bridge.reader_operation after 25ms (pool: khive.db)",
            "retryable": true,
            "timeout_ms": 25,
            "capability": serde_json::Value::Null,
            "operation": "sql_bridge.reader_operation",
            "pool_identity": "khive.db",
            "scope": serde_json::Value::Null,
            "retry_after_ms": serde_json::Value::Null,
        }),
        "pool identity must survive the wire without changing operation or retry semantics"
    );
    Ok(())
}

/// #1846 / PR #2229 follow-up: a cached read-only transaction evicted for
/// pinning a WAL snapshot past `read_tx_max_age` must remain a
/// machine-detectable retryable failure once it crosses the normal MCP
/// dispatch boundary — not just at the direct `StorageError::is_retryable()`
/// call site. Without `RuntimeError::retryable_failure_context()` special-
/// casing `StorageError::ReadTransactionAgeEvicted`, this would flatten to
/// `runtime_error_value`'s plain-string fallback and lose `retryable: true`.
#[tokio::test]
async fn read_tx_age_evicted_survives_storage_runtime_and_mcp_wire() -> anyhow::Result<()> {
    let client = connect_error_inject().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "read_tx_age_evicted()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];

    assert_eq!(first["ok"], false, "expected op failure: {first}");
    assert_eq!(
        first["error"],
        serde_json::json!({
            "kind": "unavailable",
            "code": "read_tx_age_evicted",
            "stage": "read_tx_age_evicted",
            "domain_disposition": "unknown",
            "message": "storage: cached read-only transaction exceeded the maximum read-transaction age (120s) during sql_bridge.cached_reader and was rolled back; retry to open a fresh read snapshot",
            "retryable": true,
            "timeout_ms": 120_000,
            "capability": "sql",
            "operation": "sql_bridge.cached_reader",
            "scope": serde_json::Value::Null,
            "retry_after_ms": serde_json::Value::Null,
        }),
        "a read-age eviction must stay a caller-visible retryable unavailable on the wire"
    );

    Ok(())
}

/// PR #2229 round-2 follow-up: a cached read-only transaction's age-eviction
/// rollback can also be denied or fail outright rather than cleanly
/// restoring autocommit. That cleanup-failure outcome must reach the MCP
/// wire as the same retryable `read_tx_age_evicted` stage as a clean
/// eviction — not fall back to `runtime_error_value`'s plain-string
/// classification, which is what happens if the production branch regresses
/// to generic `StorageError::Transaction`.
#[tokio::test]
async fn read_tx_age_eviction_cleanup_failure_survives_storage_runtime_and_mcp_wire(
) -> anyhow::Result<()> {
    let client = connect_error_inject().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "read_tx_age_eviction_cleanup_failed()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];

    assert_eq!(first["ok"], false, "expected op failure: {first}");
    assert_eq!(
        first["error"],
        serde_json::json!({
            "kind": "unavailable",
            "code": "read_tx_age_evicted",
            "stage": "read_tx_age_evicted",
            "domain_disposition": "unknown",
            "message": "storage: cached read-only transaction exceeded the maximum read-transaction age (120s) during sql_bridge.cached_reader but could not be cleanly rolled back (rollback failed: disk I/O error); the connection was discarded, retry to open a fresh read snapshot",
            "retryable": true,
            "timeout_ms": 120_000,
            "capability": "sql",
            "operation": "sql_bridge.cached_reader",
            "scope": serde_json::Value::Null,
            "retry_after_ms": serde_json::Value::Null,
        }),
        "a denied/failed cleanup rollback must stay a caller-visible retryable unavailable on \
         the wire, distinguishable in the message from a clean eviction"
    );

    Ok(())
}
