use std::collections::BTreeSet;
use std::ops::Deref;
use std::time::Duration;

use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceExt,
};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct AgendaClient;

impl ClientHandler for AgendaClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

async fn connect() -> anyhow::Result<impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    static NO_DAEMON: std::sync::Once = std::sync::Once::new();
    NO_DAEMON.call_once(|| std::env::set_var("KHIVE_NO_DAEMON", "1"));
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        actor_id: Some("test:mcp-agenda-cursor".to_owned()),
        visible_namespaces: vec![],
        packs: vec!["kg".to_owned(), "comm".to_owned(), "schedule".to_owned()],
        ..RuntimeConfig::no_embeddings()
    })?;
    assert!(!runtime.backend().is_file_backed());
    let server = KhiveMcpServer::new(runtime)?;
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(service) = server.serve(server_transport).await {
            let _ = service.waiting().await;
        }
    });
    Ok(tokio::time::timeout(
        Duration::from_secs(15),
        AgendaClient.serve(client_transport),
    )
    .await??)
}

async fn request(
    client: &impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    tool: &str,
    args: Value,
) -> anyhow::Result<Value> {
    let params = CallToolRequestParams::new("request").with_arguments(
        json!({
            "ops": json!([{"tool": tool, "args": args}]).to_string(),
            "format": "json",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let response =
        tokio::time::timeout(Duration::from_secs(15), client.call_tool(params)).await??;
    let text = response
        .content
        .first()
        .and_then(|content| content.raw.as_text())
        .expect("JSON text envelope from the real MCP request tool");
    let body: Value = serde_json::from_str(&text.text)?;
    let receipts = body["results"].as_array().expect("per-operation receipts");
    assert_eq!(receipts.len(), 1, "{body}");
    assert_eq!(receipts[0]["tool"], tool, "{body}");
    assert_eq!(receipts[0]["ok"], true, "{body}");
    Ok(receipts[0]["result"].clone())
}

fn assert_empty(page: &Value) {
    assert_eq!(page.get("events"), Some(&json!([])), "{page}");
    assert_eq!(page.get("count"), Some(&json!(0)), "{page}");
    assert_eq!(page.get("next"), Some(&Value::Null), "{page}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn default_agent_agenda_cursor_round_trips_over_rmcp() -> anyhow::Result<()> {
    let client = connect().await?;
    assert_empty(&request(&client, "schedule.agenda", json!({"limit": 1})).await?);
    let mut expected = Vec::new();
    for at in [
        "2099-01-01T12:00:00.123456+02:00",
        "2099-01-01T05:00:00.123456-05:00",
        "2099-01-01T10:00:00.123456Z",
    ] {
        let reminder = request(
            &client,
            "schedule.remind",
            json!({"content": "MCP agenda continuation sentinel", "at": at}),
        )
        .await?;
        let id = reminder["full_id"].as_str().expect("full reminder UUID");
        assert_eq!(uuid::Uuid::parse_str(id)?.to_string(), id);
        assert_eq!(reminder["trigger_at"], at);
        expected.push((at.to_owned(), id.to_owned()));
    }
    expected.sort();
    let mut args = json!({"limit": 1});
    let mut seen = BTreeSet::new();
    for position in 0..=expected.len() {
        let page = request(&client, "schedule.agenda", args.clone()).await?;
        if position == expected.len() {
            assert_empty(&page);
            break;
        }
        let events = page["events"].as_array().expect("agenda event rows");
        assert_eq!(page["count"], 1);
        assert_eq!(events.len(), 1);
        let (at, id) = &expected[position];
        assert_eq!(events[0]["full_id"], *id, "ordered event UUID");
        assert_eq!(events[0]["properties"]["trigger_at"], *at);
        let short_id = uuid::Uuid::parse_str(id)?.simple().to_string();
        assert_eq!(
            events[0]["id"],
            &short_id[..8],
            "ordinary Agent event metadata remains shortened"
        );
        assert!(
            events[0]
                .get("created_at_relative")
                .is_some_and(Value::is_string),
            "ordinary Agent event metadata retains relative timestamps"
        );
        assert!(seen.insert(id.clone()), "continuation must make progress");
        let next = page["next"].as_object().expect("nonempty page cursor");
        assert_eq!(
            next.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from(["after", "after_id"])
        );
        assert_eq!(next["after"], *at, "original timestamp spelling");
        assert_eq!(next["after_id"], *id, "canonical full cursor UUID");
        args["after"] = next["after"].clone();
        args["after_id"] = next["after_id"].clone();
    }
    assert_eq!(seen.len(), expected.len(), "every reminder appears once");
    Ok(())
}
