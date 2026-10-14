use std::collections::{BTreeMap, BTreeSet};

use khive_runtime::graph_lint::{
    InspectedLintEdge, LintReport, LintRuleId as Rule, LintSeverity as Severity, LintSubject,
    LintSummary, LintViolation,
};
use khive_storage::{Entity, Note};
use serde_json::Value;
use uuid::Uuid;

use super::{params::Selection, stats};

fn entity_subject(entity: &Entity) -> LintSubject {
    LintSubject::Entity {
        namespace: entity.namespace.clone(),
        entity_id: entity.id,
        entity_name: entity.name.clone(),
        entity_kind: entity.kind.clone(),
    }
}

fn violation(
    subject: LintSubject,
    rule: Rule,
    severity: Severity,
    message: impl Into<String>,
    suggestion: &str,
) -> LintViolation {
    LintViolation {
        subject,
        rule,
        severity,
        message: message.into(),
        suggestion: suggestion.into(),
        auto_fixable: matches!(
            rule,
            Rule::DanglingEdge | Rule::DuplicateName | Rule::SupersededNotMarked
        ),
        member_ids: None,
    }
}

fn property_string<'a>(entity: &'a Entity, key: &str) -> Option<&'a str> {
    entity.properties.as_ref()?.as_object()?.get(key)?.as_str()
}

pub(super) fn evaluate(
    entities: &[Entity],
    notes: &[Note],
    edges: &[InspectedLintEdge],
    selection: &Selection,
) -> LintReport {
    let live: BTreeMap<Uuid, &Entity> = entities
        .iter()
        .filter(|entity| entity.deleted_at.is_none())
        .map(|entity| (entity.id, entity))
        .collect();
    let degrees = stats::degrees(entities, edges);
    let witnesses = edges.iter().filter(|observed| {
        observed.edge.deleted_at.is_none()
            && observed.source_live
            && observed.target_live
            && observed.contract_valid == Some(true)
    });
    let mut outgoing_relations = BTreeMap::<Uuid, BTreeSet<String>>::new();
    let mut implements_concept = BTreeSet::new();
    let mut superseded = BTreeSet::new();
    for observed in witnesses {
        let relation = observed.edge.relation.to_string();
        if relation == "implements"
            && live
                .get(&observed.edge.target_id)
                .is_some_and(|target| target.kind == "concept")
        {
            implements_concept.insert(observed.edge.source_id);
        }
        if relation == "supersedes" {
            superseded.insert(observed.edge.target_id);
        }
        outgoing_relations
            .entry(observed.edge.source_id)
            .or_default()
            .insert(relation);
    }
    let outgoing = |id: Uuid, relation: &str| {
        outgoing_relations
            .get(&id)
            .is_some_and(|relations| relations.contains(relation))
    };
    let mut violations = Vec::new();
    let mut groups = BTreeMap::<(&str, &str, &str), Vec<&Entity>>::new();
    for entity in live.values() {
        let subject = entity_subject(entity);
        let degree = degrees[&entity.id];
        if degree == 0 {
            violations.push(violation(
                subject.clone(),
                Rule::OrphanEntity,
                Severity::Error,
                "Entity has no visible incident edges",
                "Connect the entity to related graph records",
            ));
        }
        let minimum = match entity.kind.as_str() {
            "concept" => 4,
            "project" => 3,
            "person" => 1,
            _ => 0,
        };
        if degree < minimum {
            violations.push(violation(
                subject.clone(),
                Rule::MinEdgeDensity,
                Severity::Error,
                format!(
                    "Entity has degree {degree}, below the {minimum} minimum for {}",
                    entity.kind
                ),
                "Add meaningful relationships after checking the entity's context",
            ));
        }
        if matches!(entity.kind.as_str(), "project" | "resource")
            && entity.entity_type.as_deref().is_none_or(str::is_empty)
        {
            let legacy = entity
                .properties
                .as_ref()
                .and_then(Value::as_object)
                .is_some_and(|object| object.contains_key("entity_type"));
            let message = if legacy {
                "Canonical entity_type is absent or empty; legacy properties.entity_type does not satisfy the rule"
            } else {
                "Canonical entity_type is absent or empty"
            };
            violations.push(violation(
                subject.clone(),
                Rule::MissingEntityType,
                Severity::Warn,
                message,
                "Set the canonical entity_type column to an allowed subtype",
            ));
        }
        let paper = entity.kind == "concept" && property_string(entity, "type") == Some("paper");
        if paper && !outgoing(entity.id, "introduced_by") {
            violations.push(violation(
                subject.clone(),
                Rule::MissingIntroducedBy,
                Severity::Warn,
                "Concept paper lacks a live valid outgoing introduced_by witness",
                "Link the paper concept to its introducer",
            ));
        }
        if entity.kind == "project" && !implements_concept.contains(&entity.id) {
            violations.push(violation(
                subject.clone(),
                Rule::MissingImplements,
                Severity::Warn,
                "Project lacks a live valid implements edge to a concept",
                "Link the project to a concept it implements",
            ));
        }
        if superseded.contains(&entity.id)
            && property_string(entity, "status") != Some("deprecated")
        {
            violations.push(violation(
                subject.clone(),
                Rule::SupersededNotMarked,
                Severity::Warn,
                "Live entity is superseded but is not marked deprecated",
                "Mark the observed entity deprecated through a guarded property update",
            ));
        }
        if entity.kind == "concept"
            && !["instance_of", "extends", "variant_of"]
                .iter()
                .any(|relation| outgoing(entity.id, relation))
        {
            violations.push(violation(
                subject.clone(),
                Rule::ConceptNoParent,
                Severity::Warn,
                "Concept lacks a live valid outgoing parent witness",
                "Link the concept to its parent with instance_of, extends, or variant_of",
            ));
        }
        if paper {
            let object = entity.properties.as_ref().and_then(Value::as_object);
            let missing: Vec<_> = ["authors", "year", "source"]
                .into_iter()
                .filter(|key| !object.is_some_and(|object| object.contains_key(*key)))
                .collect();
            if !missing.is_empty() {
                violations.push(violation(
                    subject,
                    Rule::PaperMissingMetadata,
                    Severity::Info,
                    format!(
                        "Concept paper lacks property members: {}",
                        missing.join(", ")
                    ),
                    "Add the missing paper metadata members",
                ));
            }
        }
        groups
            .entry((&entity.namespace, &entity.kind, &entity.name))
            .or_default()
            .push(entity);
    }
    for members in groups.values_mut().filter(|members| members.len() > 1) {
        members.sort_by_key(|entity| entity.id);
        let mut row = violation(entity_subject(members[0]), Rule::DuplicateName, Severity::Warn, format!("{} live entities share this exact namespace, kind and name", members.len()), "Create a question about the observed duplicates; do not choose a merge winner automatically");
        row.member_ids = Some(members.iter().map(|entity| entity.id).collect());
        violations.push(row);
    }
    for observed in edges
        .iter()
        .filter(|observed| observed.edge.deleted_at.is_none())
    {
        let subject = LintSubject::Edge {
            namespace: observed.edge.namespace.clone(),
            edge_id: observed.edge.id.0,
        };
        if observed.source_soft_deleted || observed.target_soft_deleted {
            violations.push(violation(subject.clone(), Rule::DanglingEdge, Severity::Error, "Edge has a confirmed soft-deleted endpoint", "Soft-delete the unchanged edge only while a local observed endpoint remains tombstoned"));
        }
        if observed.contract_valid == Some(false) {
            violations.push(violation(
                subject.clone(),
                Rule::DirectionViolation,
                Severity::Error,
                "Resolved endpoint types violate the shared link contract",
                "Repair the endpoint modeling under the current base and pack rules",
            ));
        }
        if observed.edge.weight < 0.3 {
            violations.push(violation(
                subject,
                Rule::LowWeightEdge,
                Severity::Info,
                "Edge weight is below 0.3",
                "Review the relationship's strength and evidence",
            ));
        }
    }
    for note in notes.iter().filter(|note| note.deleted_at.is_none()) {
        if !outgoing(note.id, "annotates") {
            violations.push(violation(
                LintSubject::Note {
                    namespace: note.namespace.clone(),
                    note_id: note.id,
                    note_kind: note.kind.clone(),
                },
                Rule::NoteNoAnnotates,
                Severity::Info,
                "Note lacks a live valid outgoing annotates witness",
                "Annotate a visible live entity, note, edge or event allowed by the link contract",
            ));
        }
    }
    violations
        .retain(|row| selection.rules.contains(&row.rule) && row.severity >= selection.severity);
    violations.sort_by(|left, right| {
        (
            left.rule.as_str(),
            left.subject.namespace(),
            left.subject.substrate(),
            left.subject.id(),
        )
            .cmp(&(
                right.rule.as_str(),
                right.subject.namespace(),
                right.subject.substrate(),
                right.subject.id(),
            ))
    });
    let mut summary = LintSummary {
        total: violations.len() as u64,
        fixed: selection.fix.then_some(0),
        ..LintSummary::default()
    };
    for row in &violations {
        *summary.by_rule.entry(row.rule).or_default() += 1;
        match row.severity {
            Severity::Error => summary.errors += 1,
            Severity::Warn => summary.warnings += 1,
            Severity::Info => summary.infos += 1,
        }
    }
    LintReport {
        violations,
        summary,
        stats: stats::collect(entities, notes, edges),
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
