// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Least-common-ancestor (LCA) computation for snapshot histories.
//!
//! Algorithm: iterative walk of the `ours` parent chain into a HashSet;
//! then walk the `theirs` parent chain until the first ID in the set.
//! O(D_ours + D_theirs) snapshot metadata reads.
//!
//! The `SnapshotReader` abstraction decouples the algorithm from the VCS
//! storage backend so it can be unit-tested without a live runtime.

use std::collections::HashSet;

use khive_vcs::{SnapshotId, VcsError};

use crate::diff_local::SnapshotReader;

/// Find the lowest common ancestor of two snapshot histories.
///
/// Returns `None` if the two histories are disjoint (no common ancestor).
/// In that case the merge should use an empty `KgArchive` as the base.
pub fn find_lca(
    reader: &dyn SnapshotReader,
    ours_id: &SnapshotId,
    theirs_id: &SnapshotId,
) -> Result<Option<SnapshotId>, VcsError> {
    if ours_id == theirs_id {
        return Ok(Some(ours_id.clone()));
    }

    // Step 1: collect all ours ancestor IDs into a set.
    let ours_ancestors = collect_ancestors(reader, ours_id)?;

    // Step 2: walk theirs chain until we hit a known ancestor.
    let their_chain = collect_ancestors(reader, theirs_id)?;
    for id in &their_chain {
        if ours_ancestors.contains(id) {
            return Ok(Some(id.clone()));
        }
    }

    Ok(None)
}

/// Collect all ancestor IDs for a snapshot (including itself).
///
/// Uses the `SnapshotReader` trait so the walk can be tested with
/// in-memory fixtures without a live VCS backend.
fn collect_ancestors(
    reader: &dyn SnapshotReader,
    start: &SnapshotId,
) -> Result<HashSet<SnapshotId>, VcsError> {
    let mut visited: HashSet<SnapshotId> = HashSet::new();
    let mut queue: Vec<SnapshotId> = vec![start.clone()];

    while let Some(current) = queue.pop() {
        if visited.contains(&current) {
            continue;
        }
        visited.insert(current.clone());
        if let Some(parent_str) = reader.parent_of(current.as_str()) {
            if let Ok(parent_id) = SnapshotId::from_prefixed(&parent_str) {
                queue.push(parent_id);
            }
        }
    }

    Ok(visited)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use khive_vcs::SnapshotId;

    use super::*;
    use crate::diff_local::SnapshotReader;

    /// In-memory snapshot graph for testing.
    ///
    /// Maps `snapshot_id.as_str()` → `parent_id.as_str()`.
    struct MapReader(HashMap<String, String>);

    impl SnapshotReader for MapReader {
        fn parent_of(&self, id: &str) -> Option<String> {
            self.0.get(id).cloned()
        }
    }

    fn sha(c: char) -> SnapshotId {
        SnapshotId::from_hash(&c.to_string().repeat(64)).unwrap()
    }

    #[test]
    fn lca_same_id_is_itself() {
        let reader = MapReader(HashMap::new());
        let a = sha('a');
        let result = find_lca(&reader, &a, &a).unwrap();
        assert_eq!(result, Some(a));
    }

    #[test]
    fn lca_different_ids_no_common_ancestor() {
        let reader = MapReader(HashMap::new());
        let a = sha('a');
        let b = sha('b');
        // No parent entries → disjoint histories.
        let result = find_lca(&reader, &a, &b).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn lca_direct_ancestor() {
        // History: a ← b (b's parent is a)
        let a = sha('a');
        let b = sha('b');
        let mut map = HashMap::new();
        map.insert(b.as_str().to_string(), a.as_str().to_string());
        let reader = MapReader(map);

        // LCA(b, a) = a
        let result = find_lca(&reader, &b, &a).unwrap();
        assert_eq!(result, Some(a.clone()));

        // LCA(a, b) = a
        let reader2 = MapReader(reader.0.clone());
        let result2 = find_lca(&reader2, &a, &b).unwrap();
        assert_eq!(result2, Some(a));
    }

    #[test]
    fn lca_common_ancestor_two_hops() {
        // History: a ← b ← c  and  a ← d
        // LCA(c, d) = a
        let a = sha('a');
        let b = sha('b');
        let c = sha('c');
        let d = sha('d');
        let mut map = HashMap::new();
        map.insert(b.as_str().to_string(), a.as_str().to_string());
        map.insert(c.as_str().to_string(), b.as_str().to_string());
        map.insert(d.as_str().to_string(), a.as_str().to_string());
        let reader = MapReader(map);

        let result = find_lca(&reader, &c, &d).unwrap();
        assert_eq!(result, Some(a));
    }

    #[test]
    fn lca_disjoint_histories_returns_none() {
        // a ← b and  c ← d — no shared ancestor.
        let a = sha('a');
        let b = sha('b');
        let c = sha('c');
        let d = sha('d');
        let mut map = HashMap::new();
        map.insert(b.as_str().to_string(), a.as_str().to_string());
        map.insert(d.as_str().to_string(), c.as_str().to_string());
        let reader = MapReader(map);

        let result = find_lca(&reader, &b, &d).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn collect_ancestors_includes_start() {
        let a = sha('a');
        let reader = MapReader(HashMap::new());
        let ancestors = collect_ancestors(&reader, &a).unwrap();
        assert!(
            ancestors.contains(&a),
            "start node must be in its own ancestor set"
        );
    }
}
