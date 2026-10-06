//! Event-page refusals through the real daemon frame and MCP request transports.

#![cfg(unix)]

use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{micros_to_iso, KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::Event;
use khive_types::{EventKind, SubstrateKind};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceExt,
};
use serde_json::{json, Value};

const ACTOR: &str = "event-page-transport";
const NAMESPACE: &str = "event-page-transport";
const PRIVATE_PAYLOAD: &str = "oversized-event-private-payload";
const CHILD_CASE: &str = "KHIVE_EVENT_PAGE_TRANSPORT_CHILD";
const TEST_NAME: &str = "budget_refusals_survive_daemon_and_rmcp";
// Match the existing MCP client fixtures. The outer bound covers five calls,
// initialization, teardown, and fixture setup without racing an inner timeout.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const CHILD_TIMEOUT: Duration = CALL_TIMEOUT.saturating_mul(10);
const CALL_COUNT: usize = 5;

#[derive(Clone, Default)]
struct EventPageClient;

impl ClientHandler for EventPageClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

#[test]
fn budget_refusals_survive_daemon_and_rmcp() {
    if std::env::var_os(CHILD_CASE).is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(budget_transport_case());
        return;
    }

    // A short private path also fits Unix socket limits on macOS. Only the
    // child receives HOME and daemon-path overrides; the parent is untouched.
    let dir = tempfile::Builder::new()
        .prefix("kh-ep-")
        .tempdir_in("/tmp")
        .unwrap();
    let output_path = dir.path().join("child-output.txt");
    let output = std::fs::File::create(&output_path).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
        .env_clear()
        // Retain Cargo's executable/library paths and the collector's exact
        // profile path/pattern, but no ambient credentials or khive config.
        .envs(std::env::vars_os().filter(|(key, _)| {
            matches!(
                key.to_str(),
                Some(
                    "PATH" | "DYLD_FALLBACK_LIBRARY_PATH" | "LD_LIBRARY_PATH" | "LLVM_PROFILE_FILE"
                )
            )
        }))
        .env(CHILD_CASE, "1")
        .env("KHIVE_TEST_HARNESS", "1")
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("APPDATA", dir.path().join("config"))
        .env("LOCALAPPDATA", dir.path().join("cache"))
        .env("KHIVE_SOCKET", dir.path().join("s"))
        .env("KHIVE_PID", dir.path().join("p"))
        .env("KHIVE_LOCK", dir.path().join("l"))
        .env("KHIVE_RECOVERER_LOCK", dir.path().join("r"))
        .env("KHIVE_EVENTS_SPLIT", "0")
        .env("KHIVE_DAEMON_STRICT", "1")
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + CHILD_TIMEOUT;
    let completed = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                break false;
            }
        }
    };
    let status = child.wait().unwrap();
    let output = std::fs::read_to_string(output_path).unwrap();
    assert!(
        completed && status.success(),
        "isolated event-page transport test failed:\n{output}"
    );
    assert!(output.contains("EVENT_PAGE_TRANSPORT_VERIFIED"), "{output}");
}

async fn rpc(
    client: &impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    connections: &AtomicUsize,
    args: Value,
    expected_ok: bool,
) -> Value {
    let before = connections.load(Ordering::SeqCst);
    let params = CallToolRequestParams::new("request").with_arguments(
        json!({
            "ops": json!([{"tool": "brain.event_page", "args": args}]).to_string(),
            "presentation": "verbose",
            "format": "json",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let response = tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params))
        .await
        .expect("MCP call deadline")
        .expect("budget refusal is a per-operation receipt, not an RPC error");
    assert_eq!(response.is_error, Some(!expected_ok), "{response:?}");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        before + 1,
        "each MCP call must cross the private daemon connection"
    );
    assert_eq!(response.content.len(), 1);
    let text = &response.content[0].raw.as_text().unwrap().text;
    assert!(
        !text.contains(PRIVATE_PAYLOAD),
        "payload must not leak into a refusal"
    );
    let body: Value = serde_json::from_str(text).expect("MCP JSON text envelope");
    let entries = body["results"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{body}");
    assert_eq!(entries[0]["tool"], "brain.event_page", "{body}");
    assert_eq!(entries[0]["ok"], expected_ok, "{body}");
    entries[0].clone()
}

fn refusal_details(entry: &Value) -> &Value {
    assert_eq!(entry["ok"], false, "{entry}");
    assert!(entry.get("result").is_none(), "no successful partial page");
    assert_eq!(entry["error"]["kind"], "invalid_input", "{entry}");
    &entry["error"]["details"]
}

async fn budget_transport_case() {
    let namespace = Namespace::parse(NAMESPACE).unwrap();
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(std::env::current_dir().unwrap().join("events.db")),
        actor_id: Some(ACTOR.into()),
        default_namespace: namespace.clone(),
        visible_namespaces: vec![namespace.clone()],
        brain_profile: None,
        packs: vec!["kg".into(), "brain".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    assert!(
        runtime.backend().is_file_backed(),
        "MCP must not bypass forwarding"
    );
    let server = KhiveMcpServer::new(runtime.clone()).unwrap();
    let token = runtime.authorize(namespace).unwrap();
    let store = runtime.events(&token).unwrap();
    // Historical fixed bounds exclude every audit generated by these reads.
    // A small row, an unreturnable row, and a small successor distinguish
    // page refusal, row refusal, and cursor progress without a cursor decoder.
    let mut ids = Vec::new();
    for (created_at, bytes) in [(10, 1), (20, 1_100_000), (30, 1)] {
        let mut event = Event::new(
            NAMESPACE,
            "transport.fixture",
            EventKind::Audit,
            SubstrateKind::Note,
            ACTOR,
        )
        .with_payload(if bytes > 1 {
            json!({"marker": PRIVATE_PAYLOAD, "large": "x".repeat(bytes)})
        } else {
            json!({"small": created_at})
        });
        event.created_at = created_at;
        ids.push(event.id.to_string());
        store.append_event(event).await.unwrap();
    }

    let listener = tokio::net::UnixListener::bind(khive_runtime::daemon::socket_path()).unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&connections);
    let daemon_server = server.clone();
    let daemon_task = tokio::spawn(async move {
        for _ in 0..CALL_COUNT {
            let (stream, _) = tokio::time::timeout(CHILD_TIMEOUT, listener.accept())
                .await
                .expect("private daemon accept deadline")
                .unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
            tokio::time::timeout(
                CALL_TIMEOUT,
                khive_runtime::daemon::serve_connection_for_test(stream, daemon_server.clone()),
            )
            .await
            .expect("native daemon frame deadline");
        }
    });
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let mcp_task = tokio::spawn(async move {
        let service = server.serve(server_transport).await.unwrap();
        service.waiting().await.unwrap();
    });
    let client = tokio::time::timeout(CALL_TIMEOUT, EventPageClient.serve(client_transport))
        .await
        .expect("MCP initialization deadline")
        .unwrap();

    let mut args = json!({
        "since": micros_to_iso(0),
        "until": micros_to_iso(100),
        "namespaces": [NAMESPACE],
        "limit": 10,
    });
    let page_refusal = rpc(&client, &connections, args.clone(), false).await;
    assert_eq!(
        refusal_details(&page_refusal),
        &json!({"reason": "page_budget_exceeded"}),
        "multi-row refusal must not offer a skip cursor"
    );

    args["limit"] = json!(1);
    let first = rpc(&client, &connections, args.clone(), true).await;
    assert_eq!(first["ok"], true, "{first}");
    let page = &first["result"];
    assert_eq!(page["count"], 1);
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["events"][0]["id"], ids[0]);
    assert_eq!(page["has_more"], true);
    let previous_after = page["next_after"].as_str().unwrap().to_owned();
    assert!(!previous_after.is_empty());
    args["after"] = json!(previous_after);

    let row_refusal = rpc(&client, &connections, args.clone(), false).await;
    let details = refusal_details(&row_refusal);
    let resume_after = details["resume_after"].as_str().unwrap().to_owned();
    assert!(!resume_after.is_empty() && resume_after.len() <= 512);
    assert_ne!(resume_after, previous_after);
    assert_eq!(
        details,
        &json!({"reason": "row_exceeds_budget", "event_id": ids[1], "resume_after": resume_after}),
        "row refusal must retain exactly its ID and opaque skip cursor"
    );
    let repeated = rpc(&client, &connections, args.clone(), false).await;
    assert_eq!(
        refusal_details(&repeated),
        details,
        "no cursor means no progress"
    );

    args["after"] = json!(resume_after);
    let resumed = rpc(&client, &connections, args, true).await;
    assert_eq!(resumed["ok"], true, "{resumed}");
    let page = &resumed["result"];
    assert_eq!(page["count"], 1);
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        page["events"][0]["id"], ids[2],
        "skip exactly the oversized row"
    );
    assert_eq!(page["has_more"], false);
    assert!(page["next_after"].is_null());
    assert_eq!(page["until"], micros_to_iso(100));

    tokio::time::timeout(CALL_TIMEOUT, client.cancel())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(CALL_TIMEOUT, mcp_task)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(CALL_TIMEOUT, daemon_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(connections.load(Ordering::SeqCst), CALL_COUNT);
    println!("EVENT_PAGE_TRANSPORT_VERIFIED");
}
