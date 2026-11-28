//! Checkout reuse: a committed `git.checkout` of the same commit, by the same namespace, in the
//! same repository has already stored that commit's tree. Answering with it skips reading and
//! storing every blob again, which a repeat put does at nearly the cost of a new one because it
//! restarts the object's publish grace with a full sync.
//!
//! The record is the checkout receipt itself: its result holds `{commit, tree}`. A candidate is
//! answered only when its manifest loads and every blob it names is still in the store; any
//! other outcome, a failed read included, leaves the caller on the full checkout path, which is
//! correct on its own. A reused tree does not restart its blobs' publish grace.

use std::path::Path;

use khive_runtime::blob_tree as tree;
use khive_runtime::KhiveRuntime;
use khive_storage::types::{SqlStatement, SqlValue};

use crate::sql::sql;

/// The tree of the newest committed checkout of `commit` by `namespace` in `repo` that is still
/// whole in the blob store, if any.
pub(super) async fn reusable_tree(
    rt: &KhiveRuntime,
    namespace: &str,
    repo: &Path,
    commit: &str,
) -> Option<String> {
    let mut reader = rt.sql().reader().await.ok()?;
    let rows = reader
        .query_all(SqlStatement {
            sql: sql!("checkout_receipts_by_commit_select").into(),
            params: vec![
                SqlValue::Text(namespace.into()),
                SqlValue::Text(repo.display().to_string()),
                SqlValue::Text(commit.to_ascii_lowercase()),
            ],
            label: Some("git_checkout_reuse".into()),
        })
        .await
        .ok()?;
    // No SQL reader is held across blob reads.
    drop(reader);
    for row in &rows {
        let Some(SqlValue::Text(tree_ref)) = row.get("tree") else {
            continue;
        };
        let Ok(entries) = tree::load(rt, tree_ref).await else {
            continue;
        };
        if tree::verify_blobs(rt, &entries).await.is_ok() {
            return Some(tree_ref.clone());
        }
    }
    None
}

#[cfg(test)]
#[path = "local_git_checkout_reuse_tests.rs"]
mod tests;
