//! Integration tests for `khive-vcs` (issue #88).
//!
//! The original #88 issue requested integration tests for the snapshot,
//! branch, log, and merge subsystems. Those subsystems were superseded by
//! ADR-048 (git-native KG versioning via the Deno CLI). What remains in
//! this crate is the foundational VCS surface: content-addressed snapshot
//! identifiers and canonical archive hashing.
//!
//! These tests exercise the public API end-to-end ACROSS modules — proving
//! the surface composes correctly, not just that individual files compile.
//! Unit tests inside `src/{hash,types}.rs` test each module in isolation;
//! this file tests the composition.

use chrono::Utc;
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_storage::EdgeRelation;
use khive_vcs::hash::{canonical_json, snapshot_id_for_archive};
use khive_vcs::types::{KgBranch, KgSnapshot, RemoteAuth, RemoteConfig, SnapshotId, VcsState};
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
        name: name.into(),
        description: None,
        properties: None,
        tags: Vec::new(),
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn snapshot_id_roundtrips_through_archive_hash_into_kgsnapshot() {
    // The full chain: build archive -> compute SnapshotId -> wrap in KgSnapshot
    // -> serialize via serde -> deserialize -> verify id is recoverable.
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

    let snapshot = KgSnapshot {
        id: id.clone(),
        namespace: "test-ns".into(),
        parent_id: None,
        message: "genesis".into(),
        author: Some("test".into()),
        created_at: 0,
        entity_count: archive.entities.len() as u64,
        edge_count: archive.edges.len() as u64,
    };

    let json = serde_json::to_string(&snapshot).expect("serialize");
    let back: KgSnapshot = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.id, id, "id round-trips through serde");
    assert_eq!(back.entity_count, 1);
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
fn kg_branch_holds_snapshot_id_serde_roundtrip() {
    let archive = make_archive("ns");
    let head_id = snapshot_id_for_archive(&archive).unwrap();
    let branch = KgBranch {
        namespace: "ns".into(),
        name: "main".into(),
        head_id: head_id.clone(),
        created_at: 0,
        updated_at: 0,
    };
    let json = serde_json::to_string(&branch).unwrap();
    let back: KgBranch = serde_json::from_str(&json).unwrap();
    assert_eq!(back.head_id, head_id);
    assert_eq!(back.name, "main");
}

#[test]
fn remote_config_redacts_bearer_token_in_debug() {
    let cfg = RemoteConfig {
        name: "origin".into(),
        url: "https://khive.example.com".into(),
        auth: RemoteAuth::Bearer {
            token: "super_secret_token".into(),
        },
        namespace_map: None,
    };
    let dbg = format!("{:?}", cfg.auth);
    assert!(
        dbg.contains("REDACTED"),
        "Bearer debug must REDACT the token; got: {dbg}"
    );
    assert!(
        !dbg.contains("super_secret_token"),
        "secret must not leak through Debug; got: {dbg}"
    );
}

#[test]
fn remote_config_namespace_mapping_works() {
    let cfg = RemoteConfig {
        name: "origin".into(),
        url: "https://khive.example.com".into(),
        auth: RemoteAuth::None,
        namespace_map: Some(("local_ns".into(), "remote_ns".into())),
    };
    assert_eq!(cfg.remote_namespace("local_ns"), "remote_ns");
    assert_eq!(cfg.remote_namespace("other"), "other");
}

#[test]
fn vcs_state_can_be_serialized_and_carries_snapshot_id() {
    let archive = make_archive("ns");
    let id = snapshot_id_for_archive(&archive).unwrap();
    let state = VcsState {
        namespace: "ns".into(),
        current_branch: Some("main".into()),
        last_committed_id: Some(id.clone()),
        dirty: false,
    };
    let json = serde_json::to_string(&state).unwrap();
    let back: VcsState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.last_committed_id, Some(id));
    assert_eq!(back.current_branch.as_deref(), Some("main"));
    assert!(!back.dirty);
}
