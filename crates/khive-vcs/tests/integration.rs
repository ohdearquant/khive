//! Integration tests for `khive-vcs`.
//!
//! These tests exercise the public API end-to-end ACROSS modules — proving
//! the surface composes correctly, not just that individual files compile.
//! Unit tests inside `src/{hash,types}.rs` test each module in isolation;
//! this file tests the composition.
//!
//! Legacy types (`KgSnapshot`, `KgBranch`, `RemoteConfig`) and the `VcsState.dirty`
//! flag were removed in the ADR-010/ADR-020 alignment pass. Tests that relied on
//! those types have been replaced with tests for `SnapshotCoverage` and the
//! git-native `VcsState`.
//!
//! The final section tests that `khive-vcs-adapters::JsonFormatAdapter` can parse
//! a JSON substrate snapshot and hand the records to the hash pipeline (making
//! `khive-vcs-adapters` a non-orphan crate).

use chrono::Utc;
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_storage::EdgeRelation;
use khive_vcs::hash::{canonical_json, snapshot_id_for_archive};
use khive_vcs::types::{SnapshotCoverage, SnapshotId, VcsState, KG_V1_COVERAGE};
use khive_vcs_adapters::{FormatAdapter, JsonFormatAdapter};
use uuid::Uuid;

fn make_archive(namespace: &str) -> KgArchive {
    KgArchive {
        format: "kg-archive".into(),
        version: "0.2".into(),
        namespace: namespace.into(),
        exported_at: Utc::now(),
        entities: Vec::new(),
        edges: Vec::new(),
    }
}

fn make_entity(id: Uuid, name: &str) -> ExportedEntity {
    let now = Utc::now();
    ExportedEntity {
        id,
        kind: "concept".into(),
        entity_type: None,
        name: name.into(),
        description: None,
        properties: None,
        tags: Vec::new(),
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn snapshot_id_roundtrips_through_archive_hash() {
    // The full chain: build archive -> compute SnapshotId -> serialize via
    // serde -> deserialize -> verify id is recoverable.
    let mut archive = make_archive("test-ns");
    archive
        .entities
        .push(make_entity(Uuid::new_v4(), "FlashAttention"));

    let id = snapshot_id_for_archive(&archive).expect("hashing succeeds");
    assert!(
        id.as_str().starts_with("sha256:"),
        "id must carry sha256: prefix"
    );
    assert_eq!(id.hex().len(), 64, "hex digest is 64 chars");

    let json = serde_json::to_string(&id).expect("serialize");
    let back: SnapshotId = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, id, "id round-trips through serde");
}

#[test]
fn snapshot_id_is_deterministic_across_multiple_archives_with_same_content() {
    let id1 = make_entity("11111111-1111-1111-1111-111111111111".parse().unwrap(), "A");
    let id2 = make_entity("22222222-2222-2222-2222-222222222222".parse().unwrap(), "B");

    // Two archives with identical content but different exported_at must hash
    // to the same SnapshotId — exported_at is intentionally not in the hash.
    let mut a1 = make_archive("ns");
    a1.entities.push(id1.clone());
    a1.entities.push(id2.clone());

    let mut a2 = make_archive("ns");
    a2.exported_at = a1.exported_at + chrono::Duration::hours(1);
    a2.entities.push(id2);
    a2.entities.push(id1); // reverse order — canonicalization sorts

    let h1 = snapshot_id_for_archive(&a1).unwrap();
    let h2 = snapshot_id_for_archive(&a2).unwrap();
    assert_eq!(
        h1, h2,
        "two archives with same content (different order, different exported_at) must hash identically"
    );
}

#[test]
fn canonical_json_matches_for_equivalent_archives() {
    let e1 = make_entity("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".parse().unwrap(), "X");
    let mut a = make_archive("ns");
    let mut b = make_archive("ns");
    b.exported_at = a.exported_at + chrono::Duration::seconds(1);
    a.entities.push(e1.clone());
    b.entities.push(e1);

    let ja = canonical_json(&a).unwrap();
    let jb = canonical_json(&b).unwrap();
    assert_eq!(
        ja, jb,
        "canonical JSON must be byte-identical for equivalent archives"
    );
}

#[test]
fn snapshot_id_changes_when_edge_added() {
    let mut archive = make_archive("ns");
    let e1 = make_entity("11111111-1111-1111-1111-111111111111".parse().unwrap(), "A");
    let e2 = make_entity("22222222-2222-2222-2222-222222222222".parse().unwrap(), "B");
    archive.entities.push(e1.clone());
    archive.entities.push(e2.clone());

    let h_before = snapshot_id_for_archive(&archive).unwrap();

    archive.edges.push(ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: e1.id,
        target: e2.id,
        relation: EdgeRelation::Extends,
        weight: 1.0,
    });

    let h_after = snapshot_id_for_archive(&archive).unwrap();
    assert_ne!(
        h_before, h_after,
        "adding an edge must change the SnapshotId"
    );
}

#[test]
fn snapshot_id_from_prefixed_roundtrip() {
    let archive = make_archive("ns");
    let original = snapshot_id_for_archive(&archive).unwrap();

    let parsed = SnapshotId::from_prefixed(original.as_str()).expect("re-parse own output");
    assert_eq!(original, parsed);
    assert_eq!(parsed.hex().len(), 64);
}

#[test]
fn vcs_state_serde_roundtrip_without_dirty_flag() {
    let archive = make_archive("ns");
    let id = snapshot_id_for_archive(&archive).unwrap();
    let state = VcsState {
        namespace: "ns".into(),
        current_branch: Some("main".into()),
        last_committed_id: Some(id.clone()),
    };
    let json = serde_json::to_string(&state).unwrap();
    let back: VcsState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.last_committed_id, Some(id));
    assert_eq!(back.current_branch.as_deref(), Some("main"));
}

#[test]
fn snapshot_coverage_v1_covers_entities_and_edges_not_notes() {
    const { assert!(KG_V1_COVERAGE.entities) };
    const { assert!(KG_V1_COVERAGE.edges) };
    const { assert!(!KG_V1_COVERAGE.notes) };
}

#[test]
fn snapshot_coverage_serde_roundtrip() {
    let cov = KG_V1_COVERAGE.clone();
    let json = serde_json::to_string(&cov).unwrap();
    let back: SnapshotCoverage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, cov);
}

// ---------------------------------------------------------------------------
// Consumer test: JsonFormatAdapter → khive-vcs hash pipeline
//
// This test is the designated consumer that makes `khive-vcs-adapters` a
// non-orphan crate. It proves that records produced by the adapter can be
// fed into the vcs hash pipeline end-to-end, which is the actual use-case
// described in ADR-036 §1 (adapter → intermediate NDJSON → standard import).
// ---------------------------------------------------------------------------

#[test]
fn json_adapter_records_survive_entity_hash_pipeline() {
    // A minimal JSON substrate snapshot with one entity
    let json_input = r#"[
        {
            "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "kind": "concept",
            "name": "RoPE",
            "description": "Rotary Position Embedding"
        }
    ]"#;

    let mut adapter = JsonFormatAdapter::new(json_input).expect("valid fixture must construct");

    let entities: Vec<_> = adapter
        .entities()
        .map(|r| r.expect("no errors in fixture"))
        .collect();

    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].name, "RoPE");
    assert_eq!(entities[0].kind, "concept");
    assert_eq!(
        entities[0].id.to_string(),
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    );

    // Feed the parsed entity into a KgArchive and hash it — the full vcs pipeline.
    let now = Utc::now();
    let archive = KgArchive {
        format: "kg-archive".into(),
        version: "0.2".into(),
        namespace: "test".into(),
        exported_at: now,
        entities: vec![ExportedEntity {
            id: entities[0].id,
            kind: entities[0].kind.clone(),
            entity_type: None,
            name: entities[0].name.clone(),
            description: entities[0].description.clone(),
            properties: None,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
        }],
        edges: Vec::new(),
    };

    let snapshot_id =
        snapshot_id_for_archive(&archive).expect("hashing a single-entity archive must succeed");

    assert!(
        snapshot_id.as_str().starts_with("sha256:"),
        "snapshot id from adapter-sourced entity must carry sha256: prefix"
    );
    assert_eq!(snapshot_id.hex().len(), 64);

    // Determinism check: re-hash the same archive — must match.
    let snapshot_id2 = snapshot_id_for_archive(&archive).unwrap();
    assert_eq!(
        snapshot_id, snapshot_id2,
        "repeated hashing of the same archive must be deterministic"
    );
}
