#[test]
fn full_dataset_golden_transcript() {
    let results = run(
        ALL_CLASSES,
        GOLDEN_ENTITIES,
        GOLDEN_EDGES,
        GOLDEN_NOTES,
        EvaluationMode::FullDataset,
        &[],
    );
    let actual = serde_json::to_string(&results).unwrap();
    assert_eq!(actual, GOLDEN_FULL);
    println!("\nRULE_EVALUATOR_GOLDEN:full:{actual}");
}

#[test]
fn partial_view_golden_transcript() {
    let results = run(
        ALL_CLASSES,
        GOLDEN_ENTITIES,
        GOLDEN_EDGES,
        GOLDEN_NOTES,
        EvaluationMode::PartialView,
        &[],
    );
    let actual = serde_json::to_string(&results).unwrap();
    assert_eq!(actual, GOLDEN_PARTIAL);
    println!("\nRULE_EVALUATOR_GOLDEN:partial:{actual}");
}

#[test]
fn partial_view_preserves_generic_same_id_and_malformed_builtin_severity() {
    let config = r#"
[[rules]]
id = "dangling-refs"
kind = "entity"
severity = "error"
require_field = "name"
message = "name missing"
[dangling_refs]
severity = "warn"
"#;
    for mode in [EvaluationMode::FullDataset, EvaluationMode::PartialView] {
        let results = run(config, r#"{"id":"e"}"#, GOLDEN_EDGES, "", mode, &[]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].violations[0].message, "name missing");
        assert_eq!(results[1].violations[0].message, "Rule \"dangling-refs\": invalid severity \"warn\"; must be \"error\", \"warning\", or \"info\"");
        assert!(results.iter().all(|r| !r.passed && r.severity == "error"));
    }
    let valid = config.replace("severity = \"warn\"", "severity = \"error\"");
    let results = run(
        &valid,
        r#"{"id":"e"}"#,
        GOLDEN_EDGES,
        "",
        EvaluationMode::PartialView,
        &[],
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].violations[0].message, "name missing");
}
