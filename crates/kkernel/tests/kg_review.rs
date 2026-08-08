//! Binary-level acceptance tests for the read-only `kkernel kg review` slice
//! from ADR-145 D8. These exercise clap routing, strict ADR-101 parsing,
//! commit-time rule reuse, tier classification, JSON output, and the
//! cross-model-family gate without opening a repository or live database.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const CHANGESET_REVIEW_GOLDEN: &str =
    include_str!("../../../docs/schemas/examples/khive-review-v1-changeset.json");

fn kkernel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

fn envelope(family: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "producer": "lambda:atlas",
        "producer_model_family": family,
        "staged_at": 2_000_000_u64,
        "batch_id": "atlas-batch-7",
    })
    .to_string()
}

fn create(id: &str, name: &str) -> String {
    serde_json::json!({
        "op": "create",
        "id": id,
        "namespace": "local",
        "target": {
            "kind": "entity",
            "entity_kind": "concept",
            "name": name,
            "properties": {},
            "tags": [],
        },
    })
    .to_string()
}

fn link(id: &str, source: &str, target: &str, relation: &str, weight: f64) -> String {
    serde_json::json!({
        "op": "link",
        "id": id,
        "namespace": "local",
        "source": source,
        "target": target,
        "relation": relation,
        "weight": weight,
        "properties": {},
    })
    .to_string()
}

fn entity_update(id: &str, before: &str, after: &str) -> String {
    serde_json::json!({
        "op": "update",
        "target_id": id,
        "patch": {
            "target": "entity",
            "name": after,
        },
        "preimage": {
            "target": "entity",
            "name": before,
        },
    })
    .to_string()
}

fn write_case(dir: &Path, family: &str, ops: &[String]) -> (PathBuf, PathBuf) {
    let mut content = envelope(family);
    content.push('\n');
    for op in ops {
        content.push_str(op);
        content.push('\n');
    }

    let changeset = dir.join("changes.ndjson");
    let rules = dir.join("rules.toml");
    std::fs::write(&changeset, content).expect("write change-set");
    std::fs::write(&rules, "").expect("write rules");
    (changeset, rules)
}

fn run_review(changeset: &Path, rules: &Path, reviewer_model_family: Option<&str>) -> Output {
    let mut command = Command::new(kkernel_bin());
    command.args([
        "kg",
        "review",
        changeset.to_str().unwrap(),
        "--rules",
        rules.to_str().unwrap(),
        "--format",
        "json",
    ]);
    if let Some(family) = reviewer_model_family {
        command.args(["--reviewer-model-family", family]);
    }
    command.output().expect("run kkernel kg review")
}

fn parse_stdout(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "review stdout was not JSON: {error}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn directory_entries(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .expect("read temp directory")
        .map(|entry| {
            entry
                .expect("read entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn json_report_is_ordered_versioned_and_read_only() {
    let temp = TempDir::new().expect("temp dir");
    let alpha = "10000000-0000-4000-8000-000000000001";
    let beta = "10000000-0000-4000-8000-000000000002";
    let edge = "10000000-0000-4000-8000-000000000003";
    let (changeset, rules) = write_case(
        temp.path(),
        "family:atlas",
        &[
            create(alpha, "Alpha"),
            create(beta, "Beta"),
            link(edge, alpha, beta, "supports", 0.9),
        ],
    );
    let before_entries = directory_entries(temp.path());
    let before_changeset = std::fs::read(&changeset).unwrap();
    let before_rules = std::fs::read(&rules).unwrap();

    let output = run_review(&changeset, &rules, Some("family:independent"));
    assert!(
        output.status.success(),
        "independent review of a clean batch must pass; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = parse_stdout(&output);
    let golden: serde_json::Value =
        serde_json::from_str(CHANGESET_REVIEW_GOLDEN).expect("parse shared review golden");

    assert_eq!(
        report, golden,
        "Rust `kg review --format json` drifted from the shared khive.review.v1 changeset vector"
    );

    assert_eq!(report["schema_version"], "khive.review.v1");
    assert_eq!(report["review_kind"], "changeset");
    assert_eq!(report["capability"]["source"], "local_changeset");
    assert_eq!(report["capability"]["mutability"], "read_only");
    assert_eq!(report["capability"]["no_writes"], true);
    assert_eq!(report["capability"]["wasm"], false);
    assert_eq!(
        report["change_set"]["envelope"]["batch_id"],
        "atlas-batch-7"
    );

    let operations = report["change_set"]["operations"].as_array().unwrap();
    assert_eq!(operations.len(), 3);
    assert_eq!(operations[0]["index"], 0);
    assert_eq!(operations[0]["id"], alpha);
    assert_eq!(operations[1]["index"], 1);
    assert_eq!(operations[1]["id"], beta);
    assert_eq!(operations[2]["index"], 2);
    assert_eq!(operations[2]["id"], edge);
    assert_eq!(operations[2]["tier"], "tier_2");
    assert_eq!(report["tier_summary"]["tier_1"], 2);
    assert_eq!(report["tier_summary"]["tier_2"], 1);
    assert_eq!(report["validation"]["passed"], true);
    assert_eq!(report["review_gate"]["status"], "eligible");
    assert_eq!(report["review_gate"]["approval_ready"], true);

    assert_eq!(directory_entries(temp.path()), before_entries);
    assert_eq!(std::fs::read(&changeset).unwrap(), before_changeset);
    assert_eq!(std::fs::read(&rules).unwrap(), before_rules);
}

#[test]
fn same_family_reviewer_is_refused_with_machine_readable_gate() {
    let temp = TempDir::new().expect("temp dir");
    let alpha = "20000000-0000-4000-8000-000000000001";
    let beta = "20000000-0000-4000-8000-000000000002";
    let (changeset, rules) = write_case(
        temp.path(),
        "family:atlas",
        &[
            create(alpha, "Alpha"),
            create(beta, "Beta"),
            link(
                "20000000-0000-4000-8000-000000000003",
                alpha,
                beta,
                "refutes",
                1.0,
            ),
        ],
    );

    let output = run_review(&changeset, &rules, Some("family:atlas"));
    assert!(
        !output.status.success(),
        "same-family review must fail closed"
    );
    let report = parse_stdout(&output);
    assert_eq!(
        report["review_gate"]["status"],
        "ineligible_same_model_family"
    );
    assert_eq!(report["review_gate"]["eligible"], false);
    assert_eq!(report["review_gate"]["approval_ready"], false);
}

#[test]
fn strict_changeset_parse_rejects_unknown_fields() {
    let temp = TempDir::new().expect("temp dir");
    let changeset = temp.path().join("invalid.ndjson");
    let rules = temp.path().join("rules.toml");
    std::fs::write(
        &changeset,
        format!(
            "{}\n",
            serde_json::json!({
                "schema_version": 1,
                "producer": "lambda:atlas",
                "producer_model_family": "family:atlas",
                "staged_at": 2_000_000_u64,
                "invented": true,
            })
        ),
    )
    .unwrap();
    std::fs::write(&rules, "").unwrap();

    let output = run_review(&changeset, &rules, None);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("strict ADR-101 NDJSON-delta"), "{stderr}");
    assert!(stderr.contains("unknown field"), "{stderr}");
}

#[test]
fn error_rule_findings_are_reported_sorted_and_block_approval() {
    let temp = TempDir::new().expect("temp dir");
    let duplicate = "30000000-0000-4000-8000-000000000001";
    let (changeset, rules) = write_case(
        temp.path(),
        "family:atlas",
        &[create(duplicate, "Alpha"), create(duplicate, "Beta")],
    );

    let output = run_review(&changeset, &rules, Some("family:independent"));
    assert!(
        !output.status.success(),
        "error findings must block approval"
    );
    let report = parse_stdout(&output);

    assert_eq!(report["validation"]["passed"], false);
    assert_eq!(report["validation"]["errors"], 1);
    assert_eq!(report["findings"][0]["rule_id"], "no-duplicate-uuids");
    assert_eq!(report["findings"][0]["severity"], "error");
    assert_eq!(report["tier_summary"]["tier_2"], 2);
    assert_eq!(report["review_gate"]["eligible"], true);
    assert_eq!(report["review_gate"]["status"], "blocked_by_findings");
    assert_eq!(report["review_gate"]["approval_ready"], false);
}

#[test]
fn uncovered_update_rules_fail_closed_in_partial_view() {
    let temp = TempDir::new().expect("temp dir");
    let target = "40000000-0000-4000-8000-000000000001";
    let (changeset, rules) = write_case(
        temp.path(),
        "family:atlas",
        &[entity_update(target, "Old name", "New name")],
    );

    let output = run_review(&changeset, &rules, Some("family:independent"));
    assert!(
        !output.status.success(),
        "partial-view rules must not approve an uncovered update"
    );
    let report = parse_stdout(&output);

    assert_eq!(report["validation"]["scope"], "commit_time_partial_view");
    assert_eq!(report["validation"]["passed"], false);
    assert_eq!(report["findings"][0]["rule_id"], "review-rule-coverage");
    assert_eq!(report["findings"][0]["subject_id"], target);
    assert_eq!(report["change_set"]["operations"][0]["tier"], "tier_2");
    assert_eq!(
        report["change_set"]["operations"][0]["tier_reasons"][1],
        "validation.error_severity_finding"
    );
    assert_eq!(report["review_gate"]["status"], "blocked_by_findings");
    assert_eq!(report["review_gate"]["approval_ready"], false);
}
