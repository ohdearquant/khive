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
    let summary = ops_file_summary(OpsFileReportMode::LegacyNoSave, 2, 1, 1, 0, 0, failures, 0);

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

#[test]
fn ops_file_budget_summary_exposes_refused_count_only_when_nonzero() {
    let parsed = serde_json::json!({
        "results": [
            {"ok":true,"tool":"first","result":{}},
            {
                "ok":false,
                "tool":"second",
                "error":{"kind":"response_budget_exceeded","code":"response_budget_exceeded"}
            }
        ]
    });
    let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
    let summary = ops_file_summary(
        OpsFileReportMode::LegacyNoSave,
        2,
        1,
        1,
        0,
        1,
        failures,
        0,
    );
    assert_eq!(summary["response_budget_refused"], 1);

    let save_summary = ops_file_summary(
        OpsFileReportMode::BoundedSave,
        2,
        1,
        1,
        0,
        1,
        Vec::new(),
        0,
    );
    assert_eq!(save_summary["response_budget_refused"], 1);

    let clean_summary = ops_file_summary(
        OpsFileReportMode::LegacyNoSave,
        1,
        1,
        0,
        0,
        0,
        Vec::new(),
        0,
    );
    assert!(clean_summary.get("response_budget_refused").is_none());
}

#[tokio::test]
async fn ops_file_budget_refusal_returns_error_with_partial_success() {
    let (server, _, _) = concurrency_probe_server(false);
    let save_dir = tempfile::tempdir().expect("save directory");
    let save_path = save_dir.path().join("ops.jsonl");
    let save_path_arg = save_path.to_str().expect("UTF-8 save path").to_string();
    let result = apply_ops_file_with_response_transform(
        &server,
        reader_probe_ops(2),
        Some("verbose".to_string()),
        None,
        Some(save_path_arg),
        false,
        |_, raw| {
            let mut response: serde_json::Value =
                serde_json::from_str(&raw).expect("dispatch result is JSON");
            response["results"][1] = serde_json::json!({
                "ok": false,
                "tool": "ops_file_concurrency_probe.reader_probe",
                "error": {
                    "kind": "response_budget_exceeded",
                    "code": "response_budget_exceeded",
                    "message": "per-request batch response budget of 4194304 serialized result bytes exceeded"
                }
            });
            response["summary"]["succeeded"] = serde_json::json!(1);
            response["summary"]["failed"] = serde_json::json!(1);
            response["status"] = serde_json::json!("partial");
            serde_json::to_string(&response).expect("serialize transformed batch")
        },
    )
    .await;

    let error = result.expect_err("a budget refusal must make the ops-file call fail");
    assert!(
        error.to_string().contains("response budget refused 1 op(s)"),
        "unexpected refusal error: {error}"
    );
    let saved_rows = std::fs::read_to_string(&save_path).expect("save file published");
    assert_eq!(saved_rows.lines().count(), 2);
}

#[tokio::test]
async fn ops_file_budget_control_keeps_successful_batch_green() {
    let (server, _, _) = concurrency_probe_server(false);
    let save_dir = tempfile::tempdir().expect("save directory");
    let save_path = save_dir.path().join("ops.jsonl");
    let save_path_arg = save_path.to_str().expect("UTF-8 save path").to_string();
    let manifest = apply_ops_file_with_response_transform(
        &server,
        reader_probe_ops(2),
        Some("verbose".to_string()),
        None,
        Some(save_path_arg),
        false,
        |_, raw| raw,
    )
    .await
    .expect("a clean batch without budget refusals remains successful");

    assert_eq!(manifest["summary"]["total"], 2);
    assert_eq!(manifest["summary"]["succeeded"], 2);
    assert_eq!(manifest["summary"]["failed"], 0);
    assert!(manifest["summary"].get("response_budget_refused").is_none());
}
