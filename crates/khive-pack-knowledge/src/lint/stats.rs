use std::collections::{BTreeMap, BTreeSet};

use khive_runtime::graph_lint::{InspectedLintEdge, LintStats, PropertyCoverageEntry};
use khive_storage::{Entity, Note};
use serde_json::Value;
use uuid::Uuid;

pub(super) fn degrees(entities: &[Entity], edges: &[InspectedLintEdge]) -> BTreeMap<Uuid, u64> {
    let mut counts: BTreeMap<_, _> = entities
        .iter()
        .filter(|entity| entity.deleted_at.is_none())
        .map(|entity| (entity.id, 0))
        .collect();
    for observed in edges
        .iter()
        .filter(|observed| observed.edge.deleted_at.is_none())
    {
        // A self-loop contributes one incident edge, not two.
        for id in BTreeSet::from([observed.edge.source_id, observed.edge.target_id]) {
            if let Some(count) = counts.get_mut(&id) {
                *count += 1;
            }
        }
    }
    counts
}

fn coverage<'a>(
    properties: impl Iterator<Item = &'a Option<Value>>,
    total: u64,
) -> BTreeMap<String, PropertyCoverageEntry> {
    let mut counts = BTreeMap::<String, PropertyCoverageEntry>::new();
    for properties in properties {
        if let Some(object) = properties.as_ref().and_then(Value::as_object) {
            for key in object.keys() {
                counts
                    .entry(key.clone())
                    .or_insert(PropertyCoverageEntry { present: 0, total })
                    .present += 1;
            }
        }
    }
    counts
}

pub(super) fn collect(
    entities: &[Entity],
    notes: &[Note],
    edges: &[InspectedLintEdge],
) -> LintStats {
    let counts = degrees(entities, edges);
    let entities: Vec<_> = entities
        .iter()
        .filter(|entity| entity.deleted_at.is_none())
        .collect();
    let notes: Vec<_> = notes
        .iter()
        .filter(|note| note.deleted_at.is_none())
        .collect();
    let mut result = LintStats {
        total_entities: entities.len() as u64,
        total_notes: notes.len() as u64,
        ..LintStats::default()
    };
    for entity in &entities {
        *result.entity_kinds.entry(entity.kind.clone()).or_default() += 1;
    }
    for note in &notes {
        *result.note_kinds.entry(note.kind.clone()).or_default() += 1;
    }
    for observed in edges
        .iter()
        .filter(|observed| observed.edge.deleted_at.is_none())
    {
        result.total_edges += 1;
        *result
            .edge_relations
            .entry(observed.edge.relation.to_string())
            .or_default() += 1;
    }
    let incidence: u64 = counts.values().sum();
    result.avg_density = if result.total_entities == 0 {
        0.0
    } else {
        incidence as f64 / result.total_entities as f64
    };
    for degree in counts.values() {
        *result
            .density_histogram
            .entry(degree.to_string())
            .or_default() += 1;
    }
    result.property_coverage.entities = coverage(
        entities.iter().map(|entity| &entity.properties),
        result.total_entities,
    );
    result.property_coverage.notes = coverage(
        notes.iter().map(|note| &note.properties),
        result.total_notes,
    );
    result
}
