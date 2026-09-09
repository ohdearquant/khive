use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{
    mount_config::{MountConfig, MountEffect, MountToolConfig},
    KhiveRuntime, RuntimeConfig,
};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceExt,
};
use serde_json::{json, Value};
use std::fs;

#[derive(Default)]
struct Client;
impl ClientHandler for Client {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

#[tokio::test]
async fn mounted_catalog_and_calls_are_available_on_the_mcp_wire() {
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    let current = std::env::current_exe().unwrap();
    let executable = fs::read_dir(current.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("mcp_mount_fixture-")
                && (path.extension().is_none() || path.extension().is_some_and(|ext| ext == "exe"))
        })
        .max_by_key(|path| path.metadata().unwrap().modified().unwrap())
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::write(&state, json!({"tools": [{"name": "echo", "description": "wire fixture", "inputSchema": {"type": "object"}}]}).to_string()).unwrap();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "agent".into()],
        mounts: vec![MountConfig {
            name: "demo".into(),
            transport: "stdio".into(),
            command: executable.to_string_lossy().into_owned(),
            args: vec![state.to_string_lossy().into_owned()],
            env: vec![],
            credential: None,
            tools: vec![MountToolConfig {
                name: "echo".into(),
                effect: MountEffect::Read,
            }],
            timeout_ms: 3000,
        }],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let server = KhiveMcpServer::new_with_mounts(runtime.clone())
        .await
        .unwrap();
    let (server_io, client_io) = tokio::io::duplex(65536);
    let task = tokio::spawn(async move {
        let server = server.serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    let client = Client.serve(client_io).await.unwrap();
    assert!(client
        .peer_info()
        .unwrap()
        .instructions
        .as_deref()
        .unwrap()
        .contains("demo.echo"));
    async fn plan(
        client: &rmcp::service::RunningService<rmcp::RoleClient, Client>,
        name: &str,
    ) -> Value {
        let result = client
            .call_tool(
                CallToolRequestParams::new("request").with_arguments(
                    json!({"ops": format!("{name}()"), "plan": true})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        serde_json::from_str(&result.content[0].raw.as_text().unwrap().text).unwrap()
    }
    assert_eq!(plan(&client, "demo.echo").await["stages"][0]["known"], true);
    assert_eq!(
        plan(&client, "demo.added").await["stages"][0]["known"],
        false
    );
    fs::write(
        &state,
        json!({"tools": [
            {"name": "echo", "description": "wire fixture", "inputSchema": {"type": "object"}},
            {"name": "added", "inputSchema": {"type": "object"}}
        ]})
        .to_string(),
    )
    .unwrap();
    let mut updated = runtime.config().mounts[0].clone();
    updated.tools.push(MountToolConfig {
        name: "added".into(),
        effect: MountEffect::Read,
    });
    let operator = khive_mounts::MountedPack::start(updated, runtime.clone())
        .await
        .unwrap();
    operator.repin("operator").await.unwrap();
    // External re-pins reach advisory plans on the next ordinary catalog refresh.
    assert_eq!(
        plan(&client, "demo.added").await["stages"][0]["known"],
        false
    );
    client
        .call_tool(
            CallToolRequestParams::new("request").with_arguments(
                json!({"ops": "verbs(pack=\"demo\")"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        plan(&client, "demo.added").await["stages"][0]["known"],
        true
    );
    assert!(!state.with_extension("calls").exists());
    let tools = client.list_all_tools().await.unwrap();
    let description = tools
        .iter()
        .find(|tool| tool.name == "request")
        .unwrap()
        .description
        .as_deref()
        .unwrap();
    assert!(description.contains("demo.echo"));
    assert!(description.contains("agent.spawn"));
    for (ops, success) in [
        ("demo.echo(message=\"hello\")", true),
        ("demo.echo(mode=\"error\")", false),
        ("agent.spawn(provider=\"x\", task=\"t\")", false),
    ] {
        let result = client
            .call_tool(
                CallToolRequestParams::new("request").with_arguments(
                    json!({"ops": ops, "presentation": "verbose", "format": "json"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        let wire = &result.content[0].raw.as_text().unwrap().text;
        let body: Value = serde_json::from_str(wire).unwrap();
        assert_eq!(body["results"][0]["ok"], success, "{body}");
        if success {
            assert_eq!(
                body["results"][0]["result"]["structuredContent"]["message"],
                "hello"
            );
        } else {
            assert!(!wire.contains("RAW_SECRET_SENTINEL"));
            assert!(body["results"][0]["error"].is_object(), "{body}");
        }
    }
    // A plan keeps using owned metadata even when the catalog store is unavailable.
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_script("DROP TABLE tool_source_mounts;".into())
        .await
        .unwrap();
    assert_eq!(
        plan(&client, "demo.added").await["stages"][0]["known"],
        true
    );
    let unavailable = client.list_all_tools().await.unwrap_err();
    let rmcp::service::ServiceError::McpError(error) = unavailable else {
        panic!("expected classified catalog failure, got {unavailable:?}");
    };
    let data = error.data.expect("catalog failure carries classified data");
    assert_eq!(data["domain_disposition"], "not_committed");
    assert_eq!(data["kind"], "unavailable");
    assert_eq!(data["details"]["reason"], "catalog_drift");
    assert!(!data.to_string().contains("no such table"));
    client.cancel().await.unwrap();
    task.await.unwrap();
}
