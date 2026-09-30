use std::path::Path;
use std::time::Instant;

use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

use super::{
    build_kg_archive, write_sorted_edges, write_sorted_entities, NdjsonEdge, NdjsonEntity,
};
use crate::hash::snapshot_id_for_archive;

fn entity(id: u128, name: &str) -> NdjsonEntity {
    NdjsonEntity {
        id: Uuid::from_u128(id),
        kind: "concept".into(),
        entity_type: None,
        name: name.into(),
        description: None,
        properties: None,
        tags: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

fn edge(id: u128, source: u128, target: u128, relation: &str) -> NdjsonEdge {
    NdjsonEdge {
        edge_id: Uuid::from_u128(id),
        source: Uuid::from_u128(source),
        target: Uuid::from_u128(target),
        relation: relation.into(),
        weight: 1.0,
        properties: None,
        created_at: None,
        updated_at: None,
    }
}

fn legacy_entities(records: &[NdjsonEntity]) -> Vec<u8> {
    let mut sorted: Vec<_> = records.iter().collect();
    sorted.sort_by(|a, b| {
        a.id.to_string()
            .to_ascii_lowercase()
            .cmp(&b.id.to_string().to_ascii_lowercase())
    });
    sorted
        .into_iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

fn legacy_edges(records: &[NdjsonEdge]) -> Vec<u8> {
    let mut sorted: Vec<_> = records.iter().collect();
    sorted.sort_by(|a, b| {
        (
            a.source.to_string(),
            a.target.to_string(),
            a.relation.clone(),
        )
            .cmp(&(
                b.source.to_string(),
                b.target.to_string(),
                b.relation.clone(),
            ))
    });
    sorted
        .into_iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

fn assert_legacy_bytes(root: &Path, entities: &[NdjsonEntity], edges: &[NdjsonEdge]) {
    let entity_path = root.join("entities.ndjson");
    let edge_path = root.join("edges.ndjson");
    write_sorted_entities(&entity_path, entities).unwrap();
    write_sorted_edges(&edge_path, edges).unwrap();
    assert_eq!(
        std::fs::read(entity_path).unwrap(),
        legacy_entities(entities)
    );
    assert_eq!(std::fs::read(edge_path).unwrap(), legacy_edges(edges));
}

#[test]
fn streamed_ndjson_is_byte_identical_to_joined_legacy_output() {
    let temp = TempDir::new().unwrap();
    assert_legacy_bytes(temp.path(), &[], &[]);

    let mut entities = vec![entity(3, "third"), entity(1, "first\nquoted \"name\"")];
    entities[1].tags = vec!["z".into(), "a".into()];
    entities[1].properties = Some(json!({"nested":{"b":2,"a":1}}));
    let mut edges = vec![
        edge(11, 3, 1, "extends"),
        edge(12, 1, 3, "annotates"),
        edge(13, 1, 3, "extends"),
    ];
    edges[0].properties = Some(json!({"note":"α"}));
    assert_legacy_bytes(temp.path(), &entities, &edges);
}

/// Manual scale probe: `cargo test -p khive-vcs --lib remote_cache_scale_probe
/// -- --ignored --nocapture`. It records stage timings and output sizes without
/// imposing a machine-dependent timing threshold on CI.
#[test]
#[ignore]
fn remote_cache_scale_probe() {
    const ENTITY_COUNT: u128 = 100_000;
    const EDGE_COUNT: u128 = 300_000;
    let entities: Vec<_> = (1..=ENTITY_COUNT)
        .rev()
        .map(|index| entity(index, "synthetic entity"))
        .collect();
    let edges: Vec<_> = (1..=EDGE_COUNT)
        .rev()
        .map(|index| {
            edge(
                ENTITY_COUNT + index,
                index % ENTITY_COUNT + 1,
                (index + 1) % ENTITY_COUNT + 1,
                "extends",
            )
        })
        .collect();
    let temp = TempDir::new().unwrap();

    let started = Instant::now();
    write_sorted_entities(&temp.path().join("entities.ndjson"), &entities).unwrap();
    let entity_write = started.elapsed();
    let started = Instant::now();
    write_sorted_edges(&temp.path().join("edges.ndjson"), &edges).unwrap();
    let edge_write = started.elapsed();
    let archive = build_kg_archive("scale-probe", &entities, &edges).unwrap();
    let started = Instant::now();
    let hash = snapshot_id_for_archive(&archive).unwrap();
    let hash_time = started.elapsed();

    let entity_bytes = std::fs::metadata(temp.path().join("entities.ndjson"))
        .unwrap()
        .len();
    let edge_bytes = std::fs::metadata(temp.path().join("edges.ndjson"))
        .unwrap()
        .len();
    println!(
        "entities={ENTITY_COUNT} edges={EDGE_COUNT} entity_bytes={entity_bytes} edge_bytes={edge_bytes} entity_write={entity_write:?} edge_write={edge_write:?} hash={hash_time:?} snapshot={hash}"
    );
}
