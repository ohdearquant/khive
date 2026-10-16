use std::path::PathBuf;

use khive_types::EndpointKind;
use tempfile::TempDir;

use super::*;

// ── Taxonomy helpers ──────────────────────────────────────────────────────

/// Build the real pack-registry taxonomy. Tests that need it call this once.
fn real_taxonomy() -> KgTaxonomy {
    build_taxonomy().expect("build_taxonomy must succeed in test environment")
}

/// Minimal entity-kind set covering the 8 base kinds + `resource` (ADR-048).
fn base_entity_kinds() -> HashSet<String> {
    [
        "concept", "document", "dataset", "project", "person", "org", "artifact", "service",
        "resource",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Minimal note-kind set covering base KG kinds + pack additions.
fn base_note_kinds() -> HashSet<String> {
    [
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
        "task",
        "memory",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn make_kg_dir(tmp: &TempDir) -> PathBuf {
    let kg_dir = tmp.path().join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).unwrap();
    kg_dir
}

fn write_entities(kg_dir: &std::path::Path, entities: &[(&str, &str, &str)]) {
    let content: String = entities
        .iter()
        .map(|(id, kind, name)| format!(r#"{{"id":"{id}","kind":"{kind}","name":"{name}"}}"#))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("entities.ndjson"), content + "\n").unwrap();
}

fn write_edges(kg_dir: &std::path::Path, edges: &[(&str, &str, &str)]) {
    let content: String = edges
        .iter()
        .map(|(src, tgt, rel)| {
            format!(r#"{{"source_id":"{src}","target_id":"{tgt}","relation":"{rel}"}}"#)
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("edges.ndjson"), content + "\n").unwrap();
}

/// #1225: the canonical wire spelling every real NDJSON writer emits
/// (`khive-vcs::sync::NdjsonEdge`, `kkernel::kg::archive::NdjsonEdge`, and
/// the runtime portability `ExportedEdge`) is `source`/`target`, not the
/// `source_id`/`target_id` [`write_edges`] uses. This helper writes the
/// spelling real producers actually emit, so round-trip tests exercise
/// what `kg validate` is actually validating in production.
fn write_edges_canonical(kg_dir: &std::path::Path, edges: &[(&str, &str, &str)]) {
    let content: String = edges
        .iter()
        .map(|(src, tgt, rel)| {
            format!(r#"{{"source":"{src}","target":"{tgt}","relation":"{rel}"}}"#)
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("edges.ndjson"), content + "\n").unwrap();
}

fn write_notes(kg_dir: &std::path::Path, notes: &[(&str, &str)]) {
    let content: String = notes
        .iter()
        .map(|(id, kind)| format!(r#"{{"id":"{id}","kind":"{kind}"}}"#))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("notes.ndjson"), content + "\n").unwrap();
}

include!("validate_tests/schema_input_tests.rs");

include!("validate_tests/record_reference_tests.rs");

include!("validate_tests/generic_rule_tests.rs");

include!("validate_tests/kind_registry_tests.rs");

include!("validate_tests/rendering_and_relation_tests.rs");

include!("validate_tests/endpoint_type_tests.rs");

include!("validate_tests/direction_convention_tests.rs");

include!("validate_tests/dangling_naming_and_citation_tests.rs");

include!("validate_tests/rule_configuration_tests.rs");
