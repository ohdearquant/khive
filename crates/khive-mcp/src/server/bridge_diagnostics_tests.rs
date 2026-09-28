use super::*;
use crate::tools::request::RequestParams;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::{json, Value};

fn empty_server() -> KhiveMcpServer {
    let mut server = KhiveMcpServer::from_registry(VerbRegistryBuilder::new().build().unwrap());
    server.stdio_bridge = true;
    server
}

async fn read(server: &KhiveMcpServer, params: RequestParams) -> Value {
    let response = server.request_with_cancellation(params).await.unwrap();
    serde_json::from_str(&response).unwrap()
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn diagnostics_reads_memory_before_executable_check() {
    crate::daemon::reset_fallback_counters();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge");
    std::fs::write(&path, b"original image").unwrap();
    let executable = crate::daemon::executable::BridgeExecutable::at(path.clone()).unwrap();
    let mut server = empty_server();
    server.bridge_executable = Some(Arc::new(std::sync::Mutex::new(executable)));
    std::fs::write(dir.path().join("new"), b"replacement image").unwrap();
    std::fs::rename(dir.path().join("new"), path).unwrap();

    let response = read(
        &server,
        RequestParams {
            ops: "bridge.diagnostics()".into(),
            ..Default::default()
        },
    )
    .await;
    let result = &response["results"][0]["result"];
    assert_eq!(response["status"], "success");
    assert_eq!(
        result["bridge_instance_id"],
        bridge_instance_id().to_string()
    );
    assert_eq!(result["pid"], json!(std::process::id()));
    assert_eq!(result["fallback_total"], 0);
    assert_eq!(result["strict_violations"], 0);
    assert_eq!(result["fallback_reasons"].as_object().unwrap().len(), 5);
    let plan = read(
        &server,
        RequestParams {
            ops: "bridge.diagnostics()".into(),
            plan: Some(true),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(plan["stages"][0]["pack"], "bridge-control");
}

#[tokio::test]
#[serial_test::serial]
async fn mixed_diagnostics_refuses_before_a_write_in_each_execution_shape() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".into()],
        ..RuntimeConfig::default()
    })
    .unwrap();
    let mut server = KhiveMcpServer::new(runtime).unwrap();
    server.stdio_bridge = true;
    let write = r#"create(kind="entity", entity_kind="concept", name="must-not-exist")"#;
    for ops in [
        format!("[{write}, bridge.diagnostics()]"),
        format!("bridge.diagnostics() | {write}"),
        format!("[{write} | bridge.diagnostics(), stats()]"),
        "[bridge.diagnostics()]".to_string(),
    ] {
        let error = server
            .request_with_cancellation(RequestParams {
                ops,
                ..Default::default()
            })
            .await
            .expect_err("bridge control must be single and unbatched");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }
    let stats: Value = serde_json::from_str(
        &server
            .dispatch_request_wire(RequestParams {
                ops: "stats()".into(),
                ..Default::default()
            })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stats["results"][0]["result"]["entities"], 0);
}

#[tokio::test]
async fn help_has_local_schemas_and_shared_identifier_resolution() {
    let server = empty_server();
    let response = read(
        &server,
        RequestParams {
            ops: "bridge.diagnostics(help=true)".into(),
            ..Default::default()
        },
    )
    .await;
    let help = &response["results"][0]["result"];
    assert_eq!(help["verb"], "bridge.diagnostics");
    assert_eq!(help["pack"], "bridge-control");
    assert_eq!(help["input_schema"]["additionalProperties"], false);
    assert_eq!(
        help["result_schema"]["properties"]["bridge_instance_id"]["format"],
        "uuid"
    );
    assert_eq!(
        help["identifier_resolution"],
        khive_runtime::pack::identifier_resolution_help()
    );
}

#[tokio::test]
async fn invalid_arguments_and_save_to_refuse_before_a_sink_is_created() {
    let server = empty_server();
    let dir = tempfile::tempdir().unwrap();
    let sink = dir.path().join("diagnostics.jsonl");
    let error = server
        .request_with_cancellation(RequestParams {
            ops: "bridge.diagnostics()".into(),
            save_to: Some(sink.display().to_string()),
            ..Default::default()
        })
        .await
        .expect_err("save_to is excluded");
    assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(!sink.exists());
    let help_error = server
        .request_with_cancellation(RequestParams {
            ops: "bridge.diagnostics(help=true)".into(),
            save_to: Some(sink.display().to_string()),
            ..Default::default()
        })
        .await
        .expect_err("help cannot create a save sink either");
    assert_eq!(help_error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(!sink.exists());
    for ops in [
        "bridge.diagnostics(help=false)",
        "bridge.diagnostics(extra=1)",
        "bridge.diagnostics(help=true, extra=1)",
    ] {
        let error = server
            .request_with_cancellation(RequestParams {
                ops: ops.into(),
                ..Default::default()
            })
            .await
            .expect_err("only help=true is accepted");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }
}

#[tokio::test]
async fn bridge_plan_is_known_even_in_a_mixed_syntactic_plan() {
    let server = empty_server();
    for ops in [
        "bridge.diagnostics()",
        "bridge.diagnostics(help=true)",
        "[bridge.diagnostics(), stats()]",
    ] {
        let plan = read(
            &server,
            RequestParams {
                ops: ops.into(),
                plan: Some(true),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(plan["parsed"], true);
        assert_eq!(plan["stages"][0]["known"], true);
        assert_eq!(plan["stages"][0]["pack"], "bridge-control");
    }
    let pack_plan: Value = serde_json::from_str(&server.plan_ops("bridge.diagnostics()")).unwrap();
    assert_eq!(pack_plan["stages"][0]["known"], false);
}

#[tokio::test]
async fn all_presentations_and_formats_keep_the_generation_identifier() {
    let server = empty_server();
    let expected = bridge_instance_id().to_string();
    for presentation in [None, Some("agent"), Some("verbose"), Some("human")] {
        for format in ["json", "auto", "table"] {
            let response = read(
                &server,
                RequestParams {
                    ops: "bridge.diagnostics()".into(),
                    presentation: presentation.map(str::to_string),
                    format: Some(format.into()),
                    ..Default::default()
                },
            )
            .await;
            let result = &response["results"][0]["result"];
            if format == "json" {
                assert_eq!(result["bridge_instance_id"], expected);
                assert_eq!(result["fallback_reasons"].as_object().unwrap().len(), 5);
            } else {
                let rendered = result.as_str().unwrap();
                assert!(rendered.contains(&expected), "{format}: {rendered}");
                assert!(rendered.contains("fallback_total"), "{format}: {rendered}");
            }
        }
    }
    let per_op = read(
        &server,
        RequestParams {
            ops: "bridge.diagnostics()".into(),
            presentation: Some("verbose".into()),
            presentation_per_op: Some(vec![Some("agent".into())]),
            format: Some("json".into()),
            format_per_op: Some(vec![Some("table".into())]),
            ..Default::default()
        },
    )
    .await;
    let rendered = per_op["results"][0]["result"].as_str().unwrap();
    assert!(rendered.contains(&expected));
    assert!(rendered.contains("fallback_total"));
}

#[cfg(unix)]
#[tokio::test]
async fn bridge_read_and_help_never_call_the_forward_seam() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static FORWARD_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn spy_forward(
        _frame: khive_runtime::DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        _replay_read_only: bool,
    ) -> ForwardFuture {
        FORWARD_CALLS.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Some(Ok("unexpected forward".into())) })
    }

    let server = empty_server();
    FORWARD_CALLS.store(0, Ordering::SeqCst);
    for ops in ["bridge.diagnostics()", "bridge.diagnostics(help=true)"] {
        let result = server
            .request_with_forward(
                RequestParams {
                    ops: ops.into(),
                    ..Default::default()
                },
                spy_forward,
            )
            .await
            .unwrap();
        let response: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(response["results"][0]["ok"], true);
    }
    assert_eq!(FORWARD_CALLS.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn strict_fallback_count_is_readable_without_weakening_the_next_refusal() {
    struct RestoreStrict(Option<std::ffi::OsString>);
    impl Drop for RestoreStrict {
        fn drop(&mut self) {
            if let Some(value) = &self.0 {
                std::env::set_var("KHIVE_DAEMON_STRICT", value);
            } else {
                std::env::remove_var("KHIVE_DAEMON_STRICT");
            }
        }
    }
    let _restore = RestoreStrict(std::env::var_os("KHIVE_DAEMON_STRICT"));
    std::env::set_var("KHIVE_DAEMON_STRICT", "1");
    crate::daemon::reset_fallback_counters();
    assert!(crate::daemon::test_recordable_fallback_rejected(
        crate::daemon::FallbackReason::NoSocket
    ));
    assert!(crate::daemon::test_recordable_fallback_rejected(
        crate::daemon::FallbackReason::ConfigMismatch
    ));

    let server = empty_server();
    let response = read(
        &server,
        RequestParams {
            ops: "bridge.diagnostics()".into(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        response["results"][0]["result"]["fallback_reasons"]["no_socket"],
        1
    );
    assert_eq!(
        response["results"][0]["result"]["fallback_reasons"]["config_mismatch"],
        1
    );
    assert_eq!(response["results"][0]["result"]["fallback_total"], 2);
    assert_eq!(response["results"][0]["result"]["strict_violations"], 1);
    assert!(crate::daemon::test_recordable_fallback_rejected(
        crate::daemon::FallbackReason::NoSocket
    ));
    assert_eq!(
        crate::daemon::fallback_count(crate::daemon::FallbackReason::NoSocket),
        2
    );
    crate::daemon::reset_fallback_counters();
}

#[tokio::test]
async fn non_stdio_dispatch_cannot_execute_bridge_control() {
    let server = KhiveMcpServer::from_registry(VerbRegistryBuilder::new().build().unwrap());
    let error = server
        .request_with_cancellation(RequestParams {
            ops: "bridge.diagnostics()".into(),
            ..Default::default()
        })
        .await
        .expect_err("only a stdio bridge serves diagnostics");
    assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    let error = server
        .dispatch_request_wire(RequestParams {
            ops: "bridge.diagnostics()".into(),
            ..Default::default()
        })
        .await
        .expect_err("in-process dispatch cannot serve bridge control");
    assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}
