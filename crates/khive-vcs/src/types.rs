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
        if !khive_types::is_lowercase_hex(hex) {
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
        let mut id = format!("sha256:{}", hex);
        id.make_ascii_lowercase();
        Ok(Self(id))
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
#[path = "types_tests.rs"]
mod tests;
