use khive_mcp::{server::KhiveMcpServer, tools::request::RequestParams};
use khive_runtime::{runtime_config_from_khive_config, KhiveConfig, KhiveRuntime, RuntimeConfig};
use serde_json::Value;

fn runtime(text: &str, packs: Vec<String>) -> KhiveRuntime {
    let config: KhiveConfig = toml::from_str(text).expect("structural config parses");
    config.validate().expect("structural config validates");
    KhiveRuntime::new(runtime_config_from_khive_config(
        &config,
        RuntimeConfig {
            db_path: None,
            packs,
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        },
    ))
    .expect("in-memory runtime")
}

async fn verb_catalog(server: &KhiveMcpServer) -> String {
    let response = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "verbs()".into(),
            presentation: Some("verbose".into()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("verbs request");
    let response: Value = serde_json::from_str(&response).expect("JSON result");
    assert_eq!(response["results"][0]["ok"], true);
    response["results"][0]["result"].to_string()
}

#[tokio::test]
async fn default_boot_and_explicit_telemetry_override_have_distinct_activation_requirements() {
    let default_packs = RuntimeConfig::default().packs;
    assert!(!default_packs.iter().any(|name| name == "telemetry"));
    let missing = runtime("", default_packs.clone());
    let default_server = KhiveMcpServer::new(missing.clone()).expect("default boot needs no table");
    let catalog = verb_catalog(&default_server).await;
    assert!(catalog.contains("stream.read"));
    assert!(!catalog.contains("telemetry.emit"));

    let selected = vec!["kg".into(), "telemetry".into()];
    let error = KhiveMcpServer::with_packs(missing, &selected)
        .err()
        .expect("MCP override activates telemetry and requires its key");
    assert!(
        error.to_string().contains("telemetry.default_carrier"),
        "{error}"
    );

    let declared = runtime(
        "[telemetry]\ndefault_carrier = \"ephemeral\"\n",
        default_packs,
    );
    let enabled = KhiveMcpServer::with_packs(declared, &selected).expect("declared default loads");
    let catalog = verb_catalog(&enabled).await;
    for verb in [
        "telemetry.channels",
        "telemetry.emit",
        "telemetry.read",
        "telemetry.counts",
    ] {
        assert!(catalog.contains(verb), "missing {verb}: {catalog}");
    }
}

#[tokio::test]
async fn excluding_telemetry_overrides_runtime_pack_selection_without_requiring_its_config() {
    let selected = vec!["kg".into(), "telemetry".into()];
    let missing = runtime("[telemetry]\n", selected);
    let excluded = KhiveMcpServer::with_packs(missing.clone(), &["kg".into()])
        .expect("actual selected pack set excludes telemetry");
    let catalog = verb_catalog(&excluded).await;
    assert!(catalog.contains("stream.read"));
    assert!(!catalog.contains("telemetry.emit"));
    let error = KhiveMcpServer::new(missing)
        .err()
        .expect("runtime selection activates telemetry");
    assert!(
        error.to_string().contains("telemetry.default_carrier"),
        "{error}"
    );
}
