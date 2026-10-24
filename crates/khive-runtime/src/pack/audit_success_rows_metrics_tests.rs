use std::process::Command;

use crate::KhiveRuntime;

use super::audit_success_rows::AuditSuccessRowDecision;
use super::{AuditSuccessRowsMode, VerbRegistry, VerbRegistryBuilder};

fn audited_registry(runtime: &KhiveRuntime, mode: AuditSuccessRowsMode) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_audit_success_rows_mode(mode);
    builder
        .with_runtime_event_store(runtime)
        .expect("audit sink");
    builder.build().expect("registry")
}

#[tokio::test]
async fn audit_success_rows_counters_are_process_global_and_mode_scoped() {
    const CHILD: &str = "KHIVE_TEST_AUDIT_SUCCESS_ROWS_METRICS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "pack::audit_success_rows_metrics_tests::audit_success_rows_counters_are_process_global_and_mode_scoped", "--nocapture"])
            .env(CHILD, "1").output().expect("fresh counter process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "one isolated test: {stdout}");
        return;
    }
    use AuditSuccessRowDecision::{Forced, Omitted, SampledOut};
    let runtime = KhiveRuntime::memory().expect("runtime");
    let reduced = audited_registry(&runtime, AuditSuccessRowsMode::Reduced);
    let all = audited_registry(&runtime, AuditSuccessRowsMode::All);
    let initial = reduced.audit_batch_metrics().expect("metrics");
    assert_eq!(
        initial.audit_success_rows_omitted,
        [
            ("comm.inbox", 0),
            ("comm.probe", 0),
            ("comm.cursor_get", 0),
            ("whoami", 0)
        ]
    );
    assert_eq!(
        initial.audit_success_rows_sampled_out,
        [
            ("comm.heartbeat", 0),
            ("comm.cleanup_expired_quarantine", 0)
        ]
    );
    assert_eq!(
        initial.audit_success_rows_forced,
        [
            ("comm.heartbeat", 0),
            ("comm.cleanup_expired_quarantine", 0)
        ]
    );

    // Exercise the recording primitive directly; dispatch begins using it at its deferred cut.
    for (verb, decision, count) in [
        ("comm.inbox", Omitted, 1),
        ("comm.probe", Omitted, 2),
        ("comm.cursor_get", Omitted, 3),
        ("whoami", Omitted, 4),
        ("comm.heartbeat", SampledOut, 5),
        ("comm.cleanup_expired_quarantine", SampledOut, 6),
        ("comm.heartbeat", Forced, 7),
        ("comm.cleanup_expired_quarantine", Forced, 8),
    ] {
        for _ in 0..count {
            reduced.record_audit_success_row_decision(verb, decision);
        }
    }
    let expected = reduced.audit_batch_metrics().expect("recorded metrics");
    assert_eq!(
        expected.audit_success_rows_omitted,
        [
            ("comm.inbox", 1),
            ("comm.probe", 2),
            ("comm.cursor_get", 3),
            ("whoami", 4)
        ]
    );
    assert_eq!(
        expected.audit_success_rows_sampled_out,
        [
            ("comm.heartbeat", 5),
            ("comm.cleanup_expired_quarantine", 6)
        ]
    );
    assert_eq!(
        expected.audit_success_rows_forced,
        [
            ("comm.heartbeat", 7),
            ("comm.cleanup_expired_quarantine", 8)
        ]
    );
    for decision in [Omitted, SampledOut, Forced] {
        for verb in [
            "comm.inbox",
            "comm.probe",
            "comm.cursor_get",
            "whoami",
            "comm.heartbeat",
            "comm.cleanup_expired_quarantine",
        ] {
            all.record_audit_success_row_decision(verb, decision);
        }
        for verb in ["stats", "unknown.poll"] {
            reduced.record_audit_success_row_decision(verb, decision);
        }
    }
    reduced.record_audit_success_row_decision("comm.heartbeat", Omitted);
    reduced.record_audit_success_row_decision("whoami", SampledOut);
    reduced.record_audit_success_row_decision("whoami", Forced);
    assert_eq!(reduced.audit_batch_metrics(), Some(expected));
    assert_eq!(
        all.audit_batch_metrics(),
        Some(expected),
        "all reports the same process counts without adding decisions"
    );
    let later = audited_registry(&runtime, AuditSuccessRowsMode::Reduced);
    assert_eq!(
        later.audit_batch_metrics(),
        Some(expected),
        "building a registry never resets process counters"
    );
    later.record_audit_success_row_decision("comm.inbox", Omitted);
    assert_eq!(
        reduced
            .audit_batch_metrics()
            .unwrap()
            .audit_success_rows_omitted[0],
        ("comm.inbox", 2)
    );
    assert_eq!(all.audit_batch_metrics(), later.audit_batch_metrics());
    reduced
        .shutdown_audit_batch()
        .await
        .expect("reduced shutdown");
    all.shutdown_audit_batch().await.expect("all shutdown");
    later.shutdown_audit_batch().await.expect("later shutdown");
}
