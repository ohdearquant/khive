// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! `commit()` operation — snapshot the current namespace state (ADR-042 §2, ADR-015 §D.1).

use chrono::Utc;
use khive_runtime::KhiveRuntime;
use khive_storage::types::{SqlStatement, SqlValue};

use crate::error::VcsError;
use crate::hash::snapshot_id_for_archive;
use crate::types::{KgSnapshot, SnapshotId, VcsState};

/// Snapshot the current namespace state and advance the current branch HEAD.
///
/// Steps:
/// 1. Export the live namespace via `KhiveRuntime::export_kg`.
/// 2. Compute the content hash.
/// 3. Insert into `kg_snapshots` + `kg_snapshot_archives`.
/// 4. Advance the branch HEAD in `kg_branches`.
/// 5. Clear the dirty flag in `kg_vcs_state`.
///
/// Returns the newly created `KgSnapshot` (without the archive JSON).
pub async fn commit(
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
    message: &str,
    author: Option<&str>,
    branch_name: Option<&str>,
) -> Result<KgSnapshot, VcsError> {
    let ns = runtime.ns(namespace).to_string();
    let branch = branch_name.unwrap_or("main");

    // Export current live state.
    let archive = runtime.export_kg(Some(&ns)).await?;
    let snapshot_id = snapshot_id_for_archive(&archive)?;
    let archive_json = serde_json::to_string(&archive)?;

    let entity_count = archive.entities.len() as u64;
    let edge_count = archive.edges.len() as u64;
    let created_at = Utc::now().timestamp_micros();

    let sql = runtime.sql();
    let mut writer = sql
        .writer()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    // Resolve parent: look up current branch HEAD (if exists).
    let parent_id = get_branch_head_id(&mut *writer, &ns, branch).await?;

    // Check for duplicate (same hash already stored).
    let exists = snapshot_exists(&mut *writer, &snapshot_id).await?;
    if exists {
        // Idempotent: return the existing snapshot metadata.
        return load_snapshot_metadata(&mut *writer, &snapshot_id).await;
    }

    // Insert snapshot metadata.
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO kg_snapshots \
                  (id, namespace, parent_id, message, author, created_at, entity_count, edge_count) \
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
                .to_string(),
            params: vec![
                SqlValue::Text(snapshot_id.as_str().to_string()),
                SqlValue::Text(ns.clone()),
                match &parent_id {
                    Some(pid) => SqlValue::Text(pid.as_str().to_string()),
                    None => SqlValue::Null,
                },
                SqlValue::Text(message.to_string()),
                match author {
                    Some(a) => SqlValue::Text(a.to_string()),
                    None => SqlValue::Null,
                },
                SqlValue::Integer(created_at),
                SqlValue::Integer(entity_count as i64),
                SqlValue::Integer(edge_count as i64),
            ],
            label: Some("vcs:insert_snapshot".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    // Insert archive.
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO kg_snapshot_archives (snapshot_id, archive_json, format) \
                  VALUES (?, ?, 'full')"
                .to_string(),
            params: vec![
                SqlValue::Text(snapshot_id.as_str().to_string()),
                SqlValue::Text(archive_json),
            ],
            label: Some("vcs:insert_archive".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    // Upsert branch HEAD.
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO kg_branches (namespace, name, head_id, created_at, updated_at) \
                  VALUES (?, ?, ?, ?, ?) \
                  ON CONFLICT(namespace, name) \
                  DO UPDATE SET head_id=excluded.head_id, updated_at=excluded.updated_at"
                .to_string(),
            params: vec![
                SqlValue::Text(ns.clone()),
                SqlValue::Text(branch.to_string()),
                SqlValue::Text(snapshot_id.as_str().to_string()),
                SqlValue::Integer(created_at),
                SqlValue::Integer(created_at),
            ],
            label: Some("vcs:upsert_branch".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    // Clear dirty flag + record last committed ID.
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO kg_vcs_state (namespace, current_branch, last_committed_id, dirty) \
                  VALUES (?, ?, ?, 0) \
                  ON CONFLICT(namespace) \
                  DO UPDATE SET current_branch=excluded.current_branch, \
                                last_committed_id=excluded.last_committed_id, \
                                dirty=0"
                .to_string(),
            params: vec![
                SqlValue::Text(ns.clone()),
                SqlValue::Text(branch.to_string()),
                SqlValue::Text(snapshot_id.as_str().to_string()),
            ],
            label: Some("vcs:clear_dirty".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    Ok(KgSnapshot {
        id: snapshot_id,
        namespace: ns,
        parent_id,
        message: message.to_string(),
        author: author.map(str::to_string),
        created_at,
        entity_count,
        edge_count,
    })
}

/// Load the archive JSON for a snapshot.
pub async fn load_archive(
    runtime: &KhiveRuntime,
    snapshot_id: &SnapshotId,
) -> Result<khive_runtime::portability::KgArchive, VcsError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT archive_json FROM kg_snapshot_archives WHERE snapshot_id = ?".to_string(),
            params: vec![SqlValue::Text(snapshot_id.as_str().to_string())],
            label: Some("vcs:load_archive".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    let row = row.ok_or_else(|| VcsError::SnapshotNotFound(snapshot_id.clone()))?;
    let json = match row.get("archive_json") {
        Some(SqlValue::Text(s)) => s.clone(),
        _ => {
            return Err(VcsError::Internal(format!(
                "archive_json is not text for snapshot {}",
                snapshot_id
            )))
        }
    };

    serde_json::from_str(&json).map_err(VcsError::Json)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn get_branch_head_id(
    writer: &mut dyn khive_storage::SqlWriter,
    namespace: &str,
    branch: &str,
) -> Result<Option<SnapshotId>, VcsError> {
    let row = writer
        .query_row(SqlStatement {
            sql: "SELECT head_id FROM kg_branches WHERE namespace = ? AND name = ?".to_string(),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(branch.to_string()),
            ],
            label: None,
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    match row {
        None => Ok(None),
        Some(r) => match r.get("head_id") {
            Some(SqlValue::Text(s)) => Ok(Some(SnapshotId::from_prefixed(s)?)),
            _ => Ok(None),
        },
    }
}

async fn snapshot_exists(
    writer: &mut dyn khive_storage::SqlWriter,
    id: &SnapshotId,
) -> Result<bool, VcsError> {
    let row = writer
        .query_row(SqlStatement {
            sql: "SELECT 1 FROM kg_snapshots WHERE id = ?".to_string(),
            params: vec![SqlValue::Text(id.as_str().to_string())],
            label: None,
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;
    Ok(row.is_some())
}

async fn load_snapshot_metadata(
    writer: &mut dyn khive_storage::SqlWriter,
    id: &SnapshotId,
) -> Result<KgSnapshot, VcsError> {
    let row = writer
        .query_row(SqlStatement {
            sql: "SELECT id, namespace, parent_id, message, author, created_at, \
                  entity_count, edge_count FROM kg_snapshots WHERE id = ?"
                .to_string(),
            params: vec![SqlValue::Text(id.as_str().to_string())],
            label: None,
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?
        .ok_or_else(|| VcsError::SnapshotNotFound(id.clone()))?;

    row_to_snapshot(&row)
}

pub(crate) fn row_to_snapshot(row: &khive_storage::types::SqlRow) -> Result<KgSnapshot, VcsError> {
    let id = match row.get("id") {
        Some(SqlValue::Text(s)) => SnapshotId::from_prefixed(s)?,
        _ => return Err(VcsError::Internal("missing id column".into())),
    };
    let namespace = match row.get("namespace") {
        Some(SqlValue::Text(s)) => s.clone(),
        _ => return Err(VcsError::Internal("missing namespace column".into())),
    };
    let parent_id = match row.get("parent_id") {
        Some(SqlValue::Text(s)) => Some(SnapshotId::from_prefixed(s)?),
        _ => None,
    };
    let message = match row.get("message") {
        Some(SqlValue::Text(s)) => s.clone(),
        _ => String::new(),
    };
    let author = match row.get("author") {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    };
    let created_at = match row.get("created_at") {
        Some(SqlValue::Integer(n)) => *n,
        _ => 0,
    };
    let entity_count = match row.get("entity_count") {
        Some(SqlValue::Integer(n)) => *n as u64,
        _ => 0,
    };
    let edge_count = match row.get("edge_count") {
        Some(SqlValue::Integer(n)) => *n as u64,
        _ => 0,
    };

    Ok(KgSnapshot {
        id,
        namespace,
        parent_id,
        message,
        author,
        created_at,
        entity_count,
        edge_count,
    })
}

/// Load the current VCS state for a namespace.
pub async fn load_vcs_state(runtime: &KhiveRuntime, namespace: &str) -> Result<VcsState, VcsError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT namespace, current_branch, last_committed_id, dirty \
                  FROM kg_vcs_state WHERE namespace = ?"
                .to_string(),
            params: vec![SqlValue::Text(namespace.to_string())],
            label: None,
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    match row {
        None => Ok(VcsState {
            namespace: namespace.to_string(),
            current_branch: Some("main".to_string()),
            last_committed_id: None,
            dirty: false,
        }),
        Some(r) => {
            let current_branch = match r.get("current_branch") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            };
            let last_committed_id = match r.get("last_committed_id") {
                Some(SqlValue::Text(s)) => Some(SnapshotId::from_prefixed(s)?),
                _ => None,
            };
            let dirty = match r.get("dirty") {
                Some(SqlValue::Integer(n)) => *n != 0,
                _ => false,
            };
            Ok(VcsState {
                namespace: namespace.to_string(),
                current_branch,
                last_committed_id,
                dirty,
            })
        }
    }
}
