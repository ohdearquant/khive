//! End-to-end integration test for the five built-in configurable
//! rule classes (`edge-endpoint-types`, `edge-direction-conventions`,
//! `dangling-refs`, `naming-conventions`, `citation-date-lint`).
//!
//! Drives the actual `kkernel` binary (`kg validate`) over a small on-disk
//! fixture rather than calling `cmd_validate` in-process: `cmd_validate`
//! calls `std::process::exit` directly on a failing report, which would kill
//! the test process if invoked in-tree. Spawning the compiled binary is also
//! the only way to exercise the real CLI argument parsing and output
//! formatting end-to-end, matching how an operator or a pre-commit hook
//! actually calls this surface.

use std::process::Command;

use tempfile::TempDir;

fn kkernel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

/// Writes a small NDJSON fixture plus a `rules.toml` that enables all five
/// rule classes, each configured to trip exactly one class of
/// violation, and returns the repo root.
fn write_fixture() -> TempDir {
    let tmp = TempDir::new().expect("create temp dir");
    let kg_dir = tmp.path().join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).expect("create .khive/kg");

    // Three entities:
    // - Alpha (concept): clean, used as a valid endpoint and as the
    //   `introduced_by` target below.
    // - "Bad Name (2024) " (person): trailing whitespace + parenthetical
    //   suffix (naming-conventions) and a forward-dated `year` property
    //   (citation-date-lint).
    // - Person Two (person): plain, used as an invalid `extends` endpoint.
    let entities = r#"{"id":"11111111-1111-1111-1111-111111111111","kind":"concept","name":"Alpha"}
{"id":"22222222-2222-2222-2222-222222222222","kind":"person","name":"Bad Name (2024) ","properties":{"year":9999}}
{"id":"33333333-3333-3333-3333-333333333333","kind":"person","name":"Person Two"}
"#;
    std::fs::write(kg_dir.join("entities.ndjson"), entities).expect("write entities.ndjson");

    // Three edges:
    // 1. person -[extends]-> person: not in the base/pack endpoint allowlist
    //    (edge-endpoint-types).
    // 2. concept -[extends]-> <missing uuid>: unresolved target
    //    (dangling-refs; also trips the always-on referential-integrity
    //    structural check).
    // 3. person -[introduced_by]-> concept: fails the base contract
    //    (edge-endpoint-types requires concept/artifact -> document/person)
    //    AND matches the configured reversed direction pattern below
    //    (edge-direction-conventions).
    let edges = r#"{"source_id":"22222222-2222-2222-2222-222222222222","target_id":"33333333-3333-3333-3333-333333333333","relation":"extends"}
{"source_id":"11111111-1111-1111-1111-111111111111","target_id":"55555555-5555-5555-5555-555555555555","relation":"extends"}
{"source_id":"22222222-2222-2222-2222-222222222222","target_id":"11111111-1111-1111-1111-111111111111","relation":"introduced_by"}
"#;
    std::fs::write(kg_dir.join("edges.ndjson"), edges).expect("write edges.ndjson");

    let rules_toml = r#"
[edge_endpoint_types]
enabled = true
severity = "error"

[edge_direction_conventions]
enabled = true
severity = "warning"

[[edge_direction_conventions.relations]]
relation = "introduced_by"
forward_source_kinds = ["concept", "artifact"]
forward_target_kinds = ["document", "person"]

[dangling_refs]
enabled = true
severity = "error"

[naming_conventions]
enabled = true
severity = "warning"
max_length = 100

[citation_date_lint]
enabled = true
severity = "warning"
fields = ["year"]
"#;
    std::fs::write(kg_dir.join("rules.toml"), rules_toml).expect("write rules.toml");

    tmp
}

#[test]
fn kg_validate_end_to_end_exercises_all_five_builtin_rule_classes() {
    let tmp = write_fixture();

    let output = Command::new(kkernel_bin())
        .args([
            "kg",
            "validate",
            "--repo",
            tmp.path().to_str().expect("utf8 tmp path"),
            "--format",
            "json",
            "--verbose",
        ])
        .output()
        .expect("run kkernel kg validate");

    // Two structural errors (edge-endpoint-types, dangling-refs) plus the
    // always-on referential-integrity check are error-severity, so the
    // overall report must fail and the CLI must hard-exit non-zero
    // (`kg/validate.rs`'s documented exit-code contract).
    assert!(
        !output.status.success(),
        "validate must exit non-zero when error-severity rules fail; stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be valid JSON: {e}\n{stdout}"));

    let rules = report["rules"]
        .as_array()
        .expect("report.rules must be an array");
    let find = |id: &str| -> &serde_json::Value {
        rules
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("rule {id:?} must appear in the report: {rules:#?}"))
    };

    let edge_endpoint_types = find("edge-endpoint-types");
    assert_eq!(edge_endpoint_types["severity"], "error");
    assert_eq!(edge_endpoint_types["passed"], false);
    assert!(
        !edge_endpoint_types["violations"]
            .as_array()
            .unwrap()
            .is_empty(),
        "edge-endpoint-types must report at least one violation"
    );

    let edge_direction_conventions = find("edge-direction-conventions");
    assert_eq!(edge_direction_conventions["severity"], "warning");
    assert_eq!(edge_direction_conventions["passed"], false);

    let dangling_refs = find("dangling-refs");
    assert_eq!(dangling_refs["severity"], "error");
    assert_eq!(dangling_refs["passed"], false);

    let naming_conventions = find("naming-conventions");
    assert_eq!(naming_conventions["severity"], "warning");
    assert_eq!(naming_conventions["passed"], false);
    let naming_violations = naming_conventions["violations"].as_array().unwrap();
    assert!(
        naming_violations.len() >= 2,
        "expected both a whitespace and a parenthetical-suffix violation: {naming_violations:#?}"
    );

    let citation_date_lint = find("citation-date-lint");
    assert_eq!(citation_date_lint["severity"], "warning");
    assert_eq!(citation_date_lint["passed"], false);

    // A well-formed edge (concept -> concept `extends`, both endpoints
    // resolvable) is present nowhere in this fixture's violation set — sanity
    // check that valid-edge-relations (a built-in, always-on check) still
    // passed, so we know the JSON parse picked up the real report and not an
    // empty/error stub.
    let valid_edge_relations = find("valid-edge-relations");
    assert_eq!(valid_edge_relations["passed"], true);
}

#[test]
fn kg_validate_no_rules_flag_skips_all_builtin_rule_classes() {
    let tmp = write_fixture();

    let output = Command::new(kkernel_bin())
        .args([
            "kg",
            "validate",
            "--repo",
            tmp.path().to_str().expect("utf8 tmp path"),
            "--format",
            "json",
            "--no-rules",
        ])
        .output()
        .expect("run kkernel kg validate --no-rules");

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be valid JSON: {e}\n{stdout}"));
    let rules = report["rules"].as_array().expect("report.rules array");

    for id in [
        "edge-endpoint-types",
        "edge-direction-conventions",
        "dangling-refs",
        "naming-conventions",
        "citation-date-lint",
    ] {
        assert!(
            !rules.iter().any(|r| r["id"] == id),
            "--no-rules must skip rules.toml entirely, including built-in rule {id:?}: {rules:#?}"
        );
    }

    // The always-on referential-integrity structural check still fires (edge
    // 2's target is genuinely missing) and `--no-rules` does not silence it.
    assert!(
        !output.status.success(),
        "structural checks must still fail under --no-rules"
    );
}
