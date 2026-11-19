//! Integration tests for the request-only khive-mcp surface (ADR-016 + ADR-025).
//!
//! Validates the single-tool composition: every verb is reached via `request(ops="…")`.
//!
// FILE SIZE JUSTIFICATION: All test groups share the same in-process server
// helpers (make_server, connect, ok_one, DummyClient, first_text, call).
// Splitting into multiple files would require a `tests/common/` re-export
// module and would scatter the shared setup across files without reducing
// cognitive overhead. The single file makes it easy to verify that every
// section exercises the same server construction path and that helper changes
// propagate to all coverage areas simultaneously.

// Force the knowledge pack to be linked (inventory::submit! requires the crate
// to be linked into the test binary for its PackRegistration to self-register).
#[path = "support/integration_full_server.rs"]
mod full_server;
#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;
use full_server::make_full_server;

use khive_pack_knowledge as _;

use async_trait::async_trait;
use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{
    runtime_config_from_khive_config, GitWriteEntryConfig, GitWriteSectionConfig, KhiveConfig,
    KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::Event;
use khive_types::{
    Details, ErrorCode as KhiveErrorCode, ErrorDomain, EventKind, HandlerDef, KhiveError, Pack,
    SubstrateKind, VerbCategory, Visibility,
};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, ClientInfo, ErrorCode},
    ClientHandler, ServerHandler, ServiceError, ServiceExt,
};
use serde_json::{json, Value};

fn disable_daemon() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var("KHIVE_NO_DAEMON", "1"));
}

fn make_server() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "gtd".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+gtd")
}

#[derive(Clone, Default)]
struct DummyClient;

impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

async fn connect(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_server();
    tokio::spawn(async move {
        if let Ok(server_service) = server.serve(server_transport).await {
            let _ = server_service.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// The `request` tool's description as an MCP client receives it from
/// `tools/list`, served from `server`. The verb catalog lives there (ADR-016);
/// the `initialize` instructions point to it instead of repeating it.
async fn request_tool_description(server: KhiveMcpServer) -> anyhow::Result<String> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(server_service) = server.serve(server_transport).await {
            let _ = server_service.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    let listed = client.list_tools(None).await?;
    let request = listed
        .tools
        .iter()
        .find(|t| t.name == "request")
        .expect("request tool must be present");
    Ok(request.description.as_deref().unwrap_or("").to_string())
}

fn first_text(r: &CallToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

async fn call(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    name: impl Into<String>,
    args: Value,
) -> anyhow::Result<CallToolResult> {
    let params = CallToolRequestParams::new(name.into())
        .with_arguments(args.as_object().expect("args must be JSON object").clone());
    Ok(client.call_tool(params).await?)
}

/// Helper: run a single op via `request` and return the parsed `result` field
/// of the first entry. Uses `presentation: "verbose"` so tests receive full
/// canonical UUIDs and timestamps (not Agent-mode short forms). Panics if the
/// op failed.
async fn ok_one(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    ops: &str,
) -> anyhow::Result<Value> {
    let result = call(
        client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = body["results"].get(0).cloned().unwrap_or(Value::Null);
    assert_eq!(
        first["ok"],
        json!(true),
        "expected op to succeed, got: {first}"
    );
    Ok(first["result"].clone())
}

/// Run one op with the MCP default Agent presentation and return its result.
async fn agent_one(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    ops: &str,
) -> anyhow::Result<Value> {
    let result = call(client, "request", json!({"ops": ops})).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = body["results"].get(0).cloned().unwrap_or(Value::Null);
    assert_eq!(
        first["ok"],
        json!(true),
        "expected Agent-mode op to succeed, got: {first}"
    );
    Ok(first["result"].clone())
}

include!("integration_tests/surface_and_read_only_tests.rs");

include!("integration_tests/kg_batch_and_usage_tests.rs");

include!("integration_tests/pack_identifier_tests.rs");

include!("integration_tests/proposal_and_lifecycle_identifier_tests.rs");

include!("integration_tests/pack_and_json_dispatch_tests.rs");

include!("integration_tests/kind_hook_tests.rs");

include!("integration_tests/kind_filter_tests.rs");

include!("integration_tests/runtime_error_tests.rs");

include!("integration_tests/writer_and_read_admission_error_tests.rs");

include!("integration_tests/ambiguous_delivery_error_tests.rs");

include!("integration_tests/engine_configuration_tests.rs");

include!("integration_tests/chain_previous_value_tests.rs");

include!("integration_tests/chain_failure_tests.rs");

include!("integration_tests/help_schema_tests.rs");

include!("integration_tests/comm_and_schedule_help_tests.rs");

include!("integration_tests/startup_and_visibility_tests.rs");

include!("integration_tests/identifier_and_link_conflict_tests.rs");

include!("integration_tests/proposal_read_tests.rs");

include!("integration_tests/actor_and_namespace_tests.rs");

include!("integration_tests/validation_and_entity_timestamp_tests.rs");

include!("integration_tests/pack_timestamp_tests.rs");

include!("integration_tests/cross_pack_contract_tests.rs");

include!("integration_tests/exec_rendering_tests.rs");

include!("integration_tests/schedule_timestamp_tests.rs");

include!("integration_tests/proposal_chain_tests.rs");

include!("integration_tests/config_identity_and_routing_tests.rs");

include!("integration_tests/format_and_presentation_tests.rs");

#[path = "streams/contract.rs"]
mod stream_contract;

include!("integration_tests/receipt_composition_tests.rs");

include!("integration_tests/receipt_format_and_failure_tests.rs");

include!("integration_tests/parsed_content_tests.rs");

include!("integration_tests/indexed_conflict_tests.rs");
