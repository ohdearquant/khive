// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! `log()` operation — list snapshot history for a branch (ADR-015 `log` tool).

use khive_runtime::KhiveRuntime;
use khive_storage::types::{SqlStatement, SqlValue};

use crate::error::VcsError;
use crate::snapshot::row_to_snapshot;
use crate::types::{KgSnapshot, SnapshotId};

/// List snapshot metadata for a branch, most recent first.
///
/// Does NOT load the archive JSON — only metadata from `kg_snapshots`.
/// `limit` defaults to 20; pass `u32::MAX` for the full history.
pub async fn log(
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
    branch_name: Option<&str>,
    limit: Option<u32>,
) -> Result<Vec<KgSnapshot>, VcsError> {
    let ns = runtime.ns(namespace).to_string();
    let branch = branch_name.unwrap_or("main");
    let limit = limit.unwrap_or(20) as i64;

    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    // Get branch HEAD.
    let head_row = reader
        .query_row(SqlStatement {
            sql: "SELECT head_id FROM kg_branches WHERE namespace = ? AND name = ?".to_string(),
            params: vec![
                SqlValue::Text(ns.clone()),
                SqlValue::Text(branch.to_string()),
            ],
            label: None,
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    let head_id = match head_row {
        None => {
            return Err(VcsError::BranchNotFound {
                namespace: ns,
                name: branch.to_string(),
            })
        }
        Some(r) => match r.get("head_id") {
            Some(SqlValue::Text(s)) => SnapshotId::from_prefixed(s)?,
            _ => return Err(VcsError::Internal("branch head_id is not text".to_string())),
        },
    };

    // Walk parent chain up to `limit` snapshots.
    let mut results = Vec::new();
    let mut current_id: Option<SnapshotId> = Some(head_id);

    while let Some(id) = current_id {
        if results.len() as i64 >= limit {
            break;
        }

        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT id, namespace, parent_id, message, author, created_at, \
                      entity_count, edge_count FROM kg_snapshots WHERE id = ?"
                    .to_string(),
                params: vec![SqlValue::Text(id.as_str().to_string())],
                label: None,
            })
            .await
            .map_err(|e| VcsError::Storage(e.to_string()))?;

        match row {
            None => break, // Snapshot referenced but not stored (partial fetch).
            Some(r) => {
                let snap = row_to_snapshot(&r)?;
                let next_parent = snap.parent_id.clone();
                results.push(snap);
                current_id = next_parent;
            }
        }
    }

    Ok(results)
}

/// Walk the full ancestor chain of a snapshot, returning all ancestor IDs.
///
/// Used by `khive-merge` for LCA computation. Returns a `Vec` ordered from
/// HEAD to genesis (most recent first).
pub async fn ancestor_ids(
    runtime: &KhiveRuntime,
    start_id: &SnapshotId,
) -> Result<Vec<SnapshotId>, VcsError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    let mut ids = Vec::new();
    let mut current: Option<SnapshotId> = Some(start_id.clone());

    while let Some(id) = current {
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT parent_id FROM kg_snapshots WHERE id = ?".to_string(),
                params: vec![SqlValue::Text(id.as_str().to_string())],
                label: None,
            })
            .await
            .map_err(|e| VcsError::Storage(e.to_string()))?;

        match row {
            None => break,
            Some(r) => {
                let parent_id = match r.get("parent_id") {
                    Some(SqlValue::Text(s)) => Some(SnapshotId::from_prefixed(s)?),
                    _ => None,
                };
                ids.push(id);
                current = parent_id;
            }
        }
    }

    Ok(ids)
}
