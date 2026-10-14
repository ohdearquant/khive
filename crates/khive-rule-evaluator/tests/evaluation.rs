use std::error::Error;

use chrono::{DateTime, Utc};
use khive_rule_evaluator::{
    collect_edge_ids, collect_ids, edge_source_id, edge_target_id, evaluate, record_prefix,
    EvaluationContext, EvaluationMode, NdjsonState, RuleResult, Rules, RulesError,
};
use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind};
use serde_json::json;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-01-02T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn run(
    config: &str,
    entities: &str,
    edges: &str,
    notes: &str,
    mode: EvaluationMode,
    pack_edge_rules: &[EdgeEndpointRule],
) -> Vec<RuleResult> {
    evaluate(
        &Rules::parse_toml(config).unwrap(),
        NdjsonState {
            entities,
            edges,
            notes,
        },
        EvaluationContext {
            mode,
            pack_edge_rules,
            now: now(),
        },
    )
}

const ALL_CLASSES: &str = r#"
[[rules]]
id = "required"
kind = "entity"
require_field = "description"
message = "missing {id}"
[edge_endpoint_types]
[edge_direction_conventions]
[dangling_refs]
[naming_conventions]
[citation_date_lint]
severity = "info"
fields = ["year", "date"]
"#;
const GOLDEN_ENTITIES: &str = r#"{"id":"e","kind":"concept","name":"","properties":{"year":2099}}"#;
const GOLDEN_EDGES: &str =
    r#"{"edge_id":"x","source":"e","target":"outside","relation":"contains"}"#;
const GOLDEN_NOTES: &str = r#"{"id":"n","kind":"observation","properties":{"date":"2026-01-03"}}"#;

// Literal full result bytes are the oracle, including DTO field order, nulls,
// metadata, messages, severity and class order. Neither the oracle nor the
// native/WASI comparison derives expected findings from evaluator helpers.
const GOLDEN_FULL: &str = r#"[{"id":"required","severity":"warning","passed":false,"violations":[{"entity_id":"e","entity_name":"","entity_kind":"concept","rule_id":"required","severity":"warning","message":"missing e","fixable":false}]},{"id":"edge-endpoint-types","severity":"error","passed":true,"violations":[]},{"id":"edge-direction-conventions","severity":"warning","passed":true,"violations":[]},{"id":"dangling-refs","severity":"error","passed":false,"violations":[{"entity_id":"outside","entity_name":null,"entity_kind":null,"rule_id":"dangling-refs","severity":"error","message":"edge target outside not in dataset (validated offline within the NDJSON dataset only; no live-graph check available in this build)","fixable":false}]},{"id":"naming-conventions","severity":"warning","passed":false,"violations":[{"entity_id":"e","entity_name":"","entity_kind":"concept","rule_id":"naming-conventions","severity":"warning","message":"[e \"\"] name is empty or whitespace-only","fixable":false}]},{"id":"citation-date-lint","severity":"info","passed":false,"violations":[{"entity_id":"e","entity_name":"","entity_kind":"concept","rule_id":"citation-date-lint","severity":"info","message":"[e \"\"] property \"year\": year 2099 is after the current year 2026","fixable":false},{"entity_id":"n","entity_name":null,"entity_kind":"observation","rule_id":"citation-date-lint","severity":"info","message":"[n] property \"date\": date 2026-01-03 is in the future (validated at 2026-01-02 12:00:00 UTC)","fixable":false}]}]"#;
const GOLDEN_PARTIAL: &str = r#"[{"id":"required","severity":"warning","passed":false,"violations":[{"entity_id":"e","entity_name":"","entity_kind":"concept","rule_id":"required","severity":"warning","message":"missing e","fixable":false}]},{"id":"edge-endpoint-types","severity":"error","passed":true,"violations":[]},{"id":"edge-direction-conventions","severity":"warning","passed":true,"violations":[]},{"id":"naming-conventions","severity":"warning","passed":false,"violations":[{"entity_id":"e","entity_name":"","entity_kind":"concept","rule_id":"naming-conventions","severity":"warning","message":"[e \"\"] name is empty or whitespace-only","fixable":false}]},{"id":"citation-date-lint","severity":"info","passed":false,"violations":[{"entity_id":"e","entity_name":"","entity_kind":"concept","rule_id":"citation-date-lint","severity":"info","message":"[e \"\"] property \"year\": year 2099 is after the current year 2026","fixable":false},{"entity_id":"n","entity_name":null,"entity_kind":"observation","rule_id":"citation-date-lint","severity":"info","message":"[n] property \"date\": date 2026-01-03 is in the future (validated at 2026-01-02 12:00:00 UTC)","fixable":false}]}]"#;

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

#[test]
fn syntax_errors_retain_toml_sources_and_reject_unknown_nested_fields() {
    for text in [
        "not toml",
        "mystery = true",
        "[[rules]]\nid='x'\nkind='entity'\nsevertiy='error'",
        "[edge_endpoint_types]\nallow_all=true",
        "[edge_direction_conventions]\nunknown=true",
        "[[edge_direction_conventions.relations]]\nrelation='contains'\nforward_source_kind=['concept']",
        "[dangling_refs]\nunknown=true",
        "[naming_conventions.kinds.concept]\nmax_lenght=3",
        "[citation_date_lint]\nfield=['year']",
    ] {
        let error = Rules::parse_toml(text).unwrap_err();
        assert!(matches!(&error, RulesError::Syntax(_)), "{text}: {error}");
        assert!(error.source().is_some());
    }
}

#[test]
fn direction_validation_is_ordered_and_runs_even_when_disabled() {
    let entries = [
        ("not-a-relation", "[]", "[]", "edge_direction_conventions.relations[0]: \"not-a-relation\" is not a valid edge relation"),
        ("contains", "[]", "[]", "edge_direction_conventions.relations[0] (\"contains\"): forward_source_kinds must be non-empty"),
        ("contains", "['concept']", "[]", "edge_direction_conventions.relations[0] (\"contains\"): forward_target_kinds must be non-empty"),
    ];
    for (relation, source, target, expected) in entries {
        let text = format!("[edge_direction_conventions]\nenabled=false\n[[edge_direction_conventions.relations]]\nrelation='{relation}'\nforward_source_kinds={source}\nforward_target_kinds={target}");
        let error = Rules::parse_toml(&text).unwrap_err();
        assert!(matches!(&error, RulesError::Direction(_)));
        assert_eq!(error.to_string(), expected);
        assert!(error.source().is_none());
    }
}

#[test]
fn host_requirements_only_activate_enabled_valid_sections() {
    for text in [
        "",
        "[edge_endpoint_types]\nenabled=false\n[citation_date_lint]\nenabled=false",
        "[edge_endpoint_types]\nseverity='typo'\n[citation_date_lint]\nseverity='typo'",
    ] {
        let rules = Rules::parse_toml(text).unwrap();
        assert!(!rules.needs_pack_edge_rules());
        assert!(!rules.needs_current_time());
    }
    let endpoint = Rules::parse_toml("[edge_endpoint_types]").unwrap();
    assert!(endpoint.needs_pack_edge_rules());
    assert!(!endpoint.needs_current_time());
    let citation = Rules::parse_toml("[citation_date_lint]").unwrap();
    assert!(!citation.needs_pack_edge_rules());
    assert!(citation.needs_current_time());
    let disabled = "[edge_endpoint_types]\nenabled=false\n[edge_direction_conventions]\nenabled=false\n[dangling_refs]\nenabled=false\n[naming_conventions]\nenabled=false\n[citation_date_lint]\nenabled=false";
    assert!(run(
        disabled,
        GOLDEN_ENTITIES,
        GOLDEN_EDGES,
        GOLDEN_NOTES,
        EvaluationMode::FullDataset,
        &[]
    )
    .is_empty());
}

#[test]
fn severity_errors_precede_kind_and_all_builtin_results_keep_order() {
    let config = r#"
[[rules]]
id="bad-severity"
kind="note"
severity="invalid"
[[rules]]
id="bad-kind"
kind="note"
[edge_endpoint_types]
severity="invalid"
[edge_direction_conventions]
severity="invalid"
[dangling_refs]
severity="invalid"
[naming_conventions]
severity="invalid"
[citation_date_lint]
severity="invalid"
"#;
    let results = run(config, "", "", "", EvaluationMode::PartialView, &[]);
    assert_eq!(
        results.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        [
            "bad-severity",
            "bad-kind",
            "edge-endpoint-types",
            "edge-direction-conventions",
            "dangling-refs",
            "naming-conventions",
            "citation-date-lint"
        ]
    );
    assert!(results
        .iter()
        .all(|r| !r.passed && r.severity == "error" && r.violations.len() == 1));
    assert!(results[0].violations[0]
        .message
        .contains("invalid severity"));
    assert_eq!(
        results[1].violations[0].message,
        "Rule \"bad-kind\": unknown kind \"note\"; must be \"entity\" or \"edge\""
    );
}

#[test]
fn generic_conditions_required_field_types_and_self_loop_sentinel_are_preserved() {
    let config = r#"
[[rules]]
id="description"
kind="entity"
condition="kind=concept"
require_field="description"
message="description {id}"
[[rules]]
id="loop"
kind="edge"
condition="source_id=target_id"
require_field="ignored"
message="loop {id}"
"#;
    let entities = "invalid\n\n{\"id\":\"a\",\"kind\":\"concept\",\"description\":7}\n{\"id\":\"b\",\"kind\":\"concept\",\"description\":\" \"}\n{\"id\":\"c\",\"kind\":\"project\"}";
    let edges = r#"{"source":"a","target":"a","relation":"contains"}
{"source_id":"b","target_id":"b"}
{"source":"a","target":"b"}
{}
"#;
    let results = run(
        config,
        entities,
        edges,
        "",
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(results[0].violations.len(), 1);
    assert_eq!(results[0].violations[0].entity_id.as_deref(), Some("a"));
    assert_eq!(
        results[1]
            .violations
            .iter()
            .map(|v| v.message.as_str())
            .collect::<Vec<_>>(),
        ["loop a", "loop b", "loop "]
    );
    assert_eq!(
        results[1].violations[0].entity_kind.as_deref(),
        Some("contains")
    );
    assert!(results[1].violations[2]
        .entity_id
        .as_ref()
        .unwrap()
        .is_empty());
}

#[test]
fn canonical_fields_shadow_aliases_even_when_null_or_wrong_type() {
    assert_eq!(
        edge_source_id(&json!({"source":"first","source_id":"second"})),
        Some("first")
    );
    assert_eq!(edge_target_id(&json!({"target_id":"alias"})), Some("alias"));
    assert_eq!(
        edge_source_id(&json!({"source":null,"source_id":"alias"})),
        None
    );
    assert_eq!(
        edge_target_id(&json!({"target":9,"target_id":"alias"})),
        None
    );
    let input = r#"{"edge_id":"canonical","id":"other"}
{"id":"legacy"}
{"edge_id":null,"id":"hidden"}
{"edge_id":7,"id":"also-hidden"}
bad
"#;
    assert_eq!(
        collect_edge_ids(input),
        ["canonical".to_string(), "legacy".to_string()]
            .into_iter()
            .collect()
    );
    assert_eq!(
        collect_ids(input),
        ["other", "legacy", "hidden", "also-hidden"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    let results = run(
        "[dangling_refs]",
        "",
        r#"{"source":null,"source_id":"hidden","target":8,"target_id":"hidden"}"#,
        "",
        EvaluationMode::FullDataset,
        &[],
    );
    assert!(results[0].passed);
}

#[test]
fn endpoint_validation_uses_the_shared_table_and_supplied_subtype_rules() {
    let entities = r#"{"id":"c","kind":"concept"}
{"id":"p","kind":"project"}
{"id":"typed","kind":"document","entity_type":"paper"}
{"id":"plain","kind":"document"}
"#;
    let notes = r#"{"id":"n","kind":"observation"}"#;
    let edges = r#"{"source":"p","target":"c","relation":"implements"}
{"source":"c","target":"p","relation":"implements"}
{"source":"typed","target":"n","relation":"contains"}
{"source":"plain","target":"n","relation":"contains"}
{"source":"missing","target":"c","relation":"contains"}
{"source":"c","target":"p","relation":"not-a-relation"}
"#;
    let pack_rules = [EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfType {
            kind: "document",
            entity_type: "paper",
        },
        target: EndpointKind::NoteOfKind("observation"),
    }];
    let results = run(
        "[edge_endpoint_types]",
        entities,
        edges,
        notes,
        EvaluationMode::FullDataset,
        &pack_rules,
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("c"), Some("plain")]
    );
    assert_eq!(results[0].violations[0].message, "[c→p] (entity concept) -[implements]-> (entity project) is not a permitted endpoint pairing for this relation");
    let without = run(
        "[edge_endpoint_types]",
        entities,
        edges,
        notes,
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(without[0].violations.len(), 3);
}

#[test]
fn resolved_edge_endpoints_and_special_relations_retain_substrate_rules() {
    let entities = r#"{"id":"c","kind":"concept"}"#;
    let notes = "{\"id\":\"n\",\"kind\":\"observation\"}\n{\"id\":\"m\",\"kind\":\"decision\"}";
    let edges = r#"{"edge_id":"x","source":"n","target":"c","relation":"annotates"}
{"source":"n","target":"x","relation":"annotates"}
{"source":"c","target":"x","relation":"annotates"}
{"source":"n","target":"m","relation":"supports"}
{"source":"n","target":"c","relation":"supports"}
{"source":"x","target":"x","relation":"supersedes"}
{"source":"x","target":"c","relation":"contains"}
"#;
    let results = run(
        "[edge_endpoint_types]\n[dangling_refs]",
        entities,
        edges,
        notes,
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("c"), Some("n"), Some("x"), Some("x")]
    );
    assert!(
        results[1].passed,
        "edge IDs remain known dangling-reference targets"
    );
}

#[test]
fn duplicate_kind_map_prefers_last_note_then_preserves_it_over_edge_id() {
    let entities = "{\"id\":\"x\",\"kind\":\"concept\"}\n{\"id\":\"target\",\"kind\":\"concept\"}";
    let notes = "{\"id\":\"x\",\"kind\":\"observation\"}\n{\"id\":\"x\",\"kind\":\"decision\"}";
    let edges = r#"{"edge_id":"x","source":"x","target":"target","relation":"contains"}"#;
    let pack = [EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::NoteOfKind("decision"),
        target: EndpointKind::EntityOfKind("concept"),
    }];
    assert!(
        run(
            "[edge_endpoint_types]",
            entities,
            edges,
            notes,
            EvaluationMode::FullDataset,
            &pack
        )[0]
        .passed
    );
    let wrong = [EdgeEndpointRule {
        source: EndpointKind::NoteOfKind("observation"),
        ..pack[0]
    }];
    let results = run(
        "[edge_endpoint_types]",
        entities,
        edges,
        notes,
        EvaluationMode::FullDataset,
        &wrong,
    );
    assert_eq!(
        results[0].violations[0].entity_kind.as_deref(),
        Some("decision")
    );
}

#[test]
fn direction_forward_match_wins_and_reversed_alias_edges_report_in_input_order() {
    let config = r#"
[edge_direction_conventions]
[[edge_direction_conventions.relations]]
relation="implements"
forward_source_kinds=["project"]
forward_target_kinds=["concept"]
[[edge_direction_conventions.relations]]
relation="contains"
forward_source_kinds=["concept", "project"]
forward_target_kinds=["concept", "project"]
"#;
    let entities = "{\"id\":\"c\",\"kind\":\"concept\"}\n{\"id\":\"p\",\"kind\":\"project\"}";
    let edges = r#"{"source":"p","target":"c","relation":"implements"}
{"source_id":"c","target_id":"p","relation":"implements"}
{"source":"c","target":"p","relation":"contains"}
{"source":"absent","target":"p","relation":"implements"}
"#;
    let results = run(
        config,
        entities,
        edges,
        "",
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(results[0].violations.len(), 1);
    assert_eq!(results[0].violations[0].message, "[c→p] implements from concept to project matches the reversed direction convention configured for this relation; likely inverted");
}

#[test]
fn dangling_scan_preserves_edge_order_source_before_target_and_edge_id_aliases() {
    let edges = r#"{"edge_id":"x","source":"z","target":"a"}
{"id":"legacy","source_id":"x","target_id":"legacy"}
{"source":"b","target":"x"}
"#;
    let results = run(
        "[dangling_refs]",
        "",
        edges,
        "",
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("z"), Some("a"), Some("b")]
    );
    assert!(results[0].violations[0]
        .message
        .starts_with("edge source z"));
    assert!(results[0].violations[1]
        .message
        .starts_with("edge target a"));
}

#[test]
fn naming_overrides_count_characters_and_preserve_violation_order() {
    let config = r#"
[naming_conventions]
max_length=3
[naming_conventions.kinds.project]
max_length=100
no_parenthetical_suffix=false
no_leading_trailing_whitespace=false
"#;
    let entities = r#"{"id":"blank","kind":"concept","name":"    "}
{"id":"bad","kind":"concept","name":" X (q) "}
{"id":"unicode","kind":"concept","name":"猫猫猫"}
{"id":"override","kind":"project","name":" X (q) "}
{"id":"missing","kind":"concept"}
"#;
    let results = run(config, entities, "", "", EvaluationMode::FullDataset, &[]);
    let findings = &results[0].violations;
    assert_eq!(findings.len(), 4);
    assert_eq!(
        findings[0].message,
        "[blank \"    \"] name is empty or whitespace-only"
    );
    assert_eq!(
        findings[1].message,
        "[bad \" X (q) \"] name has leading/trailing whitespace"
    );
    assert_eq!(findings[2].message, "[bad \" X (q) \"] name carries a parenthetical suffix; use `properties` for qualifiers instead of embedding them in `name`");
    assert_eq!(
        findings[3].message,
        "[bad \" X (q) \"] name exceeds max length 3 (7 chars)"
    );
}

#[test]
fn citation_fixed_clock_handles_strict_boundaries_offsets_shapes_and_substrate_order() {
    let config = r#"[citation_date_lint]
fields=["past","equal_year","equal_date","equal_time","future_year","future_text","future_date","future_time","boolean","array","float","too_long","invalid"]"#;
    let entities = r#"{"id":"e","kind":"document","properties":{"past":2025,"equal_year":2026,"equal_date":"2026-01-02","equal_time":"2026-01-02T13:00:00+01:00","future_year":2027,"future_text":" 2028 ","future_date":"2026-01-03","future_time":"2026-01-02T12:00:00.000000001Z","boolean":true,"array":[2027],"float":2027.5,"too_long":10000,"invalid":"tomorrow"}}"#;
    let notes = r#"{"id":"n","kind":"reference","properties":{"future_year":"2029"}}"#;
    let results = run(
        config,
        entities,
        "",
        notes,
        EvaluationMode::FullDataset,
        &[],
    );
    let findings = &results[0].violations;
    assert_eq!(findings.iter().map(|v| v.message.as_str()).collect::<Vec<_>>(), [
        "[e] property \"future_year\": year 2027 is after the current year 2026",
        "[e] property \"future_text\": year 2028 is after the current year 2026",
        "[e] property \"future_date\": date 2026-01-03 is in the future (validated at 2026-01-02 12:00:00 UTC)",
        "[e] property \"future_time\": date 2026-01-02T12:00:00.000000001Z is in the future (validated at 2026-01-02 12:00:00 UTC)",
        "[n] property \"future_year\": year 2029 is after the current year 2026",
    ]);
    let rules = Rules::parse_toml(config).unwrap();
    let later = evaluate(
        &rules,
        NdjsonState {
            entities,
            edges: "",
            notes,
        },
        EvaluationContext {
            mode: EvaluationMode::FullDataset,
            pack_edge_rules: &[],
            now: DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        },
    );
    assert!(
        later[0].passed,
        "the supplied instant, not an ambient clock, controls every field"
    );
}

#[test]
fn empty_and_malformed_rows_remain_the_structural_pass_responsibility() {
    for input in ["", "\n\n", "malformed\nnull\n[]\n42\n{}"] {
        let results = run("[edge_endpoint_types]\n[edge_direction_conventions]\n[dangling_refs]\n[naming_conventions]\n[citation_date_lint]", input, input, input, EvaluationMode::FullDataset, &[]);
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(|r| r.passed && r.violations.is_empty()));
    }
}

#[test]
fn record_prefix_retains_debug_escaping_and_all_presence_cases() {
    assert_eq!(record_prefix(Some("id"), Some("a\nb")), "[id \"a\\nb\"] ");
    assert_eq!(record_prefix(Some("id"), None), "[id] ");
    assert_eq!(record_prefix(None, Some("name")), "[\"name\"] ");
    assert_eq!(record_prefix(None, None), "");
}
