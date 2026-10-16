// ── collect_op_failures: per-op error surfacing (#1228) ───────────────────

#[test]
fn collect_op_failures_extracts_reason_with_global_index() {
    let parsed = serde_json::json!({
        "results": [
            {"ok": true, "tool": "create", "result": {}},
            {"ok": false, "tool": "create", "error": "content rejected: suspected credential material"},
            {"ok": false, "tool": "link"},
        ],
        "summary": {"total": 3, "succeeded": 1, "failed": 2}
    });
    let failures = collect_op_failures(&parsed, 500, OpsFileReportMode::LegacyNoSave);
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0]["op_index"], 501);
    assert_eq!(failures[0]["tool"], "create");
    assert_eq!(
        failures[0]["error"],
        "content rejected: suspected credential material"
    );
    assert_eq!(failures[1]["op_index"], 502);
    assert_eq!(
        failures[1]["error"], "unknown error",
        "a failed entry with no error value still surfaces a placeholder"
    );
}

#[test]
fn collect_op_failures_preserves_structured_error_payloads() {
    let parsed = serde_json::json!({
        "results": [
            {"ok": false, "tool": "create",
             "error": {"kind": "invalid_input", "message": "content rejected"}},
        ],
        "summary": {"total": 1, "succeeded": 0, "failed": 1}
    });
    let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
    assert_eq!(
        failures[0]["error"],
        serde_json::json!({"kind": "invalid_input", "message": "content rejected"}),
        "structured KhiveError payloads pass through as JSON, not a placeholder"
    );
}

#[test]
fn collect_op_failures_preserves_stable_refusal_reason() {
    let parsed = serde_json::json!({
        "results": [
            {
                "ok": false,
                "tool": "not_loaded",
                "error": "unknown verb",
                "reason": "verb-refused"
            },
        ],
        "summary": {"total": 1, "succeeded": 0, "failed": 1}
    });
    let failures = collect_op_failures(&parsed, 9, OpsFileReportMode::BoundedSave);
    assert_eq!(failures[0]["reason"], "verb-refused");
    assert_eq!(failures[0]["op_index"], 9);
    let legacy = collect_op_failures(&parsed, 9, OpsFileReportMode::LegacyNoSave);
    assert!(
        legacy[0].get("reason").is_none(),
        "legacy no-save summary must retain its pre-reason wire shape"
    );
}

#[test]
fn collect_op_failures_empty_on_all_ok_or_missing_results() {
    let all_ok = serde_json::json!({
        "results": [{"ok": true, "tool": "stats", "result": {}}],
        "summary": {"total": 1, "succeeded": 1, "failed": 0}
    });
    assert!(collect_op_failures(&all_ok, 0, OpsFileReportMode::LegacyNoSave).is_empty());
    assert!(
        collect_op_failures(&serde_json::json!({}), 0, OpsFileReportMode::LegacyNoSave).is_empty()
    );
}

#[test]
fn no_save_reporting_matches_exact_legacy_golden() {
    let parsed = serde_json::json!({
        "results": [
            {"ok": true, "tool": "create", "result": {}},
            {"ok": false, "tool": "search", "error": "boom"},
        ],
    });
    let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].get("aborted").is_none());
    let summary = ops_file_summary(OpsFileReportMode::LegacyNoSave, 2, 1, 1, 0, failures, 0);

    assert_eq!(
        ops_file_progress_line(OpsFileReportMode::LegacyNoSave, 2, 2, 1, 1, 0),
        "applied 2/2 (ok=1, failed=1)"
    );
    assert_eq!(
        serde_json::to_string_pretty(&summary).unwrap(),
        concat!(
            "{\n",
            "  \"failed\": 1,\n",
            "  \"failures\": [\n",
            "    {\n",
            "      \"error\": \"boom\",\n",
            "      \"op_index\": 1,\n",
            "      \"tool\": \"search\"\n",
            "    }\n",
            "  ],\n",
            "  \"succeeded\": 1,\n",
            "  \"total\": 2\n",
            "}"
        )
    );
    assert!(summary.get("aborted").is_none());
    assert!(summary.get("failure_details_omitted").is_none());
}

#[test]
fn no_save_reporting_retains_more_than_one_thousand_failures() {
    let parsed = serde_json::json!({
        "results": (0..=MAX_OPS_FILE_FAILURE_DETAILS)
            .map(|index| serde_json::json!({
                "ok": false,
                "tool": "create",
                "error": format!("failure-{index}"),
            }))
            .collect::<Vec<_>>(),
    });
    let mut retained = Vec::new();
    let mut omitted = 0;
    for failure in collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave) {
        assert!(retain_failure_detail(
            OpsFileReportMode::LegacyNoSave,
            failure,
            &mut retained,
            &mut omitted,
        ));
    }
    assert_eq!(retained.len(), MAX_OPS_FILE_FAILURE_DETAILS + 1);
    assert_eq!(omitted, 0);

    let summary = ops_file_summary(
        OpsFileReportMode::LegacyNoSave,
        retained.len(),
        0,
        retained.len(),
        0,
        retained,
        omitted,
    );
    assert_eq!(
        summary["failures"].as_array().unwrap().len(),
        MAX_OPS_FILE_FAILURE_DETAILS + 1
    );
    assert!(summary.get("failure_details_omitted").is_none());
}

#[test]
fn no_save_reporting_retains_error_larger_than_four_kib() {
    let large_error = "x".repeat(MAX_OPS_FILE_FAILURE_ERROR_BYTES + 1);
    let parsed = serde_json::json!({
        "results": [{"ok": false, "tool": "create", "error": large_error}],
    });
    let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
    assert_eq!(
        failures[0]["error"].as_str().unwrap().len(),
        MAX_OPS_FILE_FAILURE_ERROR_BYTES + 1
    );
    assert_eq!(failures[0]["error"], large_error);
}

#[test]
fn save_reporting_bounds_failure_count_and_error_detail() {
    let large_error = "x".repeat(MAX_OPS_FILE_FAILURE_ERROR_BYTES + 1);
    let parsed = serde_json::json!({
        "results": (0..=MAX_OPS_FILE_FAILURE_DETAILS)
            .map(|index| serde_json::json!({
                "ok": false,
                "tool": "create",
                "aborted": index % 2 == 0,
                "error": if index == 0 {
                    serde_json::Value::String(large_error.clone())
                } else {
                    serde_json::Value::String(format!("failure-{index}"))
                },
            }))
            .collect::<Vec<_>>(),
    });
    let mut retained = Vec::new();
    let mut omitted = 0;
    for failure in collect_op_failures(&parsed, 0, OpsFileReportMode::BoundedSave) {
        retain_failure_detail(
            OpsFileReportMode::BoundedSave,
            failure,
            &mut retained,
            &mut omitted,
        );
    }
    assert_eq!(retained.len(), MAX_OPS_FILE_FAILURE_DETAILS);
    assert_eq!(omitted, 1);
    assert_eq!(retained[0]["aborted"], true);
    assert_eq!(
        retained[0]["error"],
        format!(
            "error detail omitted: exceeds {MAX_OPS_FILE_FAILURE_ERROR_BYTES}-byte ops-file diagnostic limit"
        )
    );

    let summary = ops_file_summary(
        OpsFileReportMode::BoundedSave,
        MAX_OPS_FILE_FAILURE_DETAILS + 1,
        0,
        MAX_OPS_FILE_FAILURE_DETAILS + 1,
        0,
        retained,
        omitted,
    );
    assert_eq!(summary["failure_details_omitted"], 1);
    assert_eq!(
        summary["failures"].as_array().unwrap().len(),
        MAX_OPS_FILE_FAILURE_DETAILS
    );
}
