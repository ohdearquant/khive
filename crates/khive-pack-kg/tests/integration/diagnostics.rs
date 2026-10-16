// ---- db_diagnostics: ADR-165 reader + ADR-133 writer contention fields ----

#[tokio::test]
async fn db_diagnostics_runtime_audit_fields_are_additive() {
    let pack = pack();

    let report = pack
        .dispatch("db_diagnostics", json!({}))
        .await
        .expect("db_diagnostics must succeed against an in-memory backend");

    let reader_contention = report
        .get("reader_contention")
        .expect("reader_contention section must be present");
    for field in [
        "reader_admission_capacity",
        "available_reader_admission_slots",
        "reader_acquisitions",
        "pooled_reader_checkouts",
        "standalone_reader_opens",
        "infrastructure_standalone_reader_opens",
        "reader_checkout_timeouts",
        "reader_busy_timeouts",
        "active_pooled_reader_checkouts",
        "peak_active_pooled_reader_checkouts",
        "completed_pooled_reader_checkouts",
        "max_completed_reader_hold_micros",
        "reader_discards",
        "reader_replacement_open_failures",
    ] {
        let value = reader_contention.get(field).unwrap_or_else(|| {
            panic!("reader_contention.{field} must be present in the wire payload")
        });
        assert!(
            value.is_u64(),
            "reader_contention.{field} must be a non-negative integer, got {value:?}"
        );
    }

    assert_eq!(
        reader_contention.get("reader_discards"),
        Some(&json!(0)),
        "the shared in-memory connection must not be discarded"
    );

    // The hold attribution is the one field here that is not a counter: it
    // names the operation behind `max_completed_reader_hold_micros`, or null
    // when that hold came through a route carrying no operation name (#2793).
    // Asserted separately because the loop above requires an integer, and a
    // field left out of both checks is a field that can disappear unnoticed.
    let attribution = reader_contention
        .get("max_completed_reader_hold_operation")
        .expect("reader_contention.max_completed_reader_hold_operation must be in the payload");
    assert!(
        attribution.is_null() || attribution.is_string(),
        "the hold attribution must be an operation name or null, got {attribution:?}"
    );

    let writer_contention = report
        .get("writer_contention")
        .expect("writer_contention section must be present");

    // Pool-sourced counters (ADR-133 D8): always concrete integers, never
    // Option, regardless of whether the caller is the runtime or a direct
    // khive-db user — they come straight off the pool.
    for field in [
        "writer_task_begin_busy",
        "direct_writer_busy_refusals",
        "writer_lease_timeouts",
        "configured_checkout_timeout_ms",
        "effective_writer_wait_bound_ms",
        "writer_task_begin_busy_absorbed",
        "writer_task_request_failures",
        "writer_task_side_effects_unknown",
    ] {
        let value = writer_contention.get(field).unwrap_or_else(|| {
            panic!("writer_contention.{field} must be present in the wire payload")
        });
        assert!(
            value.is_u64(),
            "writer_contention.{field} must serialize as a plain non-negative integer, got {value:?}"
        );
    }

    // Legacy field: population is unchanged by this additive change. Every
    // dispatch through the runtime supplies its process-wide swallowed-audit
    // count, so this remains a concrete number with no unavailable reason —
    // exactly today's behavior, unperturbed by the new fields.
    assert!(
        writer_contention
            .get("audit_append_failures")
            .is_some_and(Value::is_u64),
        "audit_append_failures must retain its existing store-operation population semantics: \
         {writer_contention:?}"
    );
    assert!(writer_contention
        .get("audit_append_failures_unavailable_reason")
        .map(Value::is_null)
        .unwrap_or(false));

    // Runtime audit-batch fields (ADR-133 D8 wire additions): additive and,
    // with no audit-batch control registered yet, unavailable with a reason
    // rather than a silently fabricated value.
    for (field, reason_field) in [
        (
            "audit_batch_flush_failures",
            "audit_batch_flush_failures_unavailable_reason",
        ),
        (
            "audit_degraded_rows",
            "audit_degraded_rows_unavailable_reason",
        ),
        ("audit_degraded", "audit_degraded_unavailable_reason"),
    ] {
        assert!(
            writer_contention
                .get(field)
                .map(Value::is_null)
                .unwrap_or(false),
            "writer_contention.{field} must be null until an audit-batch control is wired: \
             {writer_contention:?}"
        );
        assert!(
            writer_contention
                .get(reason_field)
                .and_then(Value::as_str)
                .is_some(),
            "writer_contention.{reason_field} must explain the unavailable field: \
             {writer_contention:?}"
        );
    }
}

// ---- db_diagnostics: backend selection and the shared read deadline ----

#[tokio::test]
async fn db_diagnostics_returns_a_partial_report_once_the_deadline_has_passed() {
    let pack = pack();

    let report = khive_storage::scope_request_read_deadline(
        std::time::Duration::ZERO,
        pack.dispatch("db_diagnostics", json!({})),
    )
    .await
    .expect("a passed deadline must return the partial report, not fail the verb");

    assert_eq!(report["complete"], json!(false), "{report}");
    let databases = report["databases"].as_array().expect("databases array");
    assert!(!databases.is_empty(), "{report}");
    for database in databases {
        assert!(database["diagnostics"].is_null(), "{database}");
        let error = database["error"].as_str().unwrap_or_default();
        assert!(
            error.starts_with("not inspected: request deadline"),
            "{database}"
        );
    }
}

#[tokio::test]
async fn db_diagnostics_without_a_deadline_is_complete() {
    let pack = pack();

    let report = pack
        .dispatch("db_diagnostics", json!({}))
        .await
        .expect("db_diagnostics must succeed against an in-memory backend");

    assert_eq!(report["complete"], json!(true), "{report}");
    assert!(report["databases"][0]["error"].is_null(), "{report}");
}

#[tokio::test]
async fn db_diagnostics_backend_selects_one_opened_backend_and_rejects_unknown_names() {
    let pack = pack();

    let report = pack
        .dispatch("db_diagnostics", json!({"backend": "main"}))
        .await
        .expect("main is always opened");
    let databases = report["databases"].as_array().expect("databases array");
    assert_eq!(databases.len(), 1, "{report}");
    assert_eq!(databases[0]["backend_names"], json!(["main"]));

    let err = pack
        .dispatch("db_diagnostics", json!({"backend": "no-such-backend"}))
        .await
        .expect_err("an unknown backend name must be refused");
    let message = invalid_input_message(&err);
    assert!(message.contains("\"no-such-backend\""), "{message}");
    assert!(message.contains("main"), "{message}");

    let err = pack
        .dispatch("db_diagnostics", json!({"size_composition": "no"}))
        .await
        .expect_err("size_composition must be a boolean");
    assert!(is_invalid_input(&err), "{err:?}");
}
