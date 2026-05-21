use serde::Deserialize;
use serde_json::Value;

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_vcs::{MergeResult, MergeStrategy, SnapshotId};

use crate::VcsPack;

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, RuntimeError> {
    serde_json::to_value(v).map_err(|e| RuntimeError::Internal(format!("serialize: {e}")))
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

/// Map typed `VcsError` variants to the appropriate `RuntimeError` variant.
///
/// Client/input errors (missing branch, invalid name, uncommitted changes) are
/// `NotFound` or `InvalidInput`. Storage failures map to `Storage`. True invariants
/// stay `Internal`.
fn vcs_to_runtime_error(e: khive_vcs::VcsError) -> RuntimeError {
    match &e {
        khive_vcs::VcsError::BranchNotFound { .. } | khive_vcs::VcsError::SnapshotNotFound(_) => {
            RuntimeError::NotFound(e.to_string())
        }
        khive_vcs::VcsError::InvalidBranchName(_)
        | khive_vcs::VcsError::InvalidSnapshotId(_)
        | khive_vcs::VcsError::UncommittedChanges { .. } => {
            RuntimeError::InvalidInput(e.to_string())
        }
        khive_vcs::VcsError::Storage(_) => RuntimeError::Internal(e.to_string()),
        _ => RuntimeError::Internal(e.to_string()),
    }
}

// ── Param types ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SnapshotParams {
    message: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct BranchParams {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    from_branch: Option<String>,
    #[serde(default)]
    from_snapshot: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
}

/// ADR-015:§D.2 — `checkout` parameters.
///
/// `branch_name` and `snapshot_id` are mutually exclusive; supplying both
/// returns `InvalidInput`. Passing neither defaults to the current branch HEAD.
#[derive(Deserialize)]
struct CheckoutParams {
    /// Branch whose HEAD to restore. Conflicts with `snapshot_id`.
    #[serde(default)]
    branch_name: Option<String>,
    /// Explicit snapshot ID (`sha256:...`) to restore. Conflicts with `branch_name`.
    #[serde(default)]
    snapshot_id: Option<String>,
    /// Old name for snapshot_id — accepted as an alias for backward compat.
    #[serde(default)]
    snapshot: Option<String>,
    #[serde(default)]
    force: Option<bool>,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct LogParams {
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    namespace: Option<String>,
}

/// `merge_branch` — three-way merge per ADR-015:§D.4 / ADR-043:§11.
///
/// `theirs` is required: branch name or snapshot ID (prefix `sha256:`).
/// `source_branch` is accepted as a backward-compatible alias for `theirs`
/// when `theirs` is absent; callers should prefer `theirs`.
#[derive(Deserialize)]
struct MergeBranchParams {
    /// ADR-015 required field: branch name or snapshot ID (`sha256:...`) for
    /// the "theirs" (incoming) side.
    #[serde(default)]
    theirs: Option<String>,
    /// Backward-compatible alias for `theirs`. Ignored when `theirs` is present.
    #[serde(default)]
    source_branch: Option<String>,
    /// Target branch to merge into (default: "main").
    #[serde(default)]
    target_branch: Option<String>,
    #[serde(default)]
    strategy: Option<String>,
    /// When `true` and conflicts remain, snapshot the current working state
    /// directly (agent has manually resolved them).
    #[serde(default)]
    force: Option<bool>,
    /// Commit message for the merge snapshot.
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct ExportParams {
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct ImportParams {
    archive: Value,
    #[serde(default)]
    target_namespace: Option<String>,
}

// ── VCS table initialization ─────────────────────────────────────────────────

async fn ensure_vcs_tables(runtime: &KhiveRuntime) -> Result<(), RuntimeError> {
    let sql = runtime.sql();
    let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
    // VCS_MIGRATIONS_V1 is a multi-statement DDL block; use execute_script
    // (backed by rusqlite::execute_batch) rather than execute (single statement).
    writer
        .execute_script(khive_vcs::migrations::VCS_MIGRATIONS_V1.to_string())
        .await
        .map_err(RuntimeError::Storage)?;
    Ok(())
}

// ── Handler implementations ──────────────────────────────────────────────────

impl VcsPack {
    pub(crate) async fn handle_snapshot(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: SnapshotParams = deser(params)?;
        ensure_vcs_tables(&self.runtime).await?;

        let snapshot = khive_vcs::snapshot::commit(
            &self.runtime,
            p.namespace.as_deref(),
            &p.message,
            p.author.as_deref(),
            p.branch.as_deref(),
        )
        .await
        .map_err(vcs_to_runtime_error)?;

        to_json(&serde_json::json!({
            "id": snapshot.id.as_str(),
            "namespace": snapshot.namespace,
            "parent_id": snapshot.parent_id.as_ref().map(|p| p.as_str().to_string()),
            "message": snapshot.message,
            "author": snapshot.author,
            "entity_count": snapshot.entity_count,
            "edge_count": snapshot.edge_count,
        }))
    }

    pub(crate) async fn handle_branch(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: BranchParams = deser(params)?;
        ensure_vcs_tables(&self.runtime).await?;

        let action = p.action.as_deref().unwrap_or("list");
        match action {
            "create" => {
                let name = p.name.as_deref().ok_or_else(|| {
                    RuntimeError::InvalidInput("branch create requires 'name'".into())
                })?;
                let from_snap = match &p.from_snapshot {
                    Some(s) => Some(SnapshotId::from_prefixed(s).map_err(|e| {
                        RuntimeError::InvalidInput(format!("bad snapshot id: {e}"))
                    })?),
                    None => None,
                };
                let branch = khive_vcs::branch::create_branch(
                    &self.runtime,
                    p.namespace.as_deref(),
                    name,
                    p.from_branch.as_deref(),
                    from_snap.as_ref(),
                )
                .await
                .map_err(vcs_to_runtime_error)?;

                to_json(&serde_json::json!({
                    "namespace": branch.namespace,
                    "name": branch.name,
                    "head_id": branch.head_id.as_str(),
                }))
            }
            "list" => {
                let branches =
                    khive_vcs::branch::list_branches(&self.runtime, p.namespace.as_deref())
                        .await
                        .map_err(vcs_to_runtime_error)?;

                let items: Vec<Value> = branches
                    .into_iter()
                    .map(|b| {
                        serde_json::json!({
                            "namespace": b.namespace,
                            "name": b.name,
                            "head_id": b.head_id.as_str(),
                        })
                    })
                    .collect();
                to_json(&items)
            }
            "get" => {
                let name = p.name.as_deref().ok_or_else(|| {
                    RuntimeError::InvalidInput("branch get requires 'name'".into())
                })?;
                let branch =
                    khive_vcs::branch::get_branch(&self.runtime, p.namespace.as_deref(), name)
                        .await
                        .map_err(vcs_to_runtime_error)?;

                match branch {
                    Some(b) => to_json(&serde_json::json!({
                        "namespace": b.namespace,
                        "name": b.name,
                        "head_id": b.head_id.as_str(),
                    })),
                    None => to_json(&Value::Null),
                }
            }
            other => Err(RuntimeError::InvalidInput(format!(
                "unknown branch action {other:?}; valid: create | list | get"
            ))),
        }
    }

    pub(crate) async fn handle_checkout(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: CheckoutParams = deser(params)?;
        ensure_vcs_tables(&self.runtime).await?;

        // Resolve snapshot_id: prefer explicit `snapshot_id` field, fall back to
        // the old `snapshot` alias for backward compat.
        let snap_str = p.snapshot_id.as_deref().or(p.snapshot.as_deref());

        // ADR-015:§D.2 — snapshot_id and branch_name are mutually exclusive.
        if snap_str.is_some() && p.branch_name.is_some() {
            return Err(RuntimeError::InvalidInput(
                "checkout: 'snapshot_id' and 'branch_name' are mutually exclusive; supply one or neither".into(),
            ));
        }

        let snap_id = match snap_str {
            Some(s) => Some(
                SnapshotId::from_prefixed(s)
                    .map_err(|e| RuntimeError::InvalidInput(format!("bad snapshot id: {e}")))?,
            ),
            None => None,
        };

        let force = p.force.unwrap_or(false);
        let summary = khive_vcs::branch::checkout(
            &self.runtime,
            p.namespace.as_deref(),
            p.branch_name.as_deref(),
            snap_id.as_ref(),
            force,
        )
        .await
        .map_err(vcs_to_runtime_error)?;

        // Return the resolved branch_name from the summary, not the request input.
        to_json(&serde_json::json!({
            "branch_name": summary.branch_name,
            "snapshot_id": summary.snapshot_id.as_str(),
            "entities_restored": summary.entities_restored,
            "edges_restored": summary.edges_restored,
        }))
    }

    pub(crate) async fn handle_log(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: LogParams = deser(params)?;
        ensure_vcs_tables(&self.runtime).await?;

        let limit = p.limit.map(|l| l as u32);
        let snapshots = khive_vcs::log::log(
            &self.runtime,
            p.namespace.as_deref(),
            p.branch.as_deref(),
            limit,
        )
        .await
        .map_err(vcs_to_runtime_error)?;

        let items: Vec<Value> = snapshots
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id.as_str(),
                    "parent_id": s.parent_id.as_ref().map(|p| p.as_str().to_string()),
                    "message": s.message,
                    "author": s.author,
                    "entity_count": s.entity_count,
                    "edge_count": s.edge_count,
                })
            })
            .collect();
        to_json(&items)
    }

    /// `merge_branch` — implements the merge contract from ADR-015:§D.4 / ADR-043:§11.
    ///
    /// `theirs` (required per ADR-015) is a branch name or snapshot ID.
    /// `source_branch` is accepted as a backward-compatible alias when `theirs` is absent.
    ///
    /// ADR-015 specifies that merge operates "into the current working state", so the
    /// live namespace export is used as "ours" (not the committed branch HEAD).  The
    /// merge base is the LCA of the resolved `theirs` snapshot and the last committed
    /// snapshot for the target branch (from `kg_vcs_state`).  For a snapshot-only
    /// `theirs` the base falls back to an empty archive (full `theirs` applied).
    ///
    /// For a clean merge: wipes the target namespace and re-imports from the merged
    /// archive (so deletions in the merged result are applied correctly), then commits
    /// a merge snapshot on the target branch.
    ///
    /// For conflicts with `force=false`: returns structured conflict JSON (status="conflicts").
    /// For conflicts with `force=true`: snapshots the current working state directly
    /// (the agent has already resolved conflicts manually).
    pub(crate) async fn handle_merge_branch(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: MergeBranchParams = deser(params)?;
        ensure_vcs_tables(&self.runtime).await?;

        let ns = p.namespace.as_deref();
        let target = p.target_branch.as_deref().unwrap_or("main");
        let force = p.force.unwrap_or(false);

        // Resolve theirs_name: ADR-015 requires `theirs`; accept `source_branch` as alias.
        let theirs_ref = p
            .theirs
            .as_deref()
            .or(p.source_branch.as_deref())
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "merge_branch: 'theirs' is required (branch name or snapshot ID)".into(),
                )
            })?;

        let strategy = match p.strategy.as_deref() {
            Some("ours") => MergeStrategy::Ours,
            Some("theirs") => MergeStrategy::Theirs,
            Some("three_way") | Some("auto") | None => MergeStrategy::Auto,
            Some(other) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown merge strategy {other:?}; valid: three_way | ours | theirs"
                )))
            }
        };

        // Resolve the "theirs" archive: explicit snapshot ID or branch HEAD.
        // Also record whether theirs is a branch (needed for LCA computation).
        let (theirs_archive, theirs_branch_name) = if theirs_ref.starts_with("sha256:") {
            let snap_id = SnapshotId::from_prefixed(theirs_ref)
                .map_err(|e| RuntimeError::InvalidInput(format!("bad snapshot id: {e}")))?;
            let archive = khive_vcs::snapshot::load_archive(&self.runtime, &snap_id)
                .await
                .map_err(vcs_to_runtime_error)?;
            (archive, None)
        } else {
            let archive = load_branch_archive(&self.runtime, ns, theirs_ref).await?;
            (archive, Some(theirs_ref))
        };

        // ADR-015: "merge into the current working state" — use the live export as
        // "ours" so that uncommitted entities in the live namespace are preserved in
        // the merge calculation.  Using the committed branch HEAD would silently drop
        // any entities added (or deleted) since the last commit.
        let ours_archive = self
            .runtime
            .export_kg(ns)
            .await
            .map_err(|e| RuntimeError::Internal(format!("export live state: {e}")))?;

        // Compute the merge base.
        //
        // For a named branch `theirs`: LCA of the theirs HEAD and the last committed
        // snapshot for the target branch.  If vcs_state has no last_committed_id
        // (no commit yet), fall back to the target branch HEAD.
        //
        // For snapshot-only `theirs`: no coherent branch history, use an empty base
        // (full "theirs" applied against live state).
        let resolved_ns = self.runtime.ns(ns).to_string();
        let base_archive = match theirs_branch_name {
            Some(src_branch) => {
                // Determine the "ours" anchor for LCA: last committed snapshot if
                // available, otherwise the target branch HEAD.
                let state = khive_vcs::snapshot::load_vcs_state(&self.runtime, &resolved_ns)
                    .await
                    .map_err(vcs_to_runtime_error)?;
                let ours_anchor_id = match state.last_committed_id {
                    Some(id) => id,
                    None => {
                        // No commit yet — try target branch HEAD as fallback.
                        let branch = khive_vcs::branch::get_branch(&self.runtime, ns, target)
                            .await
                            .map_err(vcs_to_runtime_error)?
                            .ok_or_else(|| {
                                RuntimeError::NotFound(format!("branch {target:?} not found"))
                            })?;
                        branch.head_id
                    }
                };

                // Load the theirs branch HEAD id for LCA computation.
                let theirs_branch = khive_vcs::branch::get_branch(&self.runtime, ns, src_branch)
                    .await
                    .map_err(vcs_to_runtime_error)?
                    .ok_or_else(|| {
                        RuntimeError::NotFound(format!("branch {src_branch:?} not found"))
                    })?;

                let lca_id = khive_merge::lca::find_lca(
                    &self.runtime,
                    &theirs_branch.head_id,
                    &ours_anchor_id,
                )
                .await
                .map_err(|e| RuntimeError::Internal(format!("find LCA: {e}")))?;

                match lca_id {
                    Some(id) => khive_vcs::snapshot::load_archive(&self.runtime, &id)
                        .await
                        .map_err(vcs_to_runtime_error)?,
                    None => khive_runtime::portability::KgArchive {
                        format: "khive-kg".to_string(),
                        version: "0.1".to_string(),
                        namespace: resolved_ns.clone(),
                        exported_at: chrono::Utc::now(),
                        entities: vec![],
                        edges: vec![],
                    },
                }
            }
            None => khive_runtime::portability::KgArchive {
                format: "khive-kg".to_string(),
                version: "0.1".to_string(),
                namespace: resolved_ns.clone(),
                exported_at: chrono::Utc::now(),
                entities: vec![],
                edges: vec![],
            },
        };

        let merge_result = khive_merge::merge::three_way_merge(
            &base_archive,
            &ours_archive,
            &theirs_archive,
            strategy,
        )
        .map_err(|e| RuntimeError::Internal(format!("merge: {e}")))?;

        let auto_msg = match theirs_branch_name {
            Some(src) => format!("merge {src} into {target}"),
            None => format!("merge snapshot into {target}"),
        };
        let effective_msg = p.message.as_deref().unwrap_or(&auto_msg);

        match merge_result {
            MergeResult::Clean { merged } => {
                // Wipe target namespace then re-import from merged archive so that
                // entities/edges deleted in the merge result are removed from live state.
                khive_vcs::branch::wipe_namespace_for_merge(&self.runtime, ns)
                    .await
                    .map_err(vcs_to_runtime_error)?;
                let summary = self.runtime.import_kg(&merged, ns).await?;

                let snapshot = khive_vcs::snapshot::commit(
                    &self.runtime,
                    ns,
                    effective_msg,
                    None,
                    Some(target),
                )
                .await
                .map_err(vcs_to_runtime_error)?;

                // ADR-015: clean status is "clean".
                to_json(&serde_json::json!({
                    "status": "clean",
                    "snapshot_id": snapshot.id.as_str(),
                    "entities_imported": summary.entities_imported,
                    "edges_imported": summary.edges_imported,
                    "edges_skipped": summary.edges_skipped,
                }))
            }
            MergeResult::Conflicts { conflicts } => {
                if force {
                    // Agent resolved conflicts manually in the working state;
                    // snapshot the current state directly.
                    let snapshot = khive_vcs::snapshot::commit(
                        &self.runtime,
                        ns,
                        effective_msg,
                        None,
                        Some(target),
                    )
                    .await
                    .map_err(vcs_to_runtime_error)?;

                    // ADR-015: forced resolution produces a clean commit.
                    to_json(&serde_json::json!({
                        "status": "clean",
                        "snapshot_id": snapshot.id.as_str(),
                        "note": "force merge: working state snapshotted directly",
                    }))
                } else {
                    let conflicts_json = serde_json::to_value(&conflicts)
                        .map_err(|e| RuntimeError::Internal(format!("serialize conflicts: {e}")))?;
                    // ADR-015: conflict status is "conflicts".
                    to_json(&serde_json::json!({
                        "status": "conflicts",
                        "conflicts": conflicts_json,
                    }))
                }
            }
        }
    }

    pub(crate) async fn handle_export(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: ExportParams = deser(params)?;
        let archive = self.runtime.export_kg(p.namespace.as_deref()).await?;
        to_json(&archive)
    }

    pub(crate) async fn handle_import(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: ImportParams = deser(params)?;
        let archive: khive_runtime::portability::KgArchive = serde_json::from_value(p.archive)
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid archive: {e}")))?;
        let summary = self
            .runtime
            .import_kg(&archive, p.target_namespace.as_deref())
            .await?;
        to_json(&summary)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn load_branch_archive(
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
    branch_name: &str,
) -> Result<khive_runtime::portability::KgArchive, RuntimeError> {
    let branch = khive_vcs::branch::get_branch(runtime, namespace, branch_name)
        .await
        .map_err(vcs_to_runtime_error)?
        .ok_or_else(|| RuntimeError::NotFound(format!("branch {branch_name:?} not found")))?;

    khive_vcs::snapshot::load_archive(runtime, &branch.head_id)
        .await
        .map_err(vcs_to_runtime_error)
}
