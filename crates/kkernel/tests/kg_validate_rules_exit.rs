//! ADR-034 distinguishes invalid rules files from failed graph rules.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const ENTITIES: &str =
    "{\"id\":\"11111111-1111-1111-1111-111111111111\",\"kind\":\"concept\",\"name\":\"Alpha\"}\n";

fn fixture() -> TempDir {
    let tmp = TempDir::new().expect("create private fixture");
    let kg_dir = tmp.path().join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).expect("create KG directory");
    std::fs::write(kg_dir.join("entities.ndjson"), ENTITIES).expect("write entities");
    std::fs::write(kg_dir.join("edges.ndjson"), "").expect("write edges");
    tmp
}

fn validate(tmp: &TempDir, rules: Option<&Path>, no_rules: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    // Isolate child configuration while retaining Cargo's store-access guard.
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| key.starts_with("KHIVE_") && key != "KHIVE_TEST_HARNESS")
        {
            command.env_remove(key);
        }
    }
    command
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("KHIVE_VOLUME_LOCK_DIR", tmp.path().join("volume-locks"))
        .args(["kg", "validate", "--repo"])
        .arg(tmp.path())
        .args(["--format", "json"]);
    if let Some(rules) = rules {
        command.arg("--rules").arg(rules);
    }
    if no_rules {
        command.arg("--no-rules");
    }
    let output = command
        .output()
        .expect("run validator against private fixture");
    let kg_dir = tmp.path().join(".khive/kg");
    assert_eq!(
        std::fs::read_to_string(kg_dir.join("entities.ndjson")).unwrap(),
        ENTITIES
    );
    assert_eq!(std::fs::read(kg_dir.join("edges.ndjson")).unwrap(), b"");
    assert!(!kg_dir.join("notes.ndjson").exists());
    output
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON report: {error}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn malformed_default_and_explicit_toml_exit_two() {
    for explicit in [false, true] {
        let tmp = fixture();
        let rules = if explicit {
            tmp.path().join("custom.toml")
        } else {
            tmp.path().join(".khive/kg/rules.toml")
        };
        std::fs::write(&rules, "[").unwrap();
        let output = validate(&tmp, explicit.then_some(rules.as_path()), false);
        assert_exit(&output, 2);
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("parse rules TOML {}", rules.display())),
            "{stderr}"
        );
        assert!(stderr.contains("TOML parse error"), "{stderr}");
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), "[");
    }
}

#[test]
fn unsupported_yaml_and_yml_exit_two() {
    for extension in ["yaml", "yml"] {
        let tmp = fixture();
        let rules = tmp.path().join(format!("rules.{extension}"));
        std::fs::write(&rules, "rules: []\n").unwrap();
        let output = validate(&tmp, Some(&rules), false);
        assert_exit(&output, 2);
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("rules file {rules:?}")),
            "{stderr}"
        );
        assert!(
            stderr.contains("YAML format which is not supported"),
            "{stderr}"
        );
        assert!(stderr.contains("use TOML format instead"), "{stderr}");
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), "rules: []\n");
    }
}

#[test]
fn rules_read_error_retains_exit_one() {
    let tmp = fixture();
    let rules = tmp.path().join("unreadable.toml");
    std::fs::create_dir(&rules).unwrap();
    let output = validate(&tmp, Some(&rules), false);
    assert_exit(&output, 1);
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("read rules file {}", rules.display())),
        "{stderr}"
    );
    assert!(rules.is_dir());
}

#[test]
fn valid_rules_and_graph_violations_retain_zero_and_one() {
    for (field, exit_code, errors) in [("name", 0, 0), ("description", 1, 1)] {
        let tmp = fixture();
        let rules = tmp.path().join(".khive/kg/rules.toml");
        let content = format!(
            "[[rules]]\nid = \"required-field\"\nkind = \"entity\"\nseverity = \"error\"\ncondition = \"kind=concept\"\nrequire_field = \"{field}\"\n"
        );
        std::fs::write(&rules, &content).unwrap();
        let output = validate(&tmp, None, false);
        assert_exit(&output, exit_code);
        let report = report(&output);
        assert_eq!(report["summary"]["errors"], errors);
        assert_eq!(report["summary"]["passed"], exit_code == 0);
        let rule = report["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule["id"] == "required-field")
            .expect("loaded configurable rule");
        assert_eq!(rule["passed"], exit_code == 0);
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), content);
    }
}

#[test]
fn semantic_rule_result_errors_retain_exit_one() {
    for (kind, severity, diagnostic) in [
        ("not-a-substrate", "error", "unknown kind"),
        ("entity", "not-a-severity", "invalid severity"),
    ] {
        let tmp = fixture();
        let rules = tmp.path().join(".khive/kg/rules.toml");
        std::fs::write(
            &rules,
            format!(
                "[[rules]]\nid = \"invalid-rule\"\nkind = \"{kind}\"\nseverity = \"{severity}\"\n"
            ),
        )
        .unwrap();
        let output = validate(&tmp, None, false);
        assert_exit(&output, 1);
        let report = report(&output);
        assert_eq!(report["summary"]["errors"], 1);
        let rule = report["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule["id"] == "invalid-rule")
            .expect("invalid rule result");
        assert!(rule["violations"][0]["message"]
            .as_str()
            .unwrap()
            .contains(diagnostic));
    }
}

#[test]
fn postparse_direction_error_retains_exit_one() {
    let tmp = fixture();
    let rules = tmp.path().join(".khive/kg/rules.toml");
    std::fs::write(&rules, "[[edge_direction_conventions.relations]]\nrelation = \"not-a-relation\"\nforward_source_kinds = [\"concept\"]\nforward_target_kinds = [\"concept\"]\n").unwrap();
    let output = validate(&tmp, None, false);
    assert_exit(&output, 1);
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("validate rules TOML {}", rules.display())),
        "{stderr}"
    );
    assert!(stderr.contains("is not a valid edge relation"), "{stderr}");
}

#[test]
fn missing_default_and_no_rules_remain_successful() {
    let tmp = fixture();
    let output = validate(&tmp, None, false);
    assert_exit(&output, 0);
    assert_eq!(report(&output)["summary"]["passed"], true);

    let default_rules = tmp.path().join(".khive/kg/rules.toml");
    std::fs::write(&default_rules, "[").unwrap();
    let yaml_rules = tmp.path().join("rules.yaml");
    std::fs::write(&yaml_rules, "rules: []\n").unwrap();
    for rules in [None, Some(yaml_rules.as_path())] {
        let output = validate(&tmp, rules, true);
        assert_exit(&output, 0);
        assert_eq!(report(&output)["summary"]["passed"], true);
    }
    assert_eq!(std::fs::read_to_string(default_rules).unwrap(), "[");
    assert_eq!(std::fs::read_to_string(yaml_rules).unwrap(), "rules: []\n");
}
