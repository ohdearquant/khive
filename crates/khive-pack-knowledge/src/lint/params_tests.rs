use super::*;
use serde_json::json;

#[test]
fn strict_selection_defaults_and_duplicate_rule_ids() {
    let defaults = Selection::parse(json!({})).unwrap();
    assert_eq!(defaults.rules.len(), 13);
    assert_eq!(defaults.severity, LintSeverity::Warn);
    assert!(!defaults.fix && !defaults.stats_only);
    let selected = Selection::parse(
        json!({"rules":["orphan-entity","orphan-entity"],"severity":"info","fix":true}),
    )
    .unwrap();
    assert_eq!(selected.rules, BTreeSet::from([LintRuleId::OrphanEntity]));
    assert_eq!(selected.severity, LintSeverity::Info);
    assert!(selected.fix);
}

#[test]
fn nulls_wrong_types_unknown_ids_and_identity_overrides_are_refused() {
    for params in [
        Value::Null,
        json!([]),
        json!({"rules":null}),
        json!({"rules":[]}),
        json!({"rules":"orphan-entity"}),
        json!({"rules":[null]}),
        json!({"rules":[1]}),
        json!({"rules":["ORPHAN-ENTITY"]}),
        json!({"fix":null}),
        json!({"fix":1}),
        json!({"fix":"false"}),
        json!({"severity":null}),
        json!({"severity":"WARN"}),
        json!({"severity":1}),
        json!({"actor":"other"}),
        json!({"namespace":"other"}),
        json!({"typo":false}),
    ] {
        assert!(matches!(
            Selection::parse(params),
            Err(RuntimeError::InvalidInput(_))
        ));
    }
}

#[test]
fn stats_only_is_a_sole_selector_and_never_fixes() {
    for severity in ["info", "warn", "error"] {
        let selected =
            Selection::parse(json!({"rules":["stats-only"],"severity":severity})).unwrap();
        assert!(selected.stats_only && !selected.fix && selected.rules.is_empty());
    }
    for params in [
        json!({"rules":["stats-only","orphan-entity"]}),
        json!({"rules":["stats-only","stats-only"]}),
        json!({"rules":["stats-only"],"fix":true}),
    ] {
        assert!(Selection::parse(params).is_err());
    }
}
