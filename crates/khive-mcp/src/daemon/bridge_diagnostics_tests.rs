use super::*;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

#[test]
#[serial_test::serial]
fn snapshot_contains_exact_five_reasons_and_a_checked_total() {
    reset_fallback_counters();
    for reason in [
        FallbackReason::ConfigMismatch,
        FallbackReason::NamespaceMismatch,
        FallbackReason::NoSocket,
        FallbackReason::ParseFailure,
        FallbackReason::ProtocolMismatch,
    ] {
        record_fallback(reason, "client", Some("daemon"), "local");
    }
    let snapshot = serde_json::to_value(bridge_diagnostics_snapshot().unwrap()).unwrap();
    assert_eq!(
        snapshot["bridge_instance_id"],
        crate::server::bridge_instance_id().to_string()
    );
    assert_eq!(snapshot["pid"], json!(std::process::id()));
    let reasons = snapshot["fallback_reasons"].as_object().unwrap();
    assert_eq!(reasons.len(), 5);
    for reason in [
        "config_mismatch",
        "namespace_mismatch",
        "no_socket",
        "parse_failure",
        "protocol_mismatch",
    ] {
        assert_eq!(reasons[reason], 1);
    }
    assert_eq!(snapshot["fallback_total"], 5);
    reset_fallback_counters();
}

#[test]
#[serial_test::serial]
fn snapshot_refuses_sum_overflow_instead_of_wrapping() {
    reset_fallback_counters();
    FALLBACK_NO_SOCKET.store(usize::MAX, Ordering::SeqCst);
    FALLBACK_PARSE_FAILURE.store(1, Ordering::SeqCst);
    assert!(bridge_diagnostics_snapshot().is_none());
    reset_fallback_counters();
}

#[tokio::test]
async fn direct_daemon_dispatch_refuses_bridge_control_before_siblings() {
    let server = crate::server::KhiveMcpServer::from_registry(
        khive_runtime::VerbRegistryBuilder::new().build().unwrap(),
    );
    for ops in [
        "bridge.diagnostics()",
        "bridge.diagnostics(help=true)",
        "[stats(), bridge.diagnostics()]",
    ] {
        let error =
            <crate::server::KhiveMcpServer as daemon::DaemonDispatch>::dispatch_with_error_detail(
                &server,
                ops.into(),
                None,
                None,
                None,
                None,
                true,
                None,
            )
            .await
            .expect_err("daemon frames cannot serve bridge control");
        assert!(error.message.contains("only on the stdio bridge"));
    }
    let daemon_plan: Value = serde_json::from_str(
        &<crate::server::KhiveMcpServer as daemon::DaemonDispatch>::plan(
            &server,
            "bridge.diagnostics()",
        ),
    )
    .unwrap();
    assert_eq!(daemon_plan["stages"][0]["known"], false);
}
