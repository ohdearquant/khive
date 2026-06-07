// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Least-common-ancestor (LCA) computation for snapshot histories.
//!
//! Algorithm: iterative walk of the `ours` parent chain into a HashSet;
//! then walk the `theirs` parent chain until the first ID in the set.
//! O(D_ours + D_theirs) snapshot metadata reads.
//!
//! The `SnapshotReader` trait abstracts the storage backend so the algorithm
//! can be tested independently. Production wiring goes through `KhiveRuntime`.

use std::collections::HashSet;

/// Snapshot reader trait for LCA computation.
///
/// Implementations provide the parent chain for a given snapshot ID.
pub trait SnapshotReader: Send + Sync {
    fn parent_of(&self, id: &str) -> Option<String>;
}

/// Find the lowest common ancestor of two snapshot histories.
///
/// Returns `None` if the two histories are disjoint (no common ancestor).
/// In that case the merge uses an empty `KgArchive` as the base.
pub fn find_lca(reader: &dyn SnapshotReader, ours_id: &str, theirs_id: &str) -> Option<String> {
    if ours_id == theirs_id {
        return Some(ours_id.to_string());
    }

    let ours_ancestors = collect_ancestors(reader, ours_id);

    let mut current = Some(theirs_id.to_string());
    while let Some(id) = current {
        if ours_ancestors.contains(&id) {
            return Some(id);
        }
        current = reader.parent_of(&id);
    }

    None
}

fn collect_ancestors(reader: &dyn SnapshotReader, start: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    set.insert(start.to_string());
    let mut current = reader.parent_of(start);
    while let Some(id) = current {
        if !set.insert(id.clone()) {
            break;
        }
        current = reader.parent_of(&id);
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MockReader {
        parents: HashMap<String, String>,
    }

    impl SnapshotReader for MockReader {
        fn parent_of(&self, id: &str) -> Option<String> {
            self.parents.get(id).cloned()
        }
    }

    #[test]
    fn lca_same_id_is_itself() {
        let reader = MockReader {
            parents: HashMap::new(),
        };
        assert_eq!(find_lca(&reader, "a", "a"), Some("a".into()));
    }

    #[test]
    fn lca_disjoint_returns_none() {
        let mut parents = HashMap::new();
        parents.insert("a".into(), "root_a".into());
        parents.insert("b".into(), "root_b".into());
        let reader = MockReader { parents };
        assert_eq!(find_lca(&reader, "a", "b"), None);
    }

    #[test]
    fn lca_linear_chain() {
        let mut parents = HashMap::new();
        parents.insert("c".into(), "b".into());
        parents.insert("b".into(), "a".into());
        let reader = MockReader { parents };
        assert_eq!(find_lca(&reader, "c", "b"), Some("b".into()));
    }

    #[test]
    fn lca_fork() {
        // ours: d -> c -> base
        // theirs: e -> c -> base
        let mut parents = HashMap::new();
        parents.insert("d".into(), "c".into());
        parents.insert("e".into(), "c".into());
        parents.insert("c".into(), "base".into());
        let reader = MockReader { parents };
        assert_eq!(find_lca(&reader, "d", "e"), Some("c".into()));
    }
}
