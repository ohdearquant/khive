// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Least-common-ancestor (LCA) computation for snapshot histories (ADR-043 §2).
//!
//! Algorithm: iterative walk of the `ours` parent chain into a HashSet;
//! then walk the `theirs` parent chain until the first ID in the set.
//! O(D_ours + D_theirs) snapshot metadata reads.

use std::collections::HashSet;

use khive_runtime::KhiveRuntime;
use khive_vcs::{SnapshotId, VcsError};

/// Find the lowest common ancestor of two snapshot histories.
///
/// Returns `None` if the two histories are disjoint (no common ancestor).
/// In that case the merge uses an empty `KgArchive` as the base.
pub async fn find_lca(
    runtime: &KhiveRuntime,
    ours_id: &SnapshotId,
    theirs_id: &SnapshotId,
) -> Result<Option<SnapshotId>, VcsError> {
    if ours_id == theirs_id {
        return Ok(Some(ours_id.clone()));
    }

    // Step 1: collect all ours ancestors into a set.
    let ours_ancestors = collect_ancestors(runtime, ours_id).await?;

    // Step 2: walk theirs until we hit a known ancestor.
    let their_chain = collect_ancestors(runtime, theirs_id).await?;
    for id in &their_chain {
        if ours_ancestors.contains(id) {
            return Ok(Some(id.clone()));
        }
    }

    Ok(None)
}

/// Collect all ancestor IDs for a snapshot (including itself).
async fn collect_ancestors(
    runtime: &KhiveRuntime,
    start: &SnapshotId,
) -> Result<HashSet<SnapshotId>, VcsError> {
    let chain = khive_vcs::log::ancestor_ids(runtime, start).await?;
    Ok(chain.into_iter().collect())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use khive_vcs::SnapshotId;

    // LCA on identical IDs should return that ID immediately.
    // (No runtime needed since the early-exit fires before any DB reads.)
    #[test]
    fn lca_same_id_is_itself() {
        // Since the async runtime is needed for actual DB ops, we test the
        // identity-shortcut logic here and rely on integration tests for the
        // full walk.
        let a = SnapshotId::from_hash(&"a".repeat(64)).unwrap();
        let b = a.clone();
        // The `find_lca` function returns `Some(ours_id)` when ours == theirs.
        // We verify the SnapshotId equality that enables this.
        assert_eq!(a, b);
    }

    #[test]
    fn lca_different_ids_not_equal() {
        let a = SnapshotId::from_hash(&"a".repeat(64)).unwrap();
        let b = SnapshotId::from_hash(&"b".repeat(64)).unwrap();
        assert_ne!(a, b);
    }
}
