// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Core versioning types: `SnapshotId`, `VcsState`.
//!
//! Legacy types (`KgSnapshot`, `KgBranch`, `RemoteConfig`) and the `VcsState.dirty`
//! flag were removed during the git-native v1 alignment pass. KG branches are now
//! git branches; there is no custom remote protocol.

use serde::{de, Deserialize, Deserializer, Serialize};

use crate::error::VcsError;

// ── SnapshotId ────────────────────────────────────────────────────────────────

/// Content-addressed snapshot identifier.
///
/// Invariant: always the string `"sha256:"` followed by exactly 64 lower-case
/// hex characters. Enforced by `SnapshotId::from_hash`. Custom `Deserialize`
/// validates the invariant via `from_prefixed`, rejecting malformed inputs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct SnapshotId(String);

impl<'de> Deserialize<'de> for SnapshotId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Require exact canonical form: "sha256:" prefix + 64 lower-case hex
        // chars with no whitespace.
        let hex = s.strip_prefix("sha256:").ok_or_else(|| {
            de::Error::custom(format!(
                "invalid SnapshotId: missing sha256: prefix in {:?}",
                s
            ))
        })?;
        if hex.len() != 64 {
            return Err(de::Error::custom(format!(
                "invalid SnapshotId: expected 64 hex chars, got {} in {:?}",
                hex.len(),
                s
            )));
        }
        if !hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
            return Err(de::Error::custom(format!(
                "invalid SnapshotId: hex must be lower-case with no whitespace in {:?}",
                s
            )));
        }
        Ok(SnapshotId(s))
    }
}

impl SnapshotId {
    /// Construct from a raw hex digest (without the `"sha256:"` prefix).
    ///
    /// Returns `Err(VcsError::InvalidSnapshotId)` if `hex` is not exactly 64
    /// lower-case hex characters.
    pub fn from_hash(hex: &str) -> Result<Self, VcsError> {
        let hex = hex.trim();
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(VcsError::InvalidSnapshotId(format!(
                "expected 64 hex chars, got {:?}",
                hex
            )));
        }
        Ok(Self(format!("sha256:{}", hex.to_ascii_lowercase())))
    }

    /// Construct from a full prefixed string (`"sha256:<hex64>"`).
    pub fn from_prefixed(s: &str) -> Result<Self, VcsError> {
        let hex = s.strip_prefix("sha256:").ok_or_else(|| {
            VcsError::InvalidSnapshotId(format!("missing sha256: prefix in {:?}", s))
        })?;
        Self::from_hash(hex)
    }

    /// Returns the full string including the `"sha256:"` prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns only the 64-character hex digest (without prefix).
    pub fn hex(&self) -> &str {
        &self.0["sha256:".len()..]
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── SnapshotCoverage ──────────────────────────────────────────────────────────

/// Records which record classes are covered by a KG snapshot.
///
/// v1 covers entities and edges only. Notes are excluded until note packs
/// define versioned export, import, privacy/redaction, and merge semantics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCoverage {
    pub entities: bool,
    pub edges: bool,
    pub notes: bool,
}

/// v1 coverage constant: entities + edges, notes excluded.
pub const KG_V1_COVERAGE: SnapshotCoverage = SnapshotCoverage {
    entities: true,
    edges: true,
    notes: false,
};

// ── VcsState ─────────────────────────────────────────────────────────────────

/// Per-namespace VCS state.
///
/// There is no `dirty` flag. The diff is computed fresh on every invocation via
/// `khive kg status` (DB vs NDJSON diff) to determine uncommitted changes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VcsState {
    pub namespace: String,
    /// Name of the currently active branch. `None` in detached HEAD state.
    pub current_branch: Option<String>,
    /// Last committed snapshot ID. `None` if no commit has been made.
    pub last_committed_id: Option<SnapshotId>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
}
