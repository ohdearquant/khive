// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! `branch()` and `checkout()` operations (ADR-042 §4, ADR-015 §D.2).

use chrono::Utc;
use khive_runtime::KhiveRuntime;
use khive_storage::types::{SqlStatement, SqlValue};

use crate::error::VcsError;
use crate::snapshot::load_archive;
use crate::types::{KgBranch, SnapshotId};

/// Create a new branch pointing to the current HEAD snapshot of `from_branch`
/// (or directly to `from_snapshot_id` if provided).
///
/// Returns the new `KgBranch`.
pub async fn create_branch(
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
    name: &str,
    from_branch: Option<&str>,
    from_snapshot_id: Option<&SnapshotId>,
) -> Result<KgBranch, VcsError> {
    validate_branch_name(name)?;

    let ns = runtime.ns(namespace).to_string();
    let created_at = Utc::now().timestamp_micros();

    let sql = runtime.sql();
    let mut writer = sql
        .writer()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    // Resolve the HEAD snapshot ID.
    let head_id = if let Some(snap_id) = from_snapshot_id {
        snap_id.clone()
    } else {
        let source_branch = from_branch.unwrap_or("main");
        let row = writer
            .query_row(SqlStatement {
                sql: "SELECT head_id FROM kg_branches WHERE namespace = ? AND name = ?".to_string(),
                params: vec![
                    SqlValue::Text(ns.clone()),
                    SqlValue::Text(source_branch.to_string()),
                ],
                label: None,
            })
            .await
            .map_err(|e| VcsError::Storage(e.to_string()))?;

        match row {
            None => {
                return Err(VcsError::BranchNotFound {
                    namespace: ns,
                    name: source_branch.to_string(),
                })
            }
            Some(r) => match r.get("head_id") {
                Some(SqlValue::Text(s)) => SnapshotId::from_prefixed(s)?,
                _ => return Err(VcsError::Internal("branch head_id is not text".to_string())),
            },
        }
    };

    // Insert new branch.
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO kg_branches (namespace, name, head_id, created_at, updated_at) \
                  VALUES (?, ?, ?, ?, ?)"
                .to_string(),
            params: vec![
                SqlValue::Text(ns.clone()),
                SqlValue::Text(name.to_string()),
                SqlValue::Text(head_id.as_str().to_string()),
                SqlValue::Integer(created_at),
                SqlValue::Integer(created_at),
            ],
            label: Some("vcs:create_branch".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    Ok(KgBranch {
        namespace: ns,
        name: name.to_string(),
        head_id,
        created_at,
        updated_at: created_at,
    })
}

/// List all branches in a namespace.
pub async fn list_branches(
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
) -> Result<Vec<KgBranch>, VcsError> {
    let ns = runtime.ns(namespace).to_string();
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT namespace, name, head_id, created_at, updated_at \
                  FROM kg_branches WHERE namespace = ? ORDER BY created_at ASC"
                .to_string(),
            params: vec![SqlValue::Text(ns)],
            label: Some("vcs:list_branches".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    rows.into_iter().map(|r| row_to_branch(&r)).collect()
}

/// Get a single branch by name.
pub async fn get_branch(
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
    name: &str,
) -> Result<Option<KgBranch>, VcsError> {
    let ns = runtime.ns(namespace).to_string();
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT namespace, name, head_id, created_at, updated_at \
                  FROM kg_branches WHERE namespace = ? AND name = ?"
                .to_string(),
            params: vec![SqlValue::Text(ns), SqlValue::Text(name.to_string())],
            label: None,
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    row.map(|r| row_to_branch(&r)).transpose()
}

/// Restore the namespace to the state recorded in a snapshot.
///
/// - If `force = false` and `dirty = true`, returns `VcsError::UncommittedChanges`.
/// - Deletes all current entities/edges and re-imports from the snapshot archive.
/// - Updates `kg_vcs_state` to reflect the new current branch and clears dirty.
/// - Returns `(branch_name, snapshot, entities_restored, edges_restored)`.
pub async fn checkout(
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
    branch_name: Option<&str>,
    snapshot_id: Option<&SnapshotId>,
    force: bool,
) -> Result<CheckoutSummary, VcsError> {
    let ns = runtime.ns(namespace).to_string();

    // Resolve which snapshot to restore.
    let (resolved_branch, resolved_snap_id) =
        resolve_checkout_target(runtime, &ns, branch_name, snapshot_id).await?;

    // Check for uncommitted changes (unless force=true).
    // ADR-042/ADR-015: checkout must reject when the live state diverges from the
    // last committed snapshot. Runtime entity writes do not set kg_vcs_state.dirty
    // directly, so we detect divergence by hashing the current archive and comparing
    // it against last_committed_id stored in kg_vcs_state.
    if !force {
        let state = crate::snapshot::load_vcs_state(runtime, &ns).await?;
        let has_uncommitted = if state.dirty {
            // Dirty flag explicitly set (e.g. by a previous partial operation).
            true
        } else {
            // Hash-compare live state to the last committed snapshot.
            has_diverged_from_last_commit(runtime, &ns, state.last_committed_id.as_ref()).await?
        };
        if has_uncommitted {
            let count = estimate_uncommitted_count(runtime, &ns).await?;
            return Err(VcsError::UncommittedChanges { count });
        }
    }

    // Load the archive for this snapshot.
    let archive = load_archive(runtime, &resolved_snap_id).await?;

    let entities_count = archive.entities.len();
    let edges_count = archive.edges.len();

    // Delete all current entities and edges in the namespace.
    wipe_namespace(runtime, &ns).await?;

    // Re-import.
    runtime
        .import_kg(&archive, Some(&ns))
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    // Update VCS state.
    let now = Utc::now().timestamp_micros();
    let sql = runtime.sql();
    let mut writer = sql
        .writer()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;
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
                match &resolved_branch {
                    Some(b) => SqlValue::Text(b.clone()),
                    None => SqlValue::Null,
                },
                SqlValue::Text(resolved_snap_id.as_str().to_string()),
            ],
            label: Some("vcs:checkout_state".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;
    drop(writer);

    // Update branch HEAD if we checked out a named branch.
    if let Some(ref b) = resolved_branch {
        let _ = sql
            .writer()
            .await
            .map_err(|e| VcsError::Storage(e.to_string()))?
            .execute(SqlStatement {
                sql: "UPDATE kg_branches SET head_id = ?, updated_at = ? \
                      WHERE namespace = ? AND name = ?"
                    .to_string(),
                params: vec![
                    SqlValue::Text(resolved_snap_id.as_str().to_string()),
                    SqlValue::Integer(now),
                    SqlValue::Text(ns.clone()),
                    SqlValue::Text(b.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| VcsError::Storage(e.to_string()))?;
    }

    Ok(CheckoutSummary {
        branch_name: resolved_branch,
        snapshot_id: resolved_snap_id,
        entities_restored: entities_count,
        edges_restored: edges_count,
        vector_index_status: VectorIndexStatus::Synchronous,
    })
}

/// Result of a `checkout` operation.
#[derive(Debug)]
pub struct CheckoutSummary {
    pub branch_name: Option<String>,
    pub snapshot_id: SnapshotId,
    pub entities_restored: usize,
    pub edges_restored: usize,
    /// In v0.1 FTS5 is rebuilt synchronously during import; vector store is also synchronous
    /// because `import_kg` calls `reindex_entity` which handles both.
    pub vector_index_status: VectorIndexStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorIndexStatus {
    /// Both FTS5 and vector store were rebuilt synchronously during checkout.
    Synchronous,
    /// Vector re-embedding is still in progress (future async mode).
    Rebuilding,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

async fn resolve_checkout_target(
    runtime: &KhiveRuntime,
    namespace: &str,
    branch_name: Option<&str>,
    snapshot_id: Option<&SnapshotId>,
) -> Result<(Option<String>, SnapshotId), VcsError> {
    match (branch_name, snapshot_id) {
        (_, Some(sid)) => Ok((None, sid.clone())),
        (Some(branch), None) => {
            let branch_row = get_branch(runtime, Some(namespace), branch).await?;
            match branch_row {
                None => Err(VcsError::BranchNotFound {
                    namespace: namespace.to_string(),
                    name: branch.to_string(),
                }),
                Some(b) => Ok((Some(branch.to_string()), b.head_id)),
            }
        }
        (None, None) => {
            // Default to current branch.
            let state = crate::snapshot::load_vcs_state(runtime, namespace).await?;
            match state.current_branch {
                Some(b) => {
                    let branch_row = get_branch(runtime, Some(namespace), &b).await?;
                    match branch_row {
                        None => Err(VcsError::BranchNotFound {
                            namespace: namespace.to_string(),
                            name: b,
                        }),
                        Some(br) => Ok((Some(b), br.head_id)),
                    }
                }
                None => Err(VcsError::Internal(
                    "no current branch and no target specified".to_string(),
                )),
            }
        }
    }
}

/// Estimate uncommitted change count as the symmetric difference between the
/// last committed archive and the live export.
///
/// Returns the number of entity IDs and edge IDs that appear in one snapshot
/// but not the other (i.e. added + deleted).  A pure deletion (e.g. committing
/// one entity then deleting it) therefore reports `count = 1`, not `count = 0`.
///
/// Falls back to the live entity count when no committed snapshot exists yet.
async fn estimate_uncommitted_count(
    runtime: &KhiveRuntime,
    namespace: &str,
) -> Result<usize, VcsError> {
    let state = crate::snapshot::load_vcs_state(runtime, namespace).await?;

    let last_committed_id = match state.last_committed_id {
        Some(id) => id,
        None => {
            // No prior commit — live count is the best estimate.
            let entities = runtime
                .list_entities(Some(namespace), None, u32::MAX)
                .await
                .map_err(|e| VcsError::Storage(e.to_string()))?;
            return Ok(entities.len());
        }
    };

    let committed = crate::snapshot::load_archive(runtime, &last_committed_id).await?;
    let live = runtime
        .export_kg(Some(namespace))
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    use std::collections::HashSet;

    // Compare by entity UUID strings.
    let committed_entity_ids: HashSet<String> = committed
        .entities
        .iter()
        .map(|e| e.id.to_string())
        .collect();
    let live_entity_ids: HashSet<String> = live.entities.iter().map(|e| e.id.to_string()).collect();

    // Edges have no standalone ID; use (source, target, relation) as a composite key.
    let committed_edge_keys: HashSet<String> = committed
        .edges
        .iter()
        .map(|e| format!("{}/{}/{}", e.source, e.target, e.relation))
        .collect();
    let live_edge_keys: HashSet<String> = live
        .edges
        .iter()
        .map(|e| format!("{}/{}/{}", e.source, e.target, e.relation))
        .collect();

    // Symmetric difference: records present in one set but not the other.
    let entity_diff = committed_entity_ids
        .symmetric_difference(&live_entity_ids)
        .count();
    let edge_diff = committed_edge_keys
        .symmetric_difference(&live_edge_keys)
        .count();

    Ok(entity_diff + edge_diff)
}

/// Return `true` when the live namespace state has diverged from the last committed snapshot.
///
/// Hashes the current `export_kg` archive and compares it against `last_committed_id`.
/// If there is no committed snapshot yet (first-use), any non-empty live state is
/// considered dirty (must commit before checkout).
async fn has_diverged_from_last_commit(
    runtime: &KhiveRuntime,
    namespace: &str,
    last_committed_id: Option<&SnapshotId>,
) -> Result<bool, VcsError> {
    let archive = runtime
        .export_kg(Some(namespace))
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;
    let live_id = crate::hash::snapshot_id_for_archive(&archive)?;

    match last_committed_id {
        Some(committed) => Ok(&live_id != committed),
        None => {
            // No previous commit: only clean if the namespace is completely empty.
            Ok(!archive.entities.is_empty() || !archive.edges.is_empty())
        }
    }
}

/// Delete all entities and edges in a namespace to prepare for a merge restore.
///
/// Exposed for the VCS pack's `merge_branch` handler which needs to apply
/// the merged archive through a restore-style path (wipe + import) so that
/// entities/edges absent from the merge result are removed from live state.
pub async fn wipe_namespace_for_merge(
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
) -> Result<(), VcsError> {
    let ns = runtime.ns(namespace).to_string();
    wipe_namespace(runtime, &ns).await
}

/// Delete all entities and edges in a namespace to prepare for checkout restore.
///
/// Uses bulk SQL DELETEs (edges first, then entities) to avoid N+1 overhead.
/// FTS5 and vector index cleanup is intentionally skipped here: the subsequent
/// `import_kg` call rebuilds both indexes via `reindex_entity` for every imported
/// entity, so any stale entries left behind are overwritten on import.
async fn wipe_namespace(runtime: &KhiveRuntime, namespace: &str) -> Result<(), VcsError> {
    // Ensure the entity/graph schemas exist before running raw SQL against them.
    // (Lazy schema creation means tables may not exist yet on a fresh in-memory DB.)
    runtime
        .entities(Some(namespace))
        .map_err(|e| VcsError::Storage(e.to_string()))?;
    runtime
        .graph(Some(namespace))
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    let sql = runtime.sql();
    let mut writer = sql
        .writer()
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    let ns_param = vec![SqlValue::Text(namespace.to_string())];

    // Delete edges before entities (referential integrity).
    writer
        .execute(SqlStatement {
            sql: "DELETE FROM graph_edges WHERE namespace = ?1".to_string(),
            params: ns_param.clone(),
            label: Some("wipe_edges".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    writer
        .execute(SqlStatement {
            sql: "DELETE FROM entities WHERE namespace = ?1".to_string(),
            params: ns_param,
            label: Some("wipe_entities".to_string()),
        })
        .await
        .map_err(|e| VcsError::Storage(e.to_string()))?;

    Ok(())
}

/// Validate that a branch name matches `^[a-zA-Z0-9_-]{1,64}$`.
fn validate_branch_name(name: &str) -> Result<(), VcsError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(VcsError::InvalidBranchName(name.to_string()));
    }
    Ok(())
}

fn row_to_branch(row: &khive_storage::types::SqlRow) -> Result<KgBranch, VcsError> {
    let namespace = match row.get("namespace") {
        Some(SqlValue::Text(s)) => s.clone(),
        _ => return Err(VcsError::Internal("missing namespace".into())),
    };
    let name = match row.get("name") {
        Some(SqlValue::Text(s)) => s.clone(),
        _ => return Err(VcsError::Internal("missing name".into())),
    };
    let head_id = match row.get("head_id") {
        Some(SqlValue::Text(s)) => SnapshotId::from_prefixed(s)?,
        _ => return Err(VcsError::Internal("missing head_id".into())),
    };
    let created_at = match row.get("created_at") {
        Some(SqlValue::Integer(n)) => *n,
        _ => 0,
    };
    let updated_at = match row.get("updated_at") {
        Some(SqlValue::Integer(n)) => *n,
        _ => created_at,
    };

    Ok(KgBranch {
        namespace,
        name,
        head_id,
        created_at,
        updated_at,
    })
}
