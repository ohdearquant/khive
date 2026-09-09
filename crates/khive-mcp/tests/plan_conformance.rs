//! Exercise parser conformance failures through the real MCP request tool.

use std::sync::{Mutex, OnceLock};

use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo, ErrorCode},
    service::RunningService,
    ClientHandler, RoleClient, ServiceError, ServiceExt,
};
use serde_json::{json, Value};

#[allow(dead_code)]
#[path = "../../khive-request/tests/parser.rs"]
mod parser_conformance;

#[derive(Clone, Default)]
struct CorpusClient;

impl ClientHandler for CorpusClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

struct CorpusHarness {
    runtime: tokio::runtime::Runtime,
    client: RunningService<RoleClient, CorpusClient>,
}

impl CorpusHarness {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("corpus async runtime");
        let client = runtime.block_on(async {
            let config = RuntimeConfig {
                db_path: None,
                default_namespace: Namespace::parse("test").expect("test namespace"),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::default()
            };
            let storage = KhiveRuntime::new(config).expect("in-memory corpus runtime");
            let server = KhiveMcpServer::new(storage).expect("corpus server");
            let (server_io, client_io) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                let service = server.serve(server_io).await.expect("MCP server handshake");
                let _ = service.waiting().await;
            });
            CorpusClient
                .serve(client_io)
                .await
                .expect("MCP client handshake")
        });
        Self { runtime, client }
    }
}

pub fn plan_for_parser_corpus(input: &str) -> Value {
    // Ordinary requests parse before daemon forwarding. Only failing corpus
    // inputs reach this callback, so neither call needs a production transport.
    assert!(khive_request::parse_request(input).is_err());
    static HARNESS: OnceLock<Mutex<CorpusHarness>> = OnceLock::new();
    let harness = HARNESS
        .get_or_init(|| Mutex::new(CorpusHarness::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    harness.runtime.block_on(async {
        let planned = harness
            .client
            .call_tool(
                CallToolRequestParams::new("request").with_arguments(
                    json!({"ops": input, "plan": true})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("malformed ops must return a successful plan result");
        assert_ne!(planned.is_error, Some(true));
        let text = planned
            .content
            .first()
            .and_then(|content| content.raw.as_text())
            .expect("plan response contains JSON text");
        let plan: Value = serde_json::from_str(&text.text).expect("decoded plan object");
        assert_eq!(plan["parsed"], false);
        assert!(plan.get("stages").is_none());

        let ordinary = harness
            .client
            .call_tool(
                CallToolRequestParams::new("request")
                    .with_arguments(json!({"ops": input}).as_object().unwrap().clone()),
            )
            .await
            .expect_err("ordinary request must reject the same malformed ops");
        let ServiceError::McpError(error) = ordinary else {
            panic!("expected MCP invalid_params, got {ordinary:?}");
        };
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(plan["error"].as_str(), Some(error.message.as_ref()));
        plan
    })
}

#[test]
fn plan_companions_return_mcp_invalid_params() {
    let harness = CorpusHarness::new();
    harness.runtime.block_on(async {
        for (field, value) in [
            ("presentation", json!("verbose")),
            ("presentation_per_op", json!([null])),
            ("format", json!("json")),
            ("format_per_op", json!([null])),
            ("save_to", json!("unused.jsonl")),
            ("request_id", json!(9)),
        ] {
            for value in [value, Value::Null, json!({})] {
                let mut args = json!({"ops":"stats()","plan":true});
                args[field] = value;
                let result = harness
                    .client
                    .call_tool(
                        CallToolRequestParams::new("request")
                            .with_arguments(args.as_object().unwrap().clone()),
                    )
                    .await;
                let Err(ServiceError::McpError(error)) = result else {
                    panic!("expected MCP invalid_params for {field}: {result:?}");
                };
                assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
                assert_eq!(
                    error.data.as_ref().unwrap()["domain_disposition"],
                    "not_committed"
                );
                assert!(error
                    .message
                    .contains(&format!("cannot be combined with {field}")));
            }
        }
    });
}
