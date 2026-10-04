use super::*;
use crate::tools::request::RequestParams;
use khive_runtime::{Namespace, RuntimeConfig};

fn server() -> KhiveMcpServer {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".into(), "gtd".into()],
        ..RuntimeConfig::default()
    })
    .unwrap();
    KhiveMcpServer::new(rt).unwrap()
}

async fn dispatch(server: &KhiveMcpServer, ops: &str, mode: Option<&str>) -> Value {
    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: ops.into(),
            presentation: mode.map(str::to_owned),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".into()),
            format_per_op: None,
            request_id: None,
        })
        .await
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

async fn oracle(server: &KhiveMcpServer, receipt: &Value, agent: bool) -> Value {
    let body = dispatch(
        server,
        &format!("get(id={})", json!(receipt["id"])),
        Some("verbose"),
    )
    .await;
    assert_eq!(body["results"][0]["ok"], true);
    let stored = body["results"][0]["result"].clone();
    let full = stored["id"].as_str().unwrap();
    uuid::Uuid::parse_str(full).unwrap();
    assert_eq!(
        receipt["id"].as_str().unwrap(),
        if agent { &full[..8] } else { full }
    );
    let timestamp = stored["created_at"].as_str().unwrap();
    chrono::DateTime::parse_from_rfc3339(timestamp).unwrap();
    assert_eq!(
        timestamp
            .split('.')
            .nth(1)
            .unwrap()
            .trim_end_matches('Z')
            .len(),
        6
    );
    stored
}

#[test]
fn issue_2537_registered_policy_ignores_result_markers() {
    let server = server();
    let timestamp = "2026-01-01T00:00:00.123456Z";
    let payload = json!({
        "tool": "stream.batch", "policy": "StreamBatchReceipts",
        "results": [{"id": "aabbccdd-1234-4321-1234-abcdefabcdef", "updated_at": timestamp}],
    });
    for tool in ["list", "stream.batch"] {
        let policy = server.registry.presentation_policy_for(tool);
        let parallel = present_ok_envelope_or_depth_error(
            tool.into(),
            OpSuccess::complete(payload.clone()),
            PresentationMode::Agent,
            1_767_225_780,
            policy,
            NoteContentScope::None,
        );
        let canonical = ok_envelope(tool.into(), OpSuccess::complete(payload.clone()));
        let chained = apply_presentation_to_result(
            canonical,
            PresentationMode::Agent,
            1_767_225_780,
            policy,
            NoteContentScope::None,
        );
        for result in [parallel, chained] {
            assert_eq!(result["result"]["results"][0]["updated_at"], timestamp);
            assert!(result["result"]["results"][0]
                .get("updated_at_relative")
                .is_none());
            assert_eq!(result["result"]["results"][0]["id"], "aabbccdd");
        }
    }
}

#[tokio::test]
async fn issue_2537_local_cli_defaulted_vs_raw_modes() {
    let server = server();
    let mut pairs = Vec::new();
    for (index, mode) in [None, Some("agent"), Some("verbose"), Some("human")]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            parse_presentation_mode(mode).unwrap(),
            match mode {
                Some("verbose") => PresentationMode::Verbose,
                Some("human") => PresentationMode::Human,
                _ => PresentationMode::Agent,
            }
        );
        let body = dispatch(
            &server,
            &format!(
                "stream.append(stream=\"local-{index}\",record={{\"i\":{index}}},embed=false)"
            ),
            mode,
        )
        .await;
        assert_eq!(body["results"][0]["ok"], true);
        assert_eq!(body["results"][0]["tool"], "stream.append");
        let receipt = body["results"][0]["result"].clone();
        assert_eq!(receipt["seq"], 1);
        let stored = oracle(&server, &receipt, mode.is_none() || mode == Some("agent")).await;
        if mode == Some("verbose") || mode == Some("human") {
            assert_eq!(receipt["created_at"], stored["created_at"]);
        }
        pairs.push((
            mode,
            receipt["created_at"].clone(),
            stored["created_at"].clone(),
        ));
    }
    assert_eq!(pairs.len(), 4);
    println!("SR2537 local controls complete: raw None Agent and CLI-defaulted Some(verbose)");
    for (mode, actual, expected) in pairs {
        assert_eq!(actual, expected, "SR2537 local precision {mode:?}");
    }
}

#[tokio::test]
async fn issue_2537_receipt_depth_frame_errors() {
    let server = server();
    let whole = json!({"tool":"stream.batch","ok":false,"error":{"details":{"updated_at":"2026-01-01T00:00:00.123456Z"}}});
    assert_eq!(
        apply_presentation_to_result(
            whole.clone(),
            PresentationMode::Agent,
            0,
            VerbPresentationPolicy::StreamBatchReceipts,
            NoteContentScope::None,
        ),
        whole
    );
    let nested = json!({"tool":"stream.batch","ok":true,"result":{"results":[{"error":{"details":{"updated_at":"2026-01-01T00:00:00.123456Z"}}}]}});
    assert_eq!(
        apply_presentation_to_result(
            nested,
            PresentationMode::Agent,
            0,
            VerbPresentationPolicy::StreamBatchReceipts,
            NoteContentScope::None,
        )["result"]["results"][0]["error"]["details"]["updated_at"],
        "2026-01-01T00:00:00.123456Z"
    );
    fn nest(n: usize) -> Value {
        let mut v = json!(1);
        for _ in 0..n {
            v = Value::Array(vec![v]);
        }
        v
    }
    let limit = khive_request::NESTING_DEPTH_LIMIT;
    let at =
        chain_ok_envelope_or_depth_error("stream.batch".into(), OpSuccess::complete(nest(limit)))
            .unwrap();
    assert_eq!(at["ok"], true);
    let chain = chain_ok_envelope_or_depth_error(
        "stream.batch".into(),
        OpSuccess::complete(nest(limit + 1)),
    )
    .unwrap_err();
    assert_eq!(chain.error["kind"], "result_too_deep");
    let parallel = present_ok_envelope_or_depth_error(
        "stream.append".into(),
        OpSuccess::complete(nest(limit + 1)),
        PresentationMode::Agent,
        0,
        VerbPresentationPolicy::StreamAppendReceipt,
        NoteContentScope::None,
    );
    assert_eq!(parallel["ok"], false);
    assert_eq!(parallel["error"]["kind"], "result_too_deep");
    assert!(parallel.get("result").is_none());
    let help = dispatch(&server, "stream.append(help=true)", None).await;
    assert_eq!(help["results"][0]["ok"], true);
    assert!(help["results"][0]["result"].get("created_at").is_none());
    let body = dispatch(
        &server,
        r#"stream.append(stream="frame",record={"ok":true},embed=false)"#,
        None,
    )
    .await;
    let entry = &body["results"][0];
    assert_eq!(entry["ok"], true);
    assert_eq!(entry["result"]["seq"], 1);
    let stored = oracle(&server, &entry["result"], true).await;
    let omitted = frame_budget_omission(entry, &server.registry);
    assert_eq!(omitted["ok"], false);
    assert_eq!(omitted["executed"], true);
    assert_eq!(omitted["error"]["kind"], "response_frame_budget_exceeded");
    assert!(omitted.get("result").is_none());
    println!(
        "SR2537 boundary controls complete: error/help/depth/frame; real retained receipt last"
    );
    assert_eq!(
        entry["result"]["created_at"], stored["created_at"],
        "SR2537 retained result precision"
    );
}
