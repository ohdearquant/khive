//! Rules-file acceptance at the real read-only CLI boundary.
use super::*;

const ALPHA: &str = "50000000-0000-4000-8000-000000000001";
const BETA: &str = "50000000-0000-4000-8000-000000000002";
const EDGE: &str = "50000000-0000-4000-8000-000000000003";

fn case(relation: &str, weight: f64) -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().expect("tier case");
    let (changeset, rules) = write_case(
        temp.path(),
        "family:bob",
        &[
            create(ALPHA, "Alpha"),
            create(BETA, "Beta"),
            link(EDGE, ALPHA, BETA, relation, weight),
        ],
    );
    (temp, changeset, rules)
}

fn configured_review(changeset: &Path, rules: &Path, config: &str) -> serde_json::Value {
    std::fs::write(rules, config).unwrap();
    let before_changeset = std::fs::read(changeset).unwrap();
    let before_rules = std::fs::read(rules).unwrap();
    let before_entries = directory_entries(changeset.parent().unwrap());
    let output = run_review(changeset, rules, Some("family:independent"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(changeset).unwrap(), before_changeset);
    assert_eq!(std::fs::read(rules).unwrap(), before_rules);
    assert_eq!(
        directory_entries(changeset.parent().unwrap()),
        before_entries
    );
    parse_stdout(&output)
}

#[test]
fn explicitly_admitted_supports_still_requires_tier_2() {
    let (_temp, changeset, rules) = case("supports", 0.9);
    let report = configured_review(&changeset, &rules,
        "[tier]\ntier_1_operations = ['create', 'link']\ntier_1_relations = ['supports']\nminimum_weight = 0.0\n");
    let op = &report["change_set"]["operations"][2];
    assert_eq!(op["tier"], "tier_2");
    assert!(op["tier_reasons"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("adr102.judgment_relation")));
    assert_eq!(report["tier_summary"]["tier_1"], 2);
    assert_eq!(report["tier_summary"]["tier_2"], 1);
    assert_eq!(report["review_gate"]["required"], true);
    assert_eq!(report["validation"]["passed"], true);
}

#[test]
fn create_only_configuration_changes_the_same_non_judgment_link() {
    let (_temp, changeset, rules) = case("extends", 0.9);
    let default = configured_review(&changeset, &rules, "");
    let narrow = configured_review(
        &changeset,
        &rules,
        "[tier]\ntier_1_operations = ['create']\n",
    );
    assert_eq!(default["change_set"]["operations"][2]["tier"], "tier_1");
    assert_eq!(default["review_gate"]["required"], false);
    let op = &narrow["change_set"]["operations"][2];
    assert_eq!(op["tier"], "tier_2");
    assert!(op["tier_reasons"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("config.operation_not_tier_1")));
    assert_eq!(narrow["tier_summary"]["tier_1"], 2);
    assert_eq!(narrow["tier_summary"]["tier_2"], 1);
    assert_eq!(narrow["review_gate"]["required"], true);
    assert_eq!(default["findings"], narrow["findings"]);
    assert_eq!(default["validation"], narrow["validation"]);
}

#[test]
fn explicit_seed_matches_omitted_configuration() {
    let (_temp, changeset, rules) = case("supports", 0.9);
    let omitted = configured_review(&changeset, &rules, "");
    let explicit = configured_review(
        &changeset,
        &rules,
        "[tier]\ntier_1_operations = ['create', 'link', 'update']\nminimum_weight = 0.7\n",
    );
    assert_eq!(omitted, explicit);
}

#[test]
fn relation_and_weight_configuration_only_narrow_eligibility() {
    let (_temp, changeset, rules) = case("extends", 0.8);
    let default = configured_review(&changeset, &rules, "");
    assert_eq!(default["change_set"]["operations"][2]["tier"], "tier_1");
    for (config, reason) in [
        (
            "[tier]\ntier_1_relations = []\n",
            "config.relation_not_tier_1",
        ),
        (
            "[tier]\nminimum_weight = 0.9\n",
            "config.resulting_weight_below_threshold",
        ),
    ] {
        let report = configured_review(&changeset, &rules, config);
        let op = &report["change_set"]["operations"][2];
        assert_eq!(op["tier"], "tier_2");
        assert!(op["tier_reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(reason)));
        assert_eq!(report["findings"], default["findings"]);
        assert_eq!(report["validation"], default["validation"]);
    }
}

#[test]
fn configured_entity_update_keeps_the_real_coverage_blocker() {
    let temp = TempDir::new().unwrap();
    let (changeset, rules) = write_case(
        temp.path(),
        "family:bob",
        &[entity_update(ALPHA, "Old", "New")],
    );
    std::fs::write(
        &rules,
        "[tier]\ntier_1_operations = ['update']\ntier_1_entity_fields = ['name']\n",
    )
    .unwrap();
    let output = run_review(&changeset, &rules, Some("family:independent"));
    assert!(!output.status.success());
    let report = parse_stdout(&output);
    assert_eq!(report["findings"][0]["rule_id"], "review-rule-coverage");
    assert_eq!(report["change_set"]["operations"][0]["tier"], "tier_2");
    assert!(report["change_set"]["operations"][0]["tier_reasons"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("validation.error_severity_finding")));
    assert_eq!(report["validation"]["passed"], false);
    assert_eq!(report["review_gate"]["status"], "blocked_by_findings");
}

#[test]
fn warning_findings_remain_visible_without_forcing_tier_2() {
    let (_temp, changeset, rules) = case("extends", 0.9);
    let report = configured_review(&changeset, &rules,
        "[tier]\ntier_1_operations = ['create', 'link']\n\n[[rules]]\nid = 'description-warning'\nseverity = 'warning'\nkind = 'entity'\ncondition = 'kind=concept'\nrequire_field = 'description'\nmessage = 'Description is absent'\n");
    assert!(report["validation"]["warnings"].as_u64().unwrap() > 0);
    assert_eq!(report["validation"]["passed"], true);
    assert_eq!(report["tier_summary"]["tier_2"], 0);
    assert!(!report["findings"].as_array().unwrap().is_empty());
}

#[test]
fn invalid_tier_configuration_is_refused_before_a_report_or_input_write() {
    let (_temp, changeset, rules) = case("extends", 0.9);
    for config in [
        "[tier]\nunknown = true\n",
        "[tier]\ntier_1_operations = ['invented']\n",
        "[tier]\ntier_1_operations = ['create', 'create']\n",
        "[tier]\ntier_1_relations = ['invented']\n",
        "[tier]\ntier_1_entity_fields = ['content']\n",
        "[tier]\nminimum_weight = 1.1\n",
        "[tier]\nminimum_weight = nan\n",
    ] {
        std::fs::write(&rules, config).unwrap();
        let before_changeset = std::fs::read(&changeset).unwrap();
        let before_rules = std::fs::read(&rules).unwrap();
        let before_entries = directory_entries(changeset.parent().unwrap());
        let output = run_review(&changeset, &rules, Some("family:independent"));
        assert!(
            !output.status.success(),
            "unexpected acceptance of {config}"
        );
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        assert_eq!(std::fs::read(&changeset).unwrap(), before_changeset);
        assert_eq!(std::fs::read(&rules).unwrap(), before_rules);
        assert_eq!(
            directory_entries(changeset.parent().unwrap()),
            before_entries
        );
    }
}
