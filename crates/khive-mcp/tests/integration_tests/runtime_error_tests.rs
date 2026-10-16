// ── Structured runtime-error preservation through the MCP boundary ──────────

/// A minimal mock pack whose verbs return structured errors. Used to verify
/// that the MCP per-op serializer does not flatten typed runtime/storage
/// errors at the wire boundary.
struct ErrorInjectPack;

impl khive_types::Pack for ErrorInjectPack {
    const NAME: &'static str = "error-inject";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: "always_fail",
            description: "always returns a KhiveError::unavailable with code + details",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "writer_pool_timeout",
            description: "returns a typed writer pool timeout through a storage wrapper",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "write_queue_full",
            description: "returns a typed write-queue saturation error",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "writer_task_busy",
            description: "returns a typed writer-task BEGIN contention error",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "writer_task_rolled_back",
            description: "returns an ordinary writer request error after proven rollback",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "writer_task_side_effects_unknown",
            description: "returns a terminal writer error with ambiguous side effects",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "storage_admission_timeout",
            description: "returns a typed storage-admission timeout error",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "storage_admission_timeout_with_pool",
            description: "returns a storage-admission timeout naming its pool",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "read_tx_age_evicted",
            description: "returns a typed cached-reader read-transaction age eviction error",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "read_tx_age_eviction_cleanup_failed",
            description: "returns a typed cached-reader read-transaction age eviction error \
                whose cleanup rollback was denied or failed",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait]
impl PackRuntime for ErrorInjectPack {
    fn name(&self) -> &str {
        "error-inject"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        ErrorInjectPack::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: serde_json::Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<serde_json::Value, RuntimeError> {
        if verb == "writer_pool_timeout" {
            let storage = khive_storage::StorageError::driver(
                khive_storage::StorageCapability::Notes,
                "append_note",
                khive_db::SqliteError::WriterPoolCheckoutTimeout {
                    timeout: std::time::Duration::from_millis(175),
                },
            );
            return Err(RuntimeError::Storage(storage));
        }
        if verb == "write_queue_full" {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::WriteQueueFull { timeout_ms: 175 },
            ));
        }
        if verb == "writer_task_busy" {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::WriterTaskBusy { timeout_ms: 175 },
            ));
        }
        if verb == "writer_task_rolled_back" {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::WriterTaskRequestFailed {
                    request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                    source: Box::new(khive_storage::StorageError::Pool {
                        operation: "writer_task_commit".into(),
                        message: "commit refused".into(),
                    }),
                },
            ));
        }
        if verb == "writer_task_side_effects_unknown" {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::writer_task_terminated(
                    khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                ),
            ));
        }
        if verb == "storage_admission_timeout" {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::AdmissionTimeout {
                    operation: "sql_bridge.writer_handle".into(),
                    timeout_ms: 30_000,
                    pool_identity: None,
                },
            ));
        }
        if verb == "storage_admission_timeout_with_pool" {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::AdmissionTimeout {
                    operation: "sql_bridge.reader_operation".into(),
                    timeout_ms: 25,
                    pool_identity: Some("khive.db".to_string()),
                },
            ));
        }
        if verb == "read_tx_age_evicted" {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::ReadTransactionAgeEvicted {
                    operation: "sql_bridge.cached_reader".into(),
                    max_age_secs: 120,
                },
            ));
        }
        if verb == "read_tx_age_eviction_cleanup_failed" {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::ReadTransactionAgeEvictionCleanupFailed {
                    operation: "sql_bridge.cached_reader".into(),
                    max_age_secs: 120,
                    message: "rollback failed: disk I/O error".into(),
                },
            ));
        }
        let err = KhiveError::unavailable("downstream service offline")
            .with_code(KhiveErrorCode::new(ErrorDomain::Runtime, 10))
            .with_details(Details::new([
                ("service", "embed"),
                ("region", "us-east-1"),
            ]));
        Err(RuntimeError::Khive(err))
    }
}

/// Build a server backed only by the `ErrorInjectPack` (no DB, no embedding).
fn make_error_inject_server() -> KhiveMcpServer {
    disable_daemon();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(ErrorInjectPack);
    let registry = builder.build().expect("error-inject registry builds");
    KhiveMcpServer::from_registry(registry)
}

async fn connect_error_inject(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_error_inject_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `RuntimeError::Khive` must survive the MCP per-op boundary as a structured
/// JSON object — not collapsed to a flat string via `Display`.
///
/// Verifies:
/// - `error` is a JSON object (not a string)
/// - `error.kind` is present (snake_case string)
/// - `error.message` is present
/// - `error.code` is present as a wire string (e.g. "runtime:10")
/// - `error.details` is a non-null JSON object
/// - Ordinary runtime errors retain their display text in `error.message`
///   (covered by `unknown_verb_returns_per_op_failure_not_invalid_params`)
#[tokio::test]
async fn runtime_khive_error_serializes_as_structured_object() -> anyhow::Result<()> {
    let client = connect_error_inject().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "always_fail()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];

    // The op failed.
    assert_eq!(first["ok"], false, "expected op failure: {first}");

    // `error` must be an object, not a string.
    let error = &first["error"];
    assert!(
        error.is_object(),
        "error must be a JSON object (not a string); got: {error}"
    );

    // Required fields must be present.
    assert!(
        error["kind"].is_string(),
        "error.kind must be a string; got: {error}"
    );
    assert!(
        error["message"].is_string(),
        "error.message must be a string; got: {error}"
    );
    assert!(
        error["code"].is_string(),
        "error.code must be a wire string (e.g. 'runtime:10'); got: {error}"
    );
    assert!(
        error["details"].is_object(),
        "error.details must be a JSON object; got: {error}"
    );

    // Spot-check values.
    assert_eq!(
        error["kind"].as_str().unwrap(),
        "unavailable",
        "KhiveError::unavailable should map to kind='unavailable'"
    );
    assert_eq!(
        error["code"].as_str().unwrap(),
        "runtime:10",
        "ErrorCode(Runtime, 10) should serialize as 'runtime:10'"
    );
    assert_eq!(
        error["details"]["service"].as_str().unwrap(),
        "embed",
        "details key 'service' should be preserved"
    );

    Ok(())
}

/// A typed SQLite checkout timeout must remain structurally classifiable after
/// `StorageError::Driver` and `RuntimeError::Storage` wrap it, then cross the
/// real MCP request transport. Compatibility display text is retained in the
/// `message` field while callers use `code`/`stage` instead of matching it.
#[tokio::test]
async fn writer_pool_timeout_survives_storage_runtime_and_mcp_wire() -> anyhow::Result<()> {
    let client = connect_error_inject().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "writer_pool_timeout()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];

    assert_eq!(first["ok"], false, "expected op failure: {first}");
    assert_eq!(
        first["error"],
        serde_json::json!({
            "kind": "unavailable",
            "code": "writer_pool_checkout_timeout",
            "stage": "writer_pool_checkout_timeout",
            "domain_disposition": "unknown",
            "message": "storage: backend driver error in Notes during append_note: invalid data: timed out after 175ms waiting for sqlite writer connection",
            "retryable": true,
            "timeout_ms": 175,
            "capability": "notes",
            "operation": "append_note",
            "scope": serde_json::Value::Null,
            "retry_after_ms": serde_json::Value::Null,
        }),
        "the exact wire contract must preserve stage, deadline, retryability, and storage context"
    );

    Ok(())
}

/// A bounded write-queue enqueue timeout (#1382's `send_bounded` /
/// `send_top_level_bounded`) must remain structurally classifiable and marked
/// retryable after `RuntimeError::Storage` wraps it, then crosses the real
/// MCP request transport (#1643). The request was never accepted onto the
/// queue, so retrying is safe.
#[tokio::test]
async fn write_queue_full_survives_storage_runtime_and_mcp_wire() -> anyhow::Result<()> {
    let client = connect_error_inject().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "write_queue_full()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];

    assert_eq!(first["ok"], false, "expected op failure: {first}");
    assert_eq!(
        first["error"],
        serde_json::json!({
            "kind": "unavailable",
            "code": "writer_queue_saturated",
            "stage": "writer_queue_saturated",
            "domain_disposition": "unknown",
            "message": "storage: write queue full: timed out after 175ms waiting for writer task capacity",
            "retryable": true,
            "timeout_ms": 175,
            "capability": serde_json::Value::Null,
            "operation": serde_json::Value::Null,
            "scope": "writer_admission",
            "retry_after_ms": 175,
        }),
        "the exact wire contract must preserve stage, deadline, retryability, and ADR-131:251's scope/retry_after_ms for queue saturation"
    );

    Ok(())
}
