use super::*;

#[test]
fn restore_review_refuses_approval_through_uncovered_path() {
    let temp = TempDir::new().expect("temp dir");
    let target = "50000000-0000-4000-8000-000000000001";
    let other = "50000000-0000-4000-8000-000000000002";
    let restore = serde_json::json!({
        "op": "restore",
        "target_id": target,
        "record": {
            "substrate": "entity",
            "id": target,
            "namespace": "local",
            "created_at": 1,
            "updated_at": 2,
            "kind": "concept",
            "name": "Restored entity",
            "deleted_at": null
        }
    });
    let (changeset, rules) = write_case(
        temp.path(),
        "family:atlas",
        &[create(other, "Other entity"), restore.to_string()],
    );
    let before_entries = directory_entries(temp.path());
    let before_changeset = std::fs::read(&changeset).unwrap();
    let before_rules = std::fs::read(&rules).unwrap();
    let output = run_review(&changeset, &rules, Some("family:independent"));
    let report = parse_stdout(&output);
    assert!(
        !output.status.success(),
        "uncovered restore must refuse approval"
    );
    assert_eq!(report["validation"]["scope"], "commit_time_partial_view");
    assert_eq!(report["validation"]["passed"], false);
    assert_eq!(report["validation"]["errors"], 1);
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["rule_id"], "review-rule-coverage");
    assert_eq!(findings[0]["severity"], "error");
    assert_eq!(findings[0]["subject_id"], target);
    assert!(findings[0]["message"]
        .as_str()
        .unwrap()
        .contains("do not evaluate restore operations"));
    let operations = report["change_set"]["operations"].as_array().unwrap();
    assert_eq!(operations.len(), 2);
    assert_eq!(operations[0]["tier"], "tier_1");
    assert_eq!(operations[1]["index"], 1);
    assert_eq!(operations[1]["id"], target);
    assert_eq!(operations[1]["op"], "restore");
    assert_eq!(operations[1]["tier"], "tier_2");
    assert_eq!(
        operations[1]["tier_reasons"],
        serde_json::json!(["adr102.restore", "validation.error_severity_finding"])
    );
    assert_eq!(operations[1]["after"]["name"], "Restored entity");
    assert_eq!(report["review_gate"]["eligible"], true);
    assert_eq!(report["review_gate"]["status"], "blocked_by_findings");
    assert_eq!(report["review_gate"]["approval_ready"], false);
    assert_eq!(directory_entries(temp.path()), before_entries);
    assert_eq!(std::fs::read(&changeset).unwrap(), before_changeset);
    assert_eq!(std::fs::read(&rules).unwrap(), before_rules);
}

#[test]
fn version_one_is_refused_before_review() {
    let temp = TempDir::new().expect("temp dir");
    let (changeset, rules) = write_case(temp.path(), "family:atlas", &[]);
    let mut old: serde_json::Value = serde_json::from_str(&envelope("family:atlas")).unwrap();
    old["schema_version"] = serde_json::json!(1);
    std::fs::write(&changeset, format!("{old}\n")).unwrap();
    let output = run_review(&changeset, &rules, Some("family:independent"));
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("schema_version 1 is not supported"),
        "{stderr}"
    );
}
