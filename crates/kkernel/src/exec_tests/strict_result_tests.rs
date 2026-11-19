// ── #1220: --strict exit-code signal for partially-failed batches ─────────

#[test]
fn prepare_exec_output_preserves_specific_reasons_and_fills_strict_failures() {
    let raw = serde_json::json!({
        "results": [
            {"ok": true, "tool": "stats", "result": {}},
            {"ok": false, "tool": "get", "error": "missing id"},
            {
                "ok": false,
                "tool": "not_loaded",
                "error": "unknown verb",
                "reason": "verb-refused"
            },
            {"ok": false, "tool": "update", "aborted": true},
        ],
        "summary": {"total": 4, "succeeded": 1, "failed": 2, "aborted": 1},
        "status": "partial",
    })
    .to_string();

    let parsed: serde_json::Value = serde_json::from_str(&prepare_exec_output(&raw, true)).unwrap();
    assert_eq!(parsed["results"][1]["reason"], "strict-op-failure");
    assert_eq!(parsed["results"][2]["reason"], "verb-refused");
    assert_eq!(parsed["results"][3]["reason"], "strict-op-failure");
    assert!(parsed["results"][0].get("reason").is_none());
}

#[test]
fn enforce_strict_batch_result_ok_when_strict_off_and_partially_failed() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 2, "succeeded": 1, "failed": 1, "aborted": 0},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, false).is_ok());
}

// ── #1339: fully-failed batches exit non-zero even without --strict ──────

#[test]
fn enforce_strict_batch_result_errs_when_strict_off_and_every_op_failed() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 1, "succeeded": 0, "failed": 1, "aborted": 0},
    })
    .to_string();
    let err = enforce_strict_batch_result(&raw, false).unwrap_err();
    assert!(format!("{err}").contains("every op failed"));
}

#[test]
fn enforce_strict_batch_result_errs_when_strict_off_and_chain_fully_aborted() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 3, "succeeded": 0, "failed": 1, "aborted": 2},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, false).is_err());
}

#[test]
fn enforce_strict_batch_result_ok_on_empty_batch_summary() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 0, "succeeded": 0, "failed": 0, "aborted": 0},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, false).is_ok());
    assert!(enforce_strict_batch_result(&raw, true).is_ok());
}

#[test]
fn enforce_strict_batch_result_ok_when_strict_on_and_nothing_failed() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 2, "succeeded": 2, "failed": 0, "aborted": 0},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, true).is_ok());
}

#[test]
fn enforce_strict_batch_result_errs_when_strict_on_and_a_failure_present() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 2, "succeeded": 1, "failed": 1, "aborted": 0},
    })
    .to_string();
    let err = enforce_strict_batch_result(&raw, true).unwrap_err();
    assert!(format!("{err}").contains("1 op(s) failed"));
}

#[test]
fn enforce_strict_batch_result_errs_when_strict_on_and_chain_aborted() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 2, "succeeded": 0, "failed": 1, "aborted": 1},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, true).is_err());
}

#[test]
fn enforce_strict_batch_result_errs_on_save_manifest_with_failures() {
    // The save-file path prints a manifest, not the raw envelope; the
    // manifest carries the envelope's summary through (khive-mcp
    // save_sink) precisely so --strict works on this path too.
    let raw = r#"{"path":"/tmp/out.jsonl","rows":2,"checksum":"ab","summary":{"total":2,"succeeded":1,"failed":1,"aborted":0}}"#;
    assert!(enforce_strict_batch_result(raw, true).is_err());
    let clean = r#"{"path":"/tmp/out.jsonl","rows":2,"checksum":"ab","summary":{"total":2,"succeeded":2,"failed":0,"aborted":0}}"#;
    assert!(enforce_strict_batch_result(clean, true).is_ok());
}

#[test]
fn enforce_strict_batch_result_ok_on_non_json_output() {
    // --output-format table/auto renders a non-JSON string; --strict has
    // nothing to inspect and must not itself error out on that shape.
    assert!(enforce_strict_batch_result("| a | b |\n", true).is_ok());
}

/// Asserts a `dispatch_request_local` response matches the pre-ADR-099-B1
/// golden shape: exactly the top-level keys `results` and `summary` (no
/// additive `atomic` block — that is a future, opt-in-only slice), a
/// `summary` with exactly `total`/`succeeded`/`failed`/`aborted`, and a
/// successful single-op `results[0]` carrying `ok`/`tool`/`result`.
fn assert_golden_envelope_shape(resp: &serde_json::Value, expected_tool: &str) {
    let top_level_keys: std::collections::BTreeSet<&str> = resp
        .as_object()
        .expect("response must be a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        top_level_keys,
        std::collections::BTreeSet::from(["results", "summary", "status"]),
        "non-atomic envelope must carry exactly results+summary+status, no `atomic` block (#1220 added `status`): {resp}"
    );

    let summary_keys: std::collections::BTreeSet<&str> = resp["summary"]
        .as_object()
        .expect("summary must be an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        summary_keys,
        std::collections::BTreeSet::from(["total", "succeeded", "failed", "aborted"]),
        "summary shape must be unchanged: {resp}"
    );
    assert_eq!(resp["summary"]["total"], serde_json::json!(1));
    assert_eq!(resp["summary"]["succeeded"], serde_json::json!(1));
    assert_eq!(resp["summary"]["failed"], serde_json::json!(0));

    assert_eq!(resp["results"][0]["ok"], serde_json::json!(true));
    assert_eq!(resp["results"][0]["tool"], serde_json::json!(expected_tool));
    assert!(
        resp["results"][0].get("result").is_some(),
        "results[0] must carry a `result` field: {resp}"
    );
}
