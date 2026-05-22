// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Core versioning types: `SnapshotId`, `KgSnapshot`, `KgBranch`, `RemoteConfig`.

use serde::{Deserialize, Serialize};

use crate::error::VcsError;

// ── SnapshotId ────────────────────────────────────────────────────────────────

/// Content-addressed snapshot identifier.
///
/// Invariant: always the string `"sha256:"` followed by exactly 64 lower-case
/// hex characters. Enforced by `SnapshotId::from_hash`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotId(String);

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

// ── KgSnapshot ────────────────────────────────────────────────────────────────

/// Immutable point-in-time capture of a namespace's entity and edge set.
///
/// `id` is the SHA-256 hash of the deterministically serialized archive.
/// The archive itself is stored separately in `kg_snapshot_archives`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KgSnapshot {
    /// Content hash — also the primary key in `kg_snapshots`.
    pub id: SnapshotId,
    /// Namespace this snapshot belongs to.
    pub namespace: String,
    /// Previous snapshot in this branch's history. `None` for the genesis commit.
    pub parent_id: Option<SnapshotId>,
    /// Human-readable description of the changes since the previous snapshot.
    pub message: String,
    /// Agent or user identifier for attribution. Optional.
    pub author: Option<String>,
    /// Unix microseconds (i64) — compatible with the existing substrate timestamp convention.
    pub created_at: i64,
    /// Number of entities in this snapshot.
    pub entity_count: u64,
    /// Number of edges in this snapshot.
    pub edge_count: u64,
}

// ── KgBranch ─────────────────────────────────────────────────────────────────

/// Named mutable pointer to a snapshot within a namespace.
///
/// Composite primary key: `(namespace, name)`.
/// The default branch is `"main"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KgBranch {
    /// Namespace this branch lives in.
    pub namespace: String,
    /// Branch name — alphanumeric, hyphens, underscores.
    pub name: String,
    /// The snapshot this branch currently points to.
    pub head_id: SnapshotId,
    /// Unix microseconds when the branch was first created.
    pub created_at: i64,
    /// Unix microseconds of the last HEAD update.
    pub updated_at: i64,
}

// ── RemoteConfig ──────────────────────────────────────────────────────────────

/// Connection parameters for a remote khive instance (for push/pull).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Short name used in CLI commands (e.g. `"origin"`).
    pub name: String,
    /// Base URL of the remote khive-sync server (e.g. `"https://khive.example.com"`).
    pub url: String,
    /// Authentication credentials for the remote.
    pub auth: RemoteAuth,
    /// Optional namespace mapping: `(local_namespace, remote_namespace)`.
    /// When absent, the local namespace name is used on the remote.
    pub namespace_map: Option<(String, String)>,
}

impl RemoteConfig {
    /// Returns the remote namespace name for a given local namespace.
    pub fn remote_namespace<'a>(&'a self, local: &'a str) -> &'a str {
        match &self.namespace_map {
            Some((from, to)) if from == local => to.as_str(),
            _ => local,
        }
    }
}

/// Authentication credentials for a remote khive instance.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteAuth {
    /// No authentication (anonymous access).
    None,
    /// Bearer token (API key).
    Bearer { token: String },
    /// HTTP basic authentication.
    Basic { user: String, password: String },
}

impl std::fmt::Debug for RemoteAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "RemoteAuth::None"),
            Self::Bearer { .. } => write!(f, "RemoteAuth::Bearer {{ token: \"[REDACTED]\" }}"),
            Self::Basic { user, .. } => {
                write!(
                    f,
                    "RemoteAuth::Basic {{ user: {:?}, password: \"[REDACTED]\" }}",
                    user
                )
            }
        }
    }
}

// ── VcsState ─────────────────────────────────────────────────────────────────

/// Per-namespace VCS state stored in `kg_vcs_state`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VcsState {
    pub namespace: String,
    /// Name of the currently active branch. `None` in detached HEAD state.
    pub current_branch: Option<String>,
    /// Last committed snapshot ID. `None` if no commit has been made.
    pub last_committed_id: Option<SnapshotId>,
    /// Whether uncommitted changes exist since the last commit.
    pub dirty: bool,
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
    fn remote_config_namespace_map() {
        let cfg = RemoteConfig {
            name: "origin".into(),
            url: "https://example.com".into(),
            auth: RemoteAuth::None,
            namespace_map: Some(("local".into(), "shared".into())),
        };
        assert_eq!(cfg.remote_namespace("local"), "shared");
        assert_eq!(cfg.remote_namespace("other"), "other");
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
    fn kg_snapshot_serde_roundtrip() {
        let hex = "e".repeat(64);
        let snap = KgSnapshot {
            id: SnapshotId::from_hash(&hex).unwrap(),
            namespace: "test-ns".into(),
            parent_id: None,
            message: "initial commit".into(),
            author: Some("ocean".into()),
            created_at: 1_700_000_000_000_000,
            entity_count: 42,
            edge_count: 7,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: KgSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, snap.id);
        assert_eq!(back.namespace, snap.namespace);
        assert_eq!(back.parent_id, snap.parent_id);
        assert_eq!(back.entity_count, 42);
        assert_eq!(back.edge_count, 7);
        assert_eq!(back.author, Some("ocean".into()));
    }

    #[test]
    fn kg_branch_serde_roundtrip() {
        let branch = KgBranch {
            namespace: "test-ns".into(),
            name: "main".into(),
            head_id: SnapshotId::from_hash(&"f".repeat(64)).unwrap(),
            created_at: 1_000_000,
            updated_at: 2_000_000,
        };
        let json = serde_json::to_string(&branch).unwrap();
        let back: KgBranch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.namespace, branch.namespace);
        assert_eq!(back.name, branch.name);
        assert_eq!(back.head_id, branch.head_id);
        assert_eq!(back.created_at, 1_000_000);
        assert_eq!(back.updated_at, 2_000_000);
    }

    #[test]
    fn remote_auth_bearer_serde_round_trip_and_tag() {
        let auth = RemoteAuth::Bearer {
            token: "tok123".into(),
        };
        let json = serde_json::to_string(&auth).unwrap();
        assert!(json.contains("\"type\":\"bearer\""));
        let back: RemoteAuth = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, RemoteAuth::Bearer { ref token } if token == "tok123"));
    }

    #[test]
    fn remote_auth_debug_redacts_bearer_token() {
        let auth = RemoteAuth::Bearer {
            token: "super-secret".into(),
        };
        let debug = format!("{:?}", auth);
        assert!(
            debug.contains("[REDACTED]"),
            "expected [REDACTED] in: {debug}"
        );
        assert!(!debug.contains("super-secret"), "secret leaked in: {debug}");
    }

    #[test]
    fn remote_auth_debug_redacts_basic_password() {
        let auth = RemoteAuth::Basic {
            user: "alice".into(),
            password: "hunter2".into(),
        };
        let debug = format!("{:?}", auth);
        assert!(debug.contains("alice"));
        assert!(
            debug.contains("[REDACTED]"),
            "expected [REDACTED] in: {debug}"
        );
        assert!(!debug.contains("hunter2"), "password leaked in: {debug}");
    }

    #[test]
    fn remote_config_none_namespace_map_returns_local_name() {
        let cfg = RemoteConfig {
            name: "origin".into(),
            url: "https://example.com".into(),
            auth: RemoteAuth::None,
            namespace_map: None,
        };
        assert_eq!(cfg.remote_namespace("my-ns"), "my-ns");
        assert_eq!(cfg.remote_namespace("other-ns"), "other-ns");
    }

    #[test]
    fn vcs_state_serde_roundtrip() {
        let state = VcsState {
            namespace: "proj".into(),
            current_branch: Some("main".into()),
            last_committed_id: Some(SnapshotId::from_hash(&"0".repeat(64)).unwrap()),
            dirty: true,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: VcsState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.namespace, state.namespace);
        assert_eq!(back.current_branch, Some("main".into()));
        assert!(back.dirty);
        assert_eq!(back.last_committed_id, state.last_committed_id);
    }
}
