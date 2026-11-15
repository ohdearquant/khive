use super::*;

#[test]
fn snapshot_id_from_hash_valid() {
    let hex = "a".repeat(64);
    let id = SnapshotId::from_hash(&hex).unwrap();
    assert_eq!(id.as_str(), format!("sha256:{}", hex));
    assert_eq!(id.hex(), hex);
}

#[test]
fn snapshot_id_from_hash_rejects_short() {
    let err = SnapshotId::from_hash("abc").unwrap_err();
    assert!(matches!(err, VcsError::InvalidSnapshotId(_)));
}

#[test]
fn snapshot_id_from_hash_rejects_non_hex() {
    let invalid = "z".repeat(64);
    let err = SnapshotId::from_hash(&invalid).unwrap_err();
    assert!(matches!(err, VcsError::InvalidSnapshotId(_)));
}

#[test]
fn snapshot_id_from_prefixed() {
    let hex = "b".repeat(64);
    let prefixed = format!("sha256:{}", hex);
    let id = SnapshotId::from_prefixed(&prefixed).unwrap();
    assert_eq!(id.as_str(), prefixed);
}

#[test]
fn snapshot_id_from_prefixed_rejects_missing_prefix() {
    let err = SnapshotId::from_prefixed(&"b".repeat(64)).unwrap_err();
    assert!(matches!(err, VcsError::InvalidSnapshotId(_)));
}

#[test]
fn snapshot_id_from_hash_accepts_uppercase_and_normalizes() {
    let upper = "A".repeat(64);
    let id = SnapshotId::from_hash(&upper).unwrap();
    assert_eq!(id.hex(), "a".repeat(64));
    assert!(id.as_str().starts_with("sha256:"));
}

#[test]
fn snapshot_id_from_hash_trims_whitespace() {
    let hex = "b".repeat(64);
    let padded = format!("  {hex}  ");
    let id = SnapshotId::from_hash(&padded).unwrap();
    assert_eq!(id.hex(), hex);
}

#[test]
fn snapshot_id_display_equals_as_str() {
    let hex = "c".repeat(64);
    let id = SnapshotId::from_hash(&hex).unwrap();
    assert_eq!(id.to_string(), id.as_str());
}

#[test]
fn snapshot_id_serde_roundtrip() {
    let hex = "d".repeat(64);
    let id = SnapshotId::from_hash(&hex).unwrap();
    let json = serde_json::to_string(&id).unwrap();
    let back: SnapshotId = serde_json::from_str(&json).unwrap();
    assert_eq!(back, id);
}

#[test]
fn vcs_state_serde_roundtrip() {
    let state = VcsState {
        namespace: "proj".into(),
        current_branch: Some("main".into()),
        last_committed_id: Some(SnapshotId::from_hash(&"0".repeat(64)).unwrap()),
    };
    let json = serde_json::to_string(&state).unwrap();
    let back: VcsState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.namespace, state.namespace);
    assert_eq!(back.current_branch, Some("main".into()));
    assert_eq!(back.last_committed_id, state.last_committed_id);
}

#[test]
fn snapshot_coverage_v1_entities_and_edges_only() {
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

// VCS-AUD-004: custom Deserialize must reject non-canonical inputs.

#[test]
fn snapshot_id_serde_rejects_missing_prefix() {
    let raw = format!("\"{}\"", "a".repeat(64));
    let err = serde_json::from_str::<SnapshotId>(&raw);
    assert!(err.is_err(), "must reject bare hex without sha256: prefix");
}

#[test]
fn snapshot_id_serde_rejects_uppercase_hex() {
    let raw = format!("\"sha256:{}\"", "A".repeat(64));
    let err = serde_json::from_str::<SnapshotId>(&raw);
    assert!(err.is_err(), "must reject uppercase hex in prefixed form");
}

#[test]
fn snapshot_id_serde_rejects_whitespace() {
    let raw = format!("\"sha256: {}\"", "a".repeat(64));
    let err = serde_json::from_str::<SnapshotId>(&raw);
    assert!(err.is_err(), "must reject whitespace inside hex portion");
}

#[test]
fn snapshot_id_serde_rejects_wrong_length() {
    let raw = "\"sha256:abc\"";
    let err = serde_json::from_str::<SnapshotId>(raw);
    assert!(err.is_err(), "must reject hex shorter than 64 chars");
}

#[test]
fn snapshot_id_serde_accepts_valid_prefixed() {
    let hex = "a".repeat(64);
    let raw = format!("\"sha256:{hex}\"");
    let id: SnapshotId = serde_json::from_str(&raw).unwrap();
    assert_eq!(id.hex(), hex);
}

#[test]
fn snapshot_id_serde_uppercase_hex_error_message_is_exact() {
    let prefixed = format!("sha256:{}", "A".repeat(64));
    let error = serde_json::from_value::<SnapshotId>(serde_json::Value::String(prefixed))
        .expect_err("uppercase prefixed hex must be rejected");

    assert_eq!(
        error.to_string(),
        concat!(
            "invalid SnapshotId: hex must be lower-case with no whitespace in \"sha256:",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"",
        ),
    );
}
