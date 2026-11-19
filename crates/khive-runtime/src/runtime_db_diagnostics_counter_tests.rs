#[tokio::test]
async fn runtime_db_diagnostics_supplies_both_contention_counter_sources() {
    let dir = tempfile::tempdir().expect("diagnostics database directory");
    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = Some(dir.path().join("runtime-diagnostics.db"));
    let rt = KhiveRuntime::new_for_test(config).expect("file-backed runtime should create");

    let report = rt.db_diagnostics().await.expect("diagnostics succeed");

    #[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
    assert_eq!(
        report.wal_pin.reporting_process_is_holder,
        Some(true),
        "the runtime report identifies its own process as a database holder"
    );

    assert!(
        report.writer_contention.writer_acquisitions >= 1,
        "runtime construction runs migrations through the finite-wait pooled writer"
    );
    assert_eq!(
        report.writer_contention.writer_acquisitions,
        report
            .writer_contention
            .pooled_writer_acquisitions
            .saturating_add(report.writer_contention.standalone_writer_acquisitions)
            .saturating_add(report.writer_contention.writer_task_acquisitions),
        "the public aggregate must equal the class-specific snapshot"
    );
    assert!(
        report.writer_contention.audit_append_failures.is_some(),
        "the runtime path must supply its process-wide swallowed-audit counter"
    );
    assert!(report
        .writer_contention
        .audit_obligation_append_failures
        .is_some());
    assert!(report
        .writer_contention
        .audit_obligation_append_failures_unavailable_reason
        .is_none());
    assert!(report
        .writer_contention
        .audit_append_failures_unavailable_reason
        .is_none());
}
