//! Real server-entry regressions for the debug-only batch task size guards.

use crate::server::KhiveMcpServer;
use crate::tools::request::RequestParams;
use khive_request::{parse_request, ExecutionMode};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use serde_json::Value;

const SERVER_ENTRY_STACK_BYTES: usize = 64 * 1024 * 1024;

async fn make_server() -> KhiveMcpServer {
    let cfg = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        actor_id: Some("actor:component-test".to_string()),
        packs: vec!["kg".to_string()],
        ..Default::default()
    };
    let rt = KhiveRuntime::new(cfg).expect("runtime");
    KhiveMcpServer::new(rt).expect("server")
}

fn dispatch_on_large_current_thread_stack(
    thread_name: &str,
    ops: &'static str,
    expected_rows: usize,
    check_chain_indices: bool,
) {
    // Keep enough host stack to reach the constructor's named size assertion
    // when an owner-run mutation removes either production boxing boundary.
    let thread = std::thread::Builder::new()
        .name(thread_name.to_string())
        .stack_size(SERVER_ENTRY_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");
            runtime.block_on(Box::pin(async move {
                let parsed = parse_request(ops).expect("server-entry fixture uses valid batch DSL");
                assert_eq!(parsed.mode, ExecutionMode::Parallel);
                assert_eq!(parsed.ops.len(), expected_rows);
                let expected_ranges = if check_chain_indices {
                    vec![0..2, 2..4]
                } else {
                    vec![0..1, 1..2]
                };
                assert_eq!(parsed.ranges, expected_ranges);
                // execution.rs selects UnitTask only for a range longer than
                // one leaf; the flat bracketed batch must take BatchTask.
                assert_eq!(
                    parsed.ranges.iter().any(|range| range.len() > 1),
                    check_chain_indices,
                    "fixture must select its intended task constructor"
                );
                let server = make_server().await;
                let response = server
                    .dispatch_request_local(RequestParams {
                        ops: ops.to_string(),
                        presentation: Some("verbose".to_string()),
                        format: Some("json".to_string()),
                        ..Default::default()
                    })
                    .await
                    .expect("stats batch dispatches through the actual server entry");
                let envelope: Value = serde_json::from_str(&response).expect("request envelope");

                assert_eq!(envelope["summary"]["total"], expected_rows, "{envelope}");
                assert_eq!(
                    envelope["summary"]["succeeded"], expected_rows,
                    "{envelope}"
                );
                assert_eq!(envelope["summary"]["failed"], 0, "{envelope}");
                assert_eq!(envelope["summary"]["aborted"], 0, "{envelope}");

                let results = envelope["results"].as_array().expect("results");
                assert_eq!(results.len(), expected_rows, "{envelope}");
                for (index, row) in results.iter().enumerate() {
                    assert_eq!(row["tool"], "stats", "{envelope}");
                    assert_eq!(row["ok"], true, "{envelope}");
                    if check_chain_indices {
                        assert_eq!(row["op_index"], index, "{envelope}");
                        assert_eq!(row["unit_index"], index / 2, "{envelope}");
                        assert_eq!(row["step_index"], index % 2, "{envelope}");
                    } else {
                        assert!(row.get("unit_index").is_none(), "{envelope}");
                        assert!(row.get("step_index").is_none(), "{envelope}");
                    }
                }
            }));
        })
        .expect("large-stack server-entry test thread");

    if let Err(panic) = thread.join() {
        std::panic::resume_unwind(panic);
    }
}

#[test]
#[serial_test::serial(config_ledger)]
fn plain_batch_dispatch_checks_boxed_batch_task_size() {
    dispatch_on_large_current_thread_stack(
        "khive-batch-task-size-server-entry",
        "[stats(), stats()]",
        2,
        false,
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn bracketed_chains_dispatch_checks_boxed_unit_task_size() {
    dispatch_on_large_current_thread_stack(
        "khive-unit-task-size-server-entry",
        "[stats() | stats(), stats() | stats()]",
        4,
        true,
    );
}
