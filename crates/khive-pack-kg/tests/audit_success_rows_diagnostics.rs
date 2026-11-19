use khive_pack_kg::KgPack;
use khive_runtime::pack::AuditSuccessRowsMode;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

fn registry(runtime: &KhiveRuntime, mode: AuditSuccessRowsMode, sink: bool) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_audit_success_rows_mode(mode);
    builder.register(KgPack::new(runtime.clone()));
    if sink {
        builder
            .with_runtime_event_store(runtime)
            .expect("audit sink");
    }
    builder.build().expect("registry")
}

#[tokio::test]
async fn audit_success_rows_diagnostics_preserves_no_sink_health_and_registry_modes() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let reduced = registry(&runtime, AuditSuccessRowsMode::Reduced, false);
    let all = registry(&runtime, AuditSuccessRowsMode::All, false);
    for (registry, mode) in [(&reduced, "reduced"), (&all, "all"), (&reduced, "reduced")] {
        assert!(registry.audit_batch_metrics().is_none());
        let report = registry
            .dispatch("db_diagnostics", json!({"size_composition": false}))
            .await
            .expect("diagnostics");
        assert_eq!(report["audit_success_rows_mode"], mode);
        let writer = &report["writer_contention"];
        let old_health: serde_json::Map<String, Value> = [
            "audit_batch_flush_failures",
            "audit_degraded_rows",
            "audit_degraded",
            "audit_admission_refused_obligations",
            "audit_retry_exhausted_obligations",
            "audit_admission_unresolved_obligations",
        ]
        .into_iter()
        .flat_map(|key| {
            [
                (
                    key.to_owned(),
                    writer
                        .get(key)
                        .expect("old health field remains present")
                        .clone(),
                ),
                (
                    format!("{key}_unavailable_reason"),
                    writer
                        .get(format!("{key}_unavailable_reason"))
                        .expect("old unavailable reason remains present")
                        .clone(),
                ),
            ]
        })
        .chain([
            (
                "audit_admission_refused_obligations_last_at_ms".to_owned(),
                writer
                    .get("audit_admission_refused_obligations_last_at_ms")
                    .expect("old timestamp field remains present")
                    .clone(),
            ),
            (
                "audit_retry_exhausted_obligations_last_at_ms".to_owned(),
                writer
                    .get("audit_retry_exhausted_obligations_last_at_ms")
                    .expect("old timestamp field remains present")
                    .clone(),
            ),
            (
                "audit_admission_unresolved_obligations_last_at_ms".to_owned(),
                writer
                    .get("audit_admission_unresolved_obligations_last_at_ms")
                    .expect("old timestamp field remains present")
                    .clone(),
            ),
        ])
        .collect();
        assert_eq!(
            Value::Object(old_health),
            json!({
                "audit_batch_flush_failures": null,
                "audit_batch_flush_failures_unavailable_reason": "no audit-batch control is registered with this runtime instance",
                "audit_degraded_rows": null,
                "audit_degraded_rows_unavailable_reason": "no audit-batch control is registered with this runtime instance",
                "audit_degraded": null,
                "audit_degraded_unavailable_reason": "no audit-batch control is registered with this runtime instance",
                "audit_admission_refused_obligations": null,
                "audit_admission_refused_obligations_unavailable_reason": "no audit-batch control is registered with this runtime instance",
                "audit_retry_exhausted_obligations": null,
                "audit_retry_exhausted_obligations_unavailable_reason": "no audit-batch control is registered with this runtime instance",
                "audit_admission_unresolved_obligations": null,
                "audit_admission_unresolved_obligations_unavailable_reason": "no audit-batch control is registered with this runtime instance",
                "audit_admission_refused_obligations_last_at_ms": null,
                "audit_retry_exhausted_obligations_last_at_ms": null,
                "audit_admission_unresolved_obligations_last_at_ms": null,
            })
        );
    }
}

#[tokio::test]
async fn audit_success_rows_diagnostics_forwards_per_verb_maps_with_a_sink() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let registry = registry(&runtime, AuditSuccessRowsMode::All, true);
    let report = registry
        .dispatch("db_diagnostics", json!({"size_composition": false}))
        .await
        .expect("diagnostics");
    assert_eq!(report["audit_success_rows_mode"], "all");
    let writer = &report["writer_contention"];
    assert_eq!(
        writer["audit_success_rows_omitted"],
        json!({"comm.inbox": 0, "comm.probe": 0, "comm.cursor_get": 0, "whoami": 0})
    );
    assert_eq!(
        writer["audit_success_rows_sampled_out"],
        json!({"comm.heartbeat": 0, "comm.cleanup_expired_quarantine": 0})
    );
    assert_eq!(
        writer["audit_success_rows_forced"],
        json!({"comm.heartbeat": 0, "comm.cleanup_expired_quarantine": 0})
    );
    assert!(writer["audit_batch_flush_failures"].is_u64());
    assert!(writer["audit_batch_flush_failures_unavailable_reason"].is_null());
    registry.shutdown_audit_batch().await.expect("shutdown");
}
