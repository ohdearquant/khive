//! Check the hand-authored contract corpus before an extractor consumes it.

use std::collections::BTreeMap;

use serde_json::Value;

const SOURCE: &str = include_str!("fixtures/lean-mini/Mini.lean");
const TOOLCHAIN: &str = include_str!("fixtures/lean-mini/lean-toolchain");
const LAKEFILE: &str = include_str!("fixtures/lean-mini/lakefile.toml");
const MANIFEST: &str = include_str!("fixtures/lean-mini/fixture-manifest.json");
const SCHEMA: &str = include_str!("fixtures/lean-mini/expected-extraction.schema.json");
const EXPECTED: &str = include_str!("fixtures/lean-mini/expected-extraction.json");

#[test]
fn expected_extraction_matches_schema_and_source_declarations() {
    let manifest: Value = serde_json::from_str(MANIFEST).expect("fixture manifest JSON");
    let schema: Value = serde_json::from_str(SCHEMA).expect("fixture schema JSON");
    let expected: Value = serde_json::from_str(EXPECTED).expect("expected extraction JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON schema");
    let errors = validator
        .iter_errors(&expected)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema errors: {errors:#?}");

    assert_eq!(manifest["lean_toolchain"], TOOLCHAIN.trim());
    assert_eq!(manifest["lake_version"], "5.0.0");
    assert_eq!(manifest["module"], "Mini");
    assert!(LAKEFILE.contains("defaultTargets = [\"Mini\"]"));
    let lines = SOURCE.lines().collect::<Vec<_>>();
    let mut counts = BTreeMap::new();
    let keywords = [
        ("definition", "def "),
        ("structure", "structure "),
        ("instance", "instance "),
        ("axiom", "axiom "),
        ("theorem", "theorem "),
        ("goal", "example "),
    ];

    for entry in expected.as_array().expect("extraction array") {
        let kind = entry["kind"].as_str().expect("schema enforces kind string");
        *counts.entry(kind).or_insert(0_usize) += 1;
        let keyword = keywords
            .iter()
            .find_map(|(candidate, prefix)| (*candidate == kind).then_some(*prefix))
            .expect("schema restricts kind");
        let line = entry["source_location"]["line"]
            .as_u64()
            .expect("schema enforces line integer") as usize;
        let source_line = lines.get(line - 1).expect("source line exists");
        assert!(
            source_line.starts_with(keyword),
            "{kind} at line {line} does not start with {keyword:?}: {source_line:?}"
        );
        assert_eq!(
            entry["provenance"]["lean_toolchain"],
            manifest["lean_toolchain"]
        );
        assert_eq!(
            entry["provenance"]["lake_version"],
            manifest["lake_version"]
        );
    }

    assert_eq!(
        counts,
        BTreeMap::from([
            ("axiom", 1),
            ("definition", 1),
            ("goal", 1),
            ("instance", 1),
            ("structure", 1),
            ("theorem", 1),
        ])
    );

    let theorem = expected
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "theorem")
        .unwrap();
    assert_eq!(theorem["name"], "LeanMini.double_zero");
    assert_eq!(theorem["incomplete"], true);
    let theorem_line = theorem["source_location"]["line"].as_u64().unwrap() as usize;
    assert_eq!(lines[theorem_line].trim(), "sorry");

    let axiom = expected
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "axiom")
        .unwrap();
    assert_eq!(axiom["incomplete"], false);
    let goal = expected
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "goal")
        .unwrap();
    assert!(goal["name"].is_null(), "the example is anonymous");
    assert_eq!(goal["incomplete"], false);
}

#[test]
fn schema_rejects_unknown_subtype_and_missing_provenance() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();

    let mut unknown_kind = expected.clone();
    unknown_kind[0]["kind"] = Value::String("lemma".to_string());
    assert!(!validator.is_valid(&unknown_kind));

    let mut missing_provenance = expected;
    missing_provenance[0]
        .as_object_mut()
        .unwrap()
        .remove("provenance");
    assert!(!validator.is_valid(&missing_provenance));
}
