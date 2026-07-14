// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Cycle-safe lowest-common-ancestor discovery for snapshot histories.
//!
//! See `crates/khive-merge/docs/api/lowest-common-ancestor.md`.

use std::collections::HashSet;

/// Storage-neutral parent lookup for LCA traversal; implementations are shareable.
pub trait SnapshotReader: Send + Sync {
    /// Return the parent snapshot ID for `id`, or `None` if `id` is a root.
    fn parent_of(&self, id: &str) -> Option<String>;
}

/// Finds the lowest common ancestor of two single-parent snapshot histories.
///
/// Returns `None` for disjoint histories. Both walks are cycle-safe and require
/// `O(D_ours + D_theirs)` parent reads.
/// See `crates/khive-merge/docs/api/lowest-common-ancestor.md` for integration behavior.
pub fn find_lca(reader: &dyn SnapshotReader, ours_id: &str, theirs_id: &str) -> Option<String> {
    if ours_id == theirs_id {
        return Some(ours_id.to_string());
    }

    let ours_ancestors = collect_ancestors(reader, ours_id);

    let mut visited_theirs: HashSet<String> = HashSet::new();
    visited_theirs.insert(theirs_id.to_string());
    let mut current = Some(theirs_id.to_string());
    while let Some(id) = current {
        if ours_ancestors.contains(&id) {
            return Some(id);
        }
        let next = reader.parent_of(&id);
        current = match next {
            Some(ref nid) if !visited_theirs.insert(nid.clone()) => None,
            other => other,
        };
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
        let mut parents = HashMap::new();
        parents.insert("d".into(), "c".into());
        parents.insert("e".into(), "c".into());
        parents.insert("c".into(), "base".into());
        let reader = MockReader { parents };
        assert_eq!(find_lca(&reader, "d", "e"), Some("c".into()));
    }

    #[test]
    fn lca_cycle_in_theirs_terminates() {
        let mut parents = HashMap::new();
        parents.insert("a".into(), "root_a".into());
        parents.insert("f".into(), "g".into());
        parents.insert("g".into(), "f".into());
        let reader = MockReader { parents };
        assert_eq!(find_lca(&reader, "a", "f"), None);
    }

    #[test]
    fn lca_cycle_in_ours_terminates() {
        let mut parents = HashMap::new();
        parents.insert("h".into(), "i".into());
        parents.insert("i".into(), "h".into());
        parents.insert("b".into(), "root_b".into());
        let reader = MockReader { parents };
        assert_eq!(find_lca(&reader, "h", "b"), None);
    }
}
