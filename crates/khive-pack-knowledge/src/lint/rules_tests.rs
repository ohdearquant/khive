use super::*;
use chrono::{TimeZone, Utc};
use khive_storage::{Edge, EdgeRelation};
use serde_json::json;
use std::collections::BTreeSet;

fn entity(id: u128, kind: &str, name: &str) -> Entity {
    let mut entity = Entity::new("local", kind, name);
    entity.id = Uuid::from_u128(id);
    entity
}

fn edge(id: u128, source: u128, target: u128, relation: &str, weight: f64) -> InspectedLintEdge {
    let at = Utc.timestamp_opt(1, 0).unwrap();
    InspectedLintEdge {
        edge: Edge {
            id: Uuid::from_u128(id).into(),
            namespace: "local".into(),
            source_id: Uuid::from_u128(source),
            target_id: Uuid::from_u128(target),
            relation: relation.parse::<EdgeRelation>().unwrap(),
            weight,
            created_at: at,
            updated_at: at,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        },
        source_live: true,
        target_live: true,
        source_soft_deleted: false,
        target_soft_deleted: false,
        contract_valid: Some(true),
    }
}

fn all() -> Selection {
    Selection::parse(json!({"severity":"info"})).unwrap()
}

#[test]
fn all_thirteen_rules_have_typed_subjects_and_full_pre_fix_statistics() {
    let mut paper = entity(1, "concept", "paper");
    paper.properties = Some(json!({"type":"paper","authors":null}));
    let mut project = entity(2, "project", "project");
    project.properties = Some(json!({"entity_type":"library"}));
    let mut old = entity(3, "concept", "old");
    old.properties = Some(json!({"status":false}));
    let replacement = entity(4, "concept", "replacement");
    let duplicate_a = entity(5, "document", "same");
    let duplicate_b = entity(6, "document", "same");
    let mut tombstone = entity(7, "concept", "deleted");
    tombstone.deleted_at = Some(1);
    let entities = vec![
        paper,
        project,
        old,
        replacement,
        duplicate_b,
        duplicate_a,
        tombstone,
    ];
    let note = Note::new("local", "observation", "observation");
    let valid = edge(101, 4, 3, "supersedes", 0.8);
    let mut invalid = edge(102, 2, 7, "implements", 0.2);
    invalid.target_live = false;
    invalid.target_soft_deleted = true;
    invalid.contract_valid = Some(false);
    let edges = vec![valid, invalid];
    let report = evaluate(&entities, &[note], &edges, &all());
    assert_eq!(
        report
            .summary
            .by_rule
            .keys()
            .copied()
            .collect::<BTreeSet<_>>(),
        Rule::ALL.into_iter().collect()
    );
    assert_eq!(
        report.summary.total,
        report.summary.errors + report.summary.warnings + report.summary.infos
    );
    assert_eq!(
        report.summary.total,
        report.summary.by_rule.values().sum::<u64>()
    );
    assert!(report.summary.fixed.is_none());
    assert_eq!(report.stats.total_entities, 6);
    assert_eq!(report.stats.total_notes, 1);
    assert_eq!(report.stats.total_edges, 2);
    assert_eq!(report.stats.avg_density, 0.5);
    assert_eq!(
        report.stats.density_histogram,
        BTreeMap::from([("0".into(), 3), ("1".into(), 3)])
    );
    assert_eq!(
        report.stats.property_coverage.entities["authors"].present,
        1
    );
    assert_eq!(report.stats.property_coverage.entities["authors"].total, 6);
    assert!(!report.stats.property_coverage.entities.contains_key("name"));
    let duplicate = report
        .violations
        .iter()
        .find(|row| row.rule == Rule::DuplicateName)
        .unwrap();
    assert_eq!(duplicate.subject.id(), Uuid::from_u128(5));
    assert_eq!(
        duplicate.member_ids,
        Some(vec![Uuid::from_u128(5), Uuid::from_u128(6)])
    );
    for row in &report.violations {
        let wire = serde_json::to_value(row).unwrap();
        assert!(wire.get("subject").is_none());
        let id_fields = ["entity_id", "edge_id", "note_id"]
            .into_iter()
            .filter(|key| wire.get(*key).is_some())
            .count();
        assert_eq!(id_fields, 1);
        assert_eq!(row.member_ids.is_some(), row.rule == Rule::DuplicateName);
        assert_eq!(
            row.auto_fixable,
            matches!(
                row.rule,
                Rule::DanglingEdge | Rule::DuplicateName | Rule::SupersededNotMarked
            )
        );
    }
    let filtered =
        Selection::parse(json!({"rules":["orphan-entity"],"fix":true,"severity":"error"})).unwrap();
    let selected = evaluate(&entities, &[], &edges, &filtered);
    assert_eq!(selected.stats.total_entities, report.stats.total_entities);
    assert_eq!(selected.stats.total_edges, report.stats.total_edges);
    assert_eq!(selected.stats.avg_density, report.stats.avg_density);
    assert_eq!(selected.summary.fixed, Some(0));
    assert!(selected
        .violations
        .iter()
        .all(|row| row.rule == Rule::OrphanEntity));
}

#[test]
fn raw_degree_and_density_keep_self_loops_and_dangling_live_endpoints() {
    let live = entity(1, "concept", "live");
    let mut deleted = entity(2, "concept", "deleted");
    deleted.deleted_at = Some(1);
    let mut self_loop = edge(101, 1, 1, "extends", 0.3);
    self_loop.contract_valid = Some(false);
    let mut dangling = edge(102, 1, 2, "extends", 0.3);
    dangling.target_live = false;
    dangling.target_soft_deleted = true;
    let mut missing = edge(103, 1, 99, "extends", 0.3);
    missing.target_live = false;
    missing.contract_valid = None;
    let mut ignored = edge(104, 1, 2, "extends", 0.1);
    ignored.edge.deleted_at = Some(Utc.timestamp_opt(1, 0).unwrap());
    let report = evaluate(
        &[live, deleted],
        &[],
        &[self_loop, dangling, missing, ignored],
        &all(),
    );
    assert_eq!(report.stats.total_edges, 3);
    assert_eq!(report.stats.avg_density, 3.0);
    assert_eq!(
        report.stats.density_histogram,
        BTreeMap::from([("3".into(), 1)])
    );
    assert_eq!(report.summary.by_rule[&Rule::DanglingEdge], 1);
    assert_eq!(report.summary.by_rule[&Rule::DirectionViolation], 1);
    assert!(!report.summary.by_rule.contains_key(&Rule::LowWeightEdge));
    assert_eq!(report.summary.by_rule[&Rule::ConceptNoParent], 1);
}

#[test]
fn only_live_valid_semantic_witnesses_satisfy_relation_rules() {
    let mut paper = entity(1, "concept", "paper");
    paper.properties = Some(json!({"type":"paper","authors":null,"year":null,"source":null}));
    let parent = entity(2, "concept", "parent");
    let introduced = edge(101, 1, 2, "introduced_by", 0.8);
    let extends = edge(102, 1, 2, "extends", 0.8);
    let report = evaluate(
        &[paper.clone(), parent.clone()],
        &[],
        &[introduced.clone(), extends.clone()],
        &all(),
    );
    assert!(!report
        .summary
        .by_rule
        .contains_key(&Rule::MissingIntroducedBy));
    assert!(!report
        .summary
        .by_rule
        .contains_key(&Rule::PaperMissingMetadata));
    assert!(!report
        .violations
        .iter()
        .any(|row| row.subject.id() == paper.id && row.rule == Rule::ConceptNoParent));
    for mode in 0..3 {
        let mut invalid = introduced.clone();
        let mut invalid_parent = extends.clone();
        for observed in [&mut invalid, &mut invalid_parent] {
            match mode {
                0 => observed.target_live = false,
                1 => observed.contract_valid = Some(false),
                _ => observed.edge.deleted_at = Some(Utc.timestamp_opt(1, 0).unwrap()),
            }
        }
        let report = evaluate(
            &[paper.clone(), parent.clone()],
            &[],
            &[invalid, invalid_parent],
            &all(),
        );
        assert_eq!(report.summary.by_rule[&Rule::MissingIntroducedBy], 1);
        assert!(report
            .violations
            .iter()
            .any(|row| row.subject.id() == paper.id && row.rule == Rule::ConceptNoParent));
    }
}

#[test]
fn canonical_type_exact_names_and_legacy_paper_boundaries_are_preserved() {
    let mut project = entity(1, "project", "same");
    project.entity_type = Some(" ".into());
    let mut resource = entity(2, "resource", "same");
    resource.entity_type = Some("".into());
    let mut document = entity(3, "document", "same");
    document.properties = Some(json!({"type":"paper"}));
    let case = entity(4, "document", "Same");
    let mut other_namespace = entity(5, "document", "same");
    other_namespace.namespace = "other".into();
    let report = evaluate(
        &[project, resource, document, case, other_namespace],
        &[],
        &[],
        &all(),
    );
    assert_eq!(report.summary.by_rule[&Rule::MissingEntityType], 1);
    assert!(!report.summary.by_rule.contains_key(&Rule::DuplicateName));
    assert!(!report
        .summary
        .by_rule
        .contains_key(&Rule::MissingIntroducedBy));
    assert!(!report
        .summary
        .by_rule
        .contains_key(&Rule::PaperMissingMetadata));
}

#[test]
fn annotates_can_witness_an_event_and_property_presence_counts_null_only_once() {
    let mut note = Note::new("local", "observation", "body");
    note.id = Uuid::from_u128(1);
    note.properties = Some(json!({"null":null,"nested":{"inside":1}}));
    let event_annotation = edge(101, 1, 99, "annotates", 0.9);
    let report = evaluate(&[], &[note.clone()], &[event_annotation], &all());
    assert!(!report.summary.by_rule.contains_key(&Rule::NoteNoAnnotates));
    assert_eq!(report.stats.total_notes, 1);
    assert_eq!(report.stats.avg_density, 0.0);
    assert!(report.stats.density_histogram.is_empty());
    assert_eq!(report.stats.property_coverage.notes.len(), 2);
    assert_eq!(report.stats.property_coverage.notes["null"].present, 1);
    assert!(!report.stats.property_coverage.notes.contains_key("inside"));
    let empty = evaluate(&[], &[], &[], &all());
    assert_eq!(empty.stats, khive_runtime::graph_lint::LintStats::default());
    assert_eq!(empty.summary.total, 0);
}

#[test]
fn report_order_and_severity_floor_do_not_depend_on_capture_iteration_order() {
    let mut entities = vec![
        entity(3, "document", "same"),
        entity(1, "concept", "paper"),
        entity(2, "document", "same"),
    ];
    let report = evaluate(&entities, &[], &[], &all());
    entities.reverse();
    let reordered = evaluate(&entities, &[], &[], &all());
    assert_eq!(report, reordered);
    let warn = evaluate(&entities, &[], &[], &Selection::parse(json!({})).unwrap());
    let errors = evaluate(
        &entities,
        &[],
        &[],
        &Selection::parse(json!({"severity":"error"})).unwrap(),
    );
    assert_eq!(warn.stats, errors.stats);
    assert!(warn
        .violations
        .iter()
        .all(|row| row.severity >= Severity::Warn));
    assert!(errors
        .violations
        .iter()
        .all(|row| row.severity == Severity::Error));
}
