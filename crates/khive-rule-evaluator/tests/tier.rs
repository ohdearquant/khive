use std::collections::BTreeMap;
use std::error::Error;

use khive_changeset::{ChangeSet, DeleteOp, DeletePreimage, Envelope, MergeOp, MergePreimage, Op};
use khive_rule_evaluator::{
    classify_changeset, Rules, RulesError, Tier, TierDecision, TierFinding,
};
use khive_types::{
    Entity, EntityKind, Header, Id128, Link, Namespace, Note, NoteStatus, Timestamp,
};
use serde_json::{json, Value};

const A: &str = "10000000-0000-4000-8000-000000000001";
const B: &str = "10000000-0000-4000-8000-000000000002";
const C: &str = "10000000-0000-4000-8000-000000000003";
const EDGE: &str = "10000000-0000-4000-8000-000000000004";

fn parse(value: Value) -> Op {
    serde_json::from_value(value).unwrap()
}

fn create(id: &str, note: bool) -> Op {
    let target = if note {
        json!({"kind":"note", "note_kind":"observation", "content":"Recorded note"})
    } else {
        json!({"kind":"entity", "entity_kind":"concept", "name":"Recorded entity"})
    };
    parse(json!({"op":"create", "id":id, "namespace":"local", "target":target}))
}

fn link(relation: &str, weight: f64) -> Op {
    parse(
        json!({"op":"link", "id":EDGE, "namespace":"local", "source":A,
        "target":B, "relation":relation, "weight":weight}),
    )
}

fn update(patch: Value, preimage: Value) -> Op {
    parse(json!({"op":"update", "target_id":A, "patch":patch, "preimage":preimage}))
}

fn entity(id: &str) -> Entity {
    Entity {
        header: Header::new(
            id.parse().unwrap(),
            Namespace::local(),
            Timestamp::from_secs(1),
        ),
        kind: EntityKind::Concept,
        entity_type: None,
        name: "Recorded entity".into(),
        description: None,
        properties: BTreeMap::new(),
        tags: Vec::new(),
        deleted_at: None,
    }
}

fn delete(substrate: &str) -> Op {
    let id: Id128 = A.parse().unwrap();
    let preimage = match substrate {
        "entity" => DeletePreimage::Entity(Box::new(entity(A))),
        "note" => DeletePreimage::Note(Box::new(Note {
            header: Header::new(id, Namespace::local(), Timestamp::from_secs(1)),
            kind: "observation".into(),
            status: NoteStatus::Active,
            content: "Recorded note".into(),
            properties: BTreeMap::new(),
            tags: Vec::new(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            deleted_at: None,
        })),
        "edge" => DeletePreimage::Edge(Box::new(Link {
            id,
            namespace: "local".into(),
            source: B.parse().unwrap(),
            target: C.parse().unwrap(),
            relation: "contains".parse().unwrap(),
            weight: 0.9,
            properties: BTreeMap::new(),
            created_at: Timestamp::from_secs(1),
            updated_at: Timestamp::from_secs(1),
            deleted_at: None,
        })),
        _ => panic!("invalid fixture substrate"),
    };
    Op::Delete(DeleteOp {
        target_id: id,
        hard: false,
        preimage,
    })
}

fn merge() -> Op {
    Op::Merge(MergeOp {
        into_id: A.parse().unwrap(),
        from_id: B.parse().unwrap(),
        preimage: MergePreimage {
            into: Box::new(entity(A)),
            from: Box::new(entity(B)),
            incident_edges: Vec::new(),
        },
    })
}

fn classify(config: &str, ops: Vec<Op>, findings: &[TierFinding<'_>]) -> Vec<TierDecision> {
    let changeset = ChangeSet::new(
        Envelope::new("agent:producer", "family:producer", Timestamp::from_secs(1)),
        ops,
    );
    // Exercise the existing strict codec as well as the typed classifier.
    let changeset =
        khive_changeset::from_ndjson(&khive_changeset::to_ndjson(&changeset).unwrap()).unwrap();
    classify_changeset(&Rules::parse_toml(config).unwrap(), &changeset, findings)
}

fn expected(tier: Tier, ids: &[&str], reason: &str) -> TierDecision {
    TierDecision {
        tier,
        reason: reason.into(),
        reason_ids: ids.iter().map(|id| (*id).into()).collect(),
    }
}

#[test]
fn absent_empty_and_explicit_seed_preserve_literal_default_reasons() {
    let ops = vec![
        create(A, false),
        create(B, true),
        link("contains", 0.7),
        link("supports", 0.69),
        update(
            json!({"target":"entity", "name":"After"}),
            json!({"target":"entity", "name":"Before"}),
        ),
        update(
            json!({"target":"note", "content":"After"}),
            json!({"target":"note", "content":"Before"}),
        ),
        update(
            json!({"target":"edge", "weight":0.69}),
            json!({"target":"edge", "weight":0.9}),
        ),
        delete("entity"),
        delete("note"),
        delete("edge"),
        merge(),
    ];
    let create_reason = expected(
        Tier::Tier1,
        &["adr102.create"],
        "Create is additive and Tier 1-eligible under the ADR-102 floor.",
    );
    let delete_reason = expected(
        Tier::Tier2,
        &["adr102.delete"],
        "Every delete requires Tier 2 review under the ADR-102 floor.",
    );
    let want = vec![
        create_reason, expected(Tier::Tier1, &["adr102.create"], "Create is additive and Tier 1-eligible under the ADR-102 floor."),
        expected(Tier::Tier1, &["adr102.non_judgment_link"], "This non-judgment link has weight at least 0.7 and is Tier 1-eligible."),
        expected(Tier::Tier2, &["adr102.judgment_relation", "adr102.resulting_weight_below_0_7"], "supports is judgment-bearing and requires Tier 2 review. The resulting weight 0.69 is below the ADR-102 floor of 0.7."),
        expected(Tier::Tier1, &["adr102.mutable_entity_update"], "A mutable entity-field update is Tier 1-eligible under the ADR-102 floor."),
        expected(Tier::Tier2, &["adr102.not_tier_1_eligible"], "Note updates are outside ADR-102's enumerated Tier 1 fast path."),
        expected(Tier::Tier2, &["adr102.existing_edge_update", "adr102.resulting_weight_below_0_7"], "Changing an existing edge relation or weight requires Tier 2 review. The resulting weight is below the ADR-102 floor of 0.7."),
        delete_reason, expected(Tier::Tier2, &["adr102.delete"], "Every delete requires Tier 2 review under the ADR-102 floor."),
        expected(Tier::Tier2, &["adr102.delete"], "Every delete requires Tier 2 review under the ADR-102 floor."),
        expected(Tier::Tier2, &["adr102.merge"], "Every merge requires Tier 2 review under the ADR-102 floor."),
    ];
    for config in [
        "",
        "[tier]",
        "[tier]\ntier_1_operations=['create','link','update']\nminimum_weight=0.7",
    ] {
        assert_eq!(classify(config, ops.clone(), &[]), want, "{config}");
    }
    assert_eq!(Tier::Tier1.as_str(), "tier_1");
    assert_eq!(Tier::Tier2.as_str(), "tier_2");
}

#[test]
fn configured_judgment_admission_cannot_lower_floor() {
    let config = "[tier]\ntier_1_relations=['supports','supersedes','refutes']\nminimum_weight=0.0";
    for relation in ["supports", "supersedes", "refutes"] {
        let result = classify(config, vec![link(relation, 0.9)], &[]);
        assert_eq!(result[0].tier, Tier::Tier2);
        assert_eq!(result[0].reason_ids, ["adr102.judgment_relation"]);
    }
}

#[test]
fn create_only_policy_changes_the_same_non_judgment_link() {
    let ops = vec![create(A, false), link("contains", 0.9)];
    assert_eq!(classify("", ops.clone(), &[])[1].tier, Tier::Tier1);
    let result = classify("[tier]\ntier_1_operations=['create']", ops, &[]);
    assert_eq!(result[0].tier, Tier::Tier1);
    assert_eq!(result[1].tier, Tier::Tier2);
    assert!(result[1]
        .reason_ids
        .iter()
        .any(|id| id == "config.operation_not_tier_1"));
}

#[test]
fn omitted_and_empty_lists_have_distinct_narrowing_semantics() {
    assert_eq!(
        classify("[tier]", vec![link("contains", 0.9)], &[])[0].tier,
        Tier::Tier1
    );
    for config in [
        "[tier]\ntier_1_operations=[]",
        "[tier]\ntier_1_relations=[]",
        "[tier]\ntier_1_relations=['extends']",
    ] {
        assert_eq!(
            classify(config, vec![link("contains", 0.9)], &[])[0].tier,
            Tier::Tier2
        );
    }
    let entity_update = update(
        json!({"target":"entity", "name":"After"}),
        json!({"target":"entity", "name":"Before"}),
    );
    assert_eq!(
        classify("[tier]", vec![entity_update.clone()], &[])[0].tier,
        Tier::Tier1
    );
    assert_eq!(
        classify("[tier]\ntier_1_entity_fields=[]", vec![entity_update], &[])[0].tier,
        Tier::Tier2
    );
}

#[test]
fn every_touched_mutable_field_and_nullable_clear_must_be_admitted() {
    for (field, before, after) in [
        ("name", json!("Before"), json!("After")),
        ("description", json!("Before"), Value::Null),
        ("properties", json!({"old":true}), json!({"new":true})),
        ("tags", json!(["old"]), json!(["new"])),
        ("entity_type", json!("old"), Value::Null),
    ] {
        let mut patch = json!({"target":"entity"});
        patch[field] = after;
        let mut preimage = json!({"target":"entity"});
        preimage[field] = before;
        let op = update(patch, preimage);
        let config = format!("[tier]\ntier_1_entity_fields=['{field}']");
        assert_eq!(
            classify(&config, vec![op.clone()], &[])[0].tier,
            Tier::Tier1,
            "{field}"
        );
        let result = classify("[tier]\ntier_1_entity_fields=[]", vec![op], &[]);
        assert_eq!(result[0].tier, Tier::Tier2, "{field}");
        assert!(result[0].reason.contains(field));
    }
    let op = update(
        json!({"target":"entity", "name":"After", "description":null}),
        json!({"target":"entity", "name":"Before", "description":"Before"}),
    );
    assert_eq!(
        classify("[tier]\ntier_1_entity_fields=['name']", vec![op], &[])[0].tier,
        Tier::Tier2
    );
}

#[test]
fn weight_boundary_floor_and_configured_higher_threshold_hold() {
    assert_eq!(
        classify("", vec![link("contains", 0.7)], &[])[0].tier,
        Tier::Tier1
    );
    let below = classify(
        "[tier]\nminimum_weight=0.0",
        vec![link("contains", 0.69)],
        &[],
    );
    assert_eq!(below[0].tier, Tier::Tier2);
    assert_eq!(below[0].reason_ids, ["adr102.resulting_weight_below_0_7"]);
    let narrowed = classify(
        "[tier]\nminimum_weight=0.9",
        vec![link("contains", 0.8)],
        &[],
    );
    assert_eq!(narrowed[0].tier, Tier::Tier2);
    assert!(narrowed[0]
        .reason_ids
        .iter()
        .any(|id| id == "config.resulting_weight_below_threshold"));
}

#[test]
fn destructive_and_existing_edge_updates_stay_tier_two_when_admitted() {
    let config =
        "[tier]\ntier_1_operations=['create','link','update','delete','merge']\nminimum_weight=0.0";
    let ops = vec![
        delete("entity"),
        delete("note"),
        delete("edge"),
        merge(),
        update(
            json!({"target":"edge", "relation":"extends"}),
            json!({"target":"edge", "relation":"contains"}),
        ),
        update(
            json!({"target":"edge", "weight":1.0}),
            json!({"target":"edge", "weight":0.9}),
        ),
        update(
            json!({"target":"note", "content":"After"}),
            json!({"target":"note", "content":"Before"}),
        ),
    ];
    assert!(classify(config, ops, &[])
        .iter()
        .all(|decision| decision.tier == Tier::Tier2));
}

#[test]
fn known_subjects_cover_notes_edges_and_both_merge_participants() {
    for (op, subject) in [
        (create(A, false), A),
        (create(A, true), A),
        (link("contains", 0.9), EDGE),
        (link("contains", 0.9), A),
        (link("contains", 0.9), B),
        (
            update(
                json!({"target":"entity", "name":"After"}),
                json!({"target":"entity", "name":"Before"}),
            ),
            A,
        ),
        (
            update(
                json!({"target":"note", "content":"After"}),
                json!({"target":"note", "content":"Before"}),
            ),
            A,
        ),
        (
            update(
                json!({"target":"edge", "weight":0.8}),
                json!({"target":"edge", "weight":0.9}),
            ),
            A,
        ),
        (delete("entity"), A),
        (delete("note"), A),
        (delete("edge"), A),
        (merge(), A),
        (merge(), B),
    ] {
        let findings = [TierFinding {
            severity: "error",
            subject_id: Some(subject),
        }];
        let result = classify("", vec![op, create(C, false)], &findings);
        assert_eq!(result[0].tier, Tier::Tier2);
        assert_eq!(
            result[0].reason_ids.last().unwrap(),
            "validation.error_severity_finding"
        );
        assert_eq!(
            result[1].tier,
            Tier::Tier1,
            "known subject {subject} became global"
        );
    }
}

#[test]
fn global_and_unmapped_errors_escalate_all_ops_but_warning_info_do_not() {
    let ops = vec![create(A, true), link("contains", 0.9)];
    for subject_id in [None, Some("unknown-record")] {
        let result = classify(
            "",
            ops.clone(),
            &[TierFinding {
                severity: "error",
                subject_id,
            }],
        );
        assert!(result.iter().all(|decision| decision.tier == Tier::Tier2));
    }
    for severity in ["warning", "info"] {
        assert!(classify(
            "",
            ops.clone(),
            &[TierFinding {
                severity,
                subject_id: None
            }]
        )
        .iter()
        .all(|decision| decision.tier == Tier::Tier1));
    }
    let result = classify(
        "",
        vec![create(A, false)],
        &[TierFinding {
            severity: "error",
            subject_id: Some(A),
        }],
    );
    assert_eq!(result[0].reason, "Create is additive and Tier 1-eligible under the ADR-102 floor. An error-severity validation finding escalates this operation to Tier 2.");
}

#[test]
fn coverage_error_is_a_real_floor_input_and_empty_changeset_is_empty() {
    let op = update(
        json!({"target":"entity", "name":"After"}),
        json!({"target":"entity", "name":"Before"}),
    );
    let result = classify(
        "",
        vec![op],
        &[TierFinding {
            severity: "error",
            subject_id: Some(A),
        }],
    );
    assert_eq!(result[0].tier, Tier::Tier2);
    assert_eq!(
        result[0].reason_ids,
        [
            "adr102.mutable_entity_update",
            "validation.error_severity_finding"
        ]
    );
    assert!(classify(
        "",
        vec![],
        &[TierFinding {
            severity: "error",
            subject_id: None
        }]
    )
    .is_empty());
}

#[test]
fn tier_syntax_errors_keep_toml_sources_and_semantic_errors_are_typed() {
    for input in [
        "[tier]\nunknown=true",
        "[tier]\ntier_1_operations='create'",
        "[tier]\nminimum_weight='high'",
        "[tier]\ntier_1_relations=[1]",
    ] {
        let error = Rules::parse_toml(input).unwrap_err();
        assert!(matches!(&error, RulesError::Syntax(_)), "{input}");
        assert!(error.source().is_some());
    }
    for entry in [
        "tier_1_operations=['restore']",
        "tier_1_relations=['typo']",
        "tier_1_entity_fields=['kind']",
        "tier_1_operations=['create','create']",
        "tier_1_relations=['contains','contains']",
        "tier_1_entity_fields=['name','name']",
        "minimum_weight=nan",
        "minimum_weight=inf",
        "minimum_weight=-inf",
        "minimum_weight=-0.01",
        "minimum_weight=1.01",
    ] {
        let error = Rules::parse_toml(&format!("[tier]\n{entry}")).unwrap_err();
        assert!(matches!(&error, RulesError::Tier(_)), "{entry}: {error}");
        assert!(error.source().is_none());
        assert!(error.to_string().starts_with("tier."));
    }
}
