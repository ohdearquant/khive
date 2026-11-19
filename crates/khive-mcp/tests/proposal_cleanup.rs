//! Exercise the actual MCP request transport; isolate process-wide daemon/env state.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use khive_mcp::server::KhiveMcpServer;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceExt,
};
use serde_json::{json, Value};

const CHILD: &str = "KHIVE_PROPOSAL_CLEANUP_MCP_CHILD";
const TEST: &str = "proposal_cleanup_mcp_visibility_and_help";

#[derive(Clone, Default)]
struct Client;
impl ClientHandler for Client {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

#[test]
fn proposal_cleanup_mcp_visibility_and_help() {
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(wire_case());
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let output_path = home.path().join("child-output");
    let output = std::fs::File::create(&output_path).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .env_clear()
        .envs(std::env::vars_os().filter(|(key, _)| {
            matches!(
                key.to_str(),
                Some(
                    "PATH" | "DYLD_FALLBACK_LIBRARY_PATH" | "LD_LIBRARY_PATH" | "LLVM_PROFILE_FILE"
                )
            )
        }))
        .env(CHILD, "1")
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_TEST_HARNESS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("KHIVE_VOLUME_LOCK_DIR", home.path().join("locks"))
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "{}",
                std::fs::read_to_string(output_path).unwrap()
            );
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "MCP child exceeded deadline: {}",
                std::fs::read_to_string(output_path).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn wire_case() {
    let database = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(database.path().join("proposals.db")),
        packs: vec!["kg".into()],
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("local".into()));
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    assert!(!registry
        .dispatch("verbs", json!({}))
        .await
        .unwrap()
        .to_string()
        .contains("kg.proposal_cleanup"));
    let created = registry
        .dispatch(
            "propose",
            json!({
                "title":"Wire visibility", "description":"Operator-only archive", "changeset":{
                    "kind":"add_entity","entity":{"kind":"concept","name":"Wire visibility"}}
            }),
        )
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();
    registry
        .dispatch("review", json!({"id":id,"decision":"reject"}))
        .await
        .unwrap();
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "UPDATE proposals_open SET updated_at=1 WHERE proposal_id=?1",
            vec![SqlValue::Text(id.into())],
        ))
        .await
        .unwrap();
    let server = KhiveMcpServer::new(runtime.clone()).unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let server_task = tokio::spawn(async move {
        let service = server.serve(server_transport).await.unwrap();
        service.waiting().await.unwrap();
    });
    let client = Client.serve(client_transport).await.unwrap();
    let tools = client.list_tools(None).await.unwrap().tools;
    assert!(!tools.iter().any(|tool| tool.name == "kg.proposal_cleanup"));
    assert!(!tools.iter().any(|tool| tool
        .description
        .as_ref()
        .is_some_and(|description| description.contains("kg.proposal_cleanup"))));
    let cases = [
        (r#"kg.proposal_cleanup(older_than="1d")"#.to_owned(), false),
        (json!([{"tool":"kg.proposal_cleanup","args":{"older_than":"1d"}}, {"tool":"kg.proposal_cleanup","args":{"older_than":"1d"}}]).to_string(), false),
        (r#"whoami() | kg.proposal_cleanup(older_than="1d")"#.to_owned(), false),
        (r#"kg.proposal_cleanup(help=true)"#.to_owned(), true),
    ];
    for (ops, help) in cases {
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            client.call_tool(
                CallToolRequestParams::new("request").with_arguments(
                    json!({"ops":ops,"format":"json","presentation":"verbose"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let body: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        let entries = body["results"].as_array().unwrap();
        let cleanup = entries
            .iter()
            .filter(|entry| entry["tool"] == "kg.proposal_cleanup")
            .collect::<Vec<_>>();
        assert!(!cleanup.is_empty(), "{body}");
        for entry in cleanup {
            assert_eq!(entry["ok"], help, "{entry}");
            if help {
                assert_eq!(entry["result"]["visibility"], "internal");
                assert_eq!(entry["result"]["callable_via_mcp"], false);
            } else {
                assert!(
                    entry["error"].to_string().contains("internal subhandler"),
                    "{entry}"
                );
            }
        }
        let marker = runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_row(SqlStatement::new(
                "SELECT archived_at FROM proposals_open WHERE proposal_id=?1",
                vec![SqlValue::Text(id.into())],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            marker.opt_i64("archived_at").unwrap(),
            None,
            "wire/help must not execute cleanup"
        );
    }
    // A real eligible witness ensures refusals were not vacuous zero-row calls.
    assert_eq!(
        registry
            .dispatch("kg.proposal_cleanup", json!({"older_than":"1d"}))
            .await
            .unwrap(),
        json!({"archived":1})
    );
    client.cancel().await.unwrap();
    server_task.abort();
    let _ = server_task.await;
}
