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

include!("evaluation_tests/golden_mode_tests.rs");

include!("evaluation_tests/configuration_tests.rs");

include!("evaluation_tests/generic_field_tests.rs");

include!("evaluation_tests/graph_rule_tests.rs");

include!("evaluation_tests/record_validation_tests.rs");
