use super::{
    build_kg_archive, publish_remote_cache, read_remote_snapshot_identity, snapshot_id_for_archive,
    MetaJson, RemoteName,
};
use crate::{SnapshotCoverage, SnapshotIdentity, KG_V1_COVERAGE};

#[test]
fn legacy_remote_metadata_preserves_recorded_identity_and_defaults_coverage() {
    let repo = tempfile::tempdir().unwrap();
    let name = RemoteName::parse("legacy").unwrap();
    let cache = repo.path().join(".khive/kg/remotes/legacy");
    std::fs::create_dir_all(&cache).unwrap();
    let legacy = br#"{
        "fetched_at": "2026-01-01T00:00:00Z",
        "git_ref": "main",
        "commit_sha": "0123456789abcdef0123456789abcdef01234567",
        "content_hash": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }"#;
    std::fs::write(cache.join("meta.json"), legacy).unwrap();

    let identity = read_remote_snapshot_identity(repo.path(), &name).unwrap();
    assert_eq!(
        identity.content_hash.as_str(),
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(identity.coverage, KG_V1_COVERAGE);
    assert_eq!(
        std::fs::read(cache.join("meta.json")).unwrap().as_slice(),
        legacy.as_slice()
    );
    assert!(!cache.join("entities.ndjson").exists());
    assert!(!cache.join("edges.ndjson").exists());
}

#[test]
fn published_equal_archives_retain_distinct_coverage_identity() {
    let repo = tempfile::tempdir().unwrap();
    let remotes_root = repo.path().join(".khive/kg/remotes");
    let archive = build_kg_archive("local", &[], &[]).unwrap();
    let content_hash = snapshot_id_for_archive(&archive).unwrap();
    let mut identities = Vec::new();
    let mut contents = Vec::new();

    for (name, coverage) in [
        ("notes-uncovered", KG_V1_COVERAGE),
        (
            "notes-covered-empty",
            SnapshotCoverage {
                entities: true,
                edges: true,
                notes: true,
            },
        ),
    ] {
        let name = RemoteName::parse(name).unwrap();
        let expected = SnapshotIdentity {
            content_hash: content_hash.clone(),
            coverage,
        };
        let meta = MetaJson {
            fetched_at: "2026-01-01T00:00:00Z".into(),
            git_ref: "main".into(),
            commit_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            identity: expected.clone(),
        };
        let cache =
            publish_remote_cache(&remotes_root, name.as_str(), &[], &[], &meta, None).unwrap();
        let identity = read_remote_snapshot_identity(repo.path(), &name).unwrap();
        assert_eq!(identity, expected);
        identities.push(identity);
        contents.push((
            std::fs::read(cache.join("entities.ndjson")).unwrap(),
            std::fs::read(cache.join("edges.ndjson")).unwrap(),
        ));
    }

    assert_eq!(contents[0], contents[1]);
    assert_eq!(contents[0], (Vec::<u8>::new(), Vec::<u8>::new()));
    assert_eq!(identities[0].content_hash, identities[1].content_hash);
    assert_ne!(identities[0], identities[1]);
    assert!(!identities[0].coverage.notes);
    assert!(identities[1].coverage.notes);
}

#[test]
fn remote_identity_reader_refuses_malformed_explicit_metadata() {
    let repo = tempfile::tempdir().unwrap();
    let name = RemoteName::parse("invalid").unwrap();
    let cache = repo.path().join(".khive/kg/remotes/invalid");
    std::fs::create_dir_all(&cache).unwrap();
    let hash = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    for metadata in [
        serde_json::json!({"content_hash": "sha256:invalid"}),
        serde_json::json!({"content_hash": hash, "coverage": null}),
        serde_json::json!({"content_hash": hash, "coverage": {"entities": true, "edges": true}}),
        serde_json::json!({"content_hash": hash, "coverage": {"entities": true, "edges": true, "notes": "false"}}),
    ] {
        std::fs::write(
            cache.join("meta.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        assert!(
            read_remote_snapshot_identity(repo.path(), &name).is_err(),
            "malformed explicit metadata must not become legacy coverage: {metadata}"
        );
    }
}
