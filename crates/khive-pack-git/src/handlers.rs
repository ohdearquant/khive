//! `git.digest` verb handler (ADR-088 Amendment 1).
//!
//! Resolves the `source` argument (local path or `https://` URL, cloning/
//! fetching remote sources into the scratch cache), resolves or auto-creates
//! the repo-anchor `project` entity, then drives the shared
//! `ingest::run_ingest` core with a bounded, cursor-resumable pass.

use std::path::Path;

use anyhow::anyhow;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::{SqlStatement, SqlValue};

use crate::cache::{self, CacheError};
use crate::ingest::{
    resolve_project_id, run_ingest, run_ingest_with_commit_recovery, CacheRepairStrategy,
    GitLogError, IngestInclude, IngestOptions, RecoveredRepo,
};
use crate::source::{parse_source, repo_basename, repo_identity, DigestSource, REPO_SLUG_PROPERTY};
use crate::GitPack;

/// Issue #765 bounded repair policy: at most one refetch, then at most one
/// reclone. See crates/khive-pack-git/docs/api/handlers.md#remoterecoverystage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteRecoveryStage {
    Initial,
    Refetched,
    Recloned,
}

pub(crate) struct RemoteCommitRecovery {
    canonical_url: String,
    stage: RemoteRecoveryStage,
}

impl RemoteCommitRecovery {
    pub(crate) fn new(canonical_url: impl Into<String>) -> Self {
        Self {
            canonical_url: canonical_url.into(),
            stage: RemoteRecoveryStage::Initial,
        }
    }

    /// Advance the repair state machine by one step for a classified
    /// `GitLogError`. See crates/khive-pack-git/docs/api/handlers.md#repair.
    pub(crate) fn repair(
        &mut self,
        _repo: &Path,
        _error: &GitLogError,
    ) -> anyhow::Result<Option<RecoveredRepo>> {
        match self.stage {
            RemoteRecoveryStage::Initial => match cache::refetch_clone(&self.canonical_url) {
                Ok(repo) => {
                    self.stage = RemoteRecoveryStage::Refetched;
                    Ok(Some(RecoveredRepo {
                        repo,
                        strategy: CacheRepairStrategy::Refetch,
                    }))
                }
                // The refetch command itself failed at the git level (e.g.
                // the remote still cannot supply the missing objects) --
                // fall through to the one guarded reclone immediately rather
                // than surfacing the refetch failure. An I/O, size-cap, or
                // ownership-guard failure is terminal: it is not a signal
                // that a fresh clone would fare any differently, and is
                // never worth risking a second destructive operation for.
                Err(CacheError::Git(_)) => {
                    self.stage = RemoteRecoveryStage::Refetched;
                    self.reclone()
                }
                Err(e) => Err(anyhow!("cache repair (refetch) failed: {e}")),
            },
            RemoteRecoveryStage::Refetched => self.reclone(),
            RemoteRecoveryStage::Recloned => Ok(None),
        }
    }

    fn reclone(&mut self) -> anyhow::Result<Option<RecoveredRepo>> {
        match cache::reclone(&self.canonical_url) {
            Ok(repo) => {
                self.stage = RemoteRecoveryStage::Recloned;
                Ok(Some(RecoveredRepo {
                    repo,
                    strategy: CacheRepairStrategy::Reclone,
                }))
            }
            Err(e) => Err(anyhow!("cache repair (reclone) failed: {e}")),
        }
    }
}

const DEFAULT_MAX_ITEMS: i64 = 500;
const MIN_MAX_ITEMS: i64 = 1;
const MAX_MAX_ITEMS: i64 = 2000;

impl GitPack {
    pub(crate) async fn handle_digest(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let source_raw = params
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("git.digest requires source".into()))?;
        let source =
            parse_source(source_raw).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        // Parsed as i64 (not u64) so an out-of-range negative value clamps to
        // MIN_MAX_ITEMS instead of failing `as_u64` and silently falling
        // through to the default -- a caller passing `-1` gets the smallest
        // legal budget, not an unrequested 500-item pass. A non-integer
        // value (string, float, bool, array, object) is rejected outright
        // rather than silently defaulted.
        let max_items = match params.get("max_items") {
            None | Some(Value::Null) => DEFAULT_MAX_ITEMS,
            Some(v) => v.as_i64().ok_or_else(|| {
                RuntimeError::InvalidInput(format!("max_items must be an integer, got {v:?}"))
            })?,
        }
        .clamp(MIN_MAX_ITEMS, MAX_MAX_ITEMS) as u64;

        let include = match params.get("include") {
            None | Some(Value::Null) => IngestInclude::default(),
            Some(v) => parse_include(v)?,
        };

        let mut warnings: Vec<String> = Vec::new();

        // Resolve a local repo path -- remote sources clone/fetch into the
        // scratch cache first (ADR-088 Amendment 1 §Remote-URL mode).
        let (repo_path, gh_capable) = match &source {
            DigestSource::Local(p) => (p.clone(), true),
            DigestSource::Remote { canonical, gh_slug } => {
                let cloned = cache::ensure_clone(canonical).map_err(|e| {
                    RuntimeError::InvalidInput(format!(
                        "remote clone/fetch of {canonical:?} failed: {e}"
                    ))
                })?;
                if gh_slug.is_none() {
                    warnings.push(format!(
                        "host for {canonical:?} is not github.com; issue/pull_request \
                         ingestion is skipped (commits-only degradation, ADR-088 Amendment 1)"
                    ));
                }
                (cloned, gh_slug.is_some())
            }
        };

        // Resolve or auto-create the repo-anchor `project` entity.
        let resolution = match params.get("project").and_then(Value::as_str) {
            Some(raw) => {
                let id = resolve_project_id(self.runtime(), raw)
                    .await
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(format!(
                            "project {raw:?} did not resolve to an entity"
                        ))
                    })?;
                ProjectResolution {
                    id,
                    created: false,
                    orphan: None,
                }
            }
            None => resolve_or_create_project(self.runtime(), registry, token, &source).await?,
        };
        let project_id = resolution.id;
        let project_created = resolution.created;

        let effective_include = IngestInclude {
            commits: include.commits,
            issues: include.issues && gh_capable,
            pull_requests: include.pull_requests && gh_capable,
        };

        let opts = IngestOptions {
            repo: repo_path,
            project: project_id.to_string(),
            max_items: Some(max_items),
            include: effective_include,
        };

        // Only a remote-URL source has a disposable cache to repair (ADR-088
        // Amendment 1) -- a local path is the caller's own working copy and
        // is never a candidate for self-heal (issue #765).
        let mut report = match &source {
            DigestSource::Local(_) => run_ingest(self.runtime(), token, registry, opts).await,
            DigestSource::Remote { canonical, .. } => {
                let mut recovery = RemoteCommitRecovery::new(canonical.clone());
                run_ingest_with_commit_recovery(self.runtime(), token, registry, opts, {
                    move |repo, err| recovery.repair(repo, err)
                })
                .await
            }
        }
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        report.warnings.extend(warnings);
        report.project_id = Some(project_id.to_string());
        report.project_created = project_created;
        if let Some(orphan) = resolution.orphan {
            report.orphaned_corpus_detected = true;
            report.orphaned_project_id = Some(orphan.dead_project_id.to_string());
            report.orphaned_note_count = orphan.annotated_note_count;
        }

        serde_json::to_value(&report)
            .map_err(|e| RuntimeError::InvalidInput(format!("serializing report: {e}")))
    }
}

fn parse_include(v: &Value) -> Result<IngestInclude, RuntimeError> {
    let arr = v
        .as_array()
        .ok_or_else(|| RuntimeError::InvalidInput("include must be an array of strings".into()))?;
    let mut include = IngestInclude {
        commits: false,
        issues: false,
        pull_requests: false,
    };
    for entry in arr {
        let s = entry
            .as_str()
            .ok_or_else(|| RuntimeError::InvalidInput("include entries must be strings".into()))?;
        match s {
            "commits" => include.commits = true,
            "issues" => include.issues = true,
            "pull_requests" => include.pull_requests = true,
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown include kind {other:?}; valid: commits | issues | pull_requests"
                )))
            }
        }
    }
    Ok(include)
}

/// Outcome of `resolve_or_create_project`'s match/create decision.
pub(crate) struct ProjectResolution {
    pub(crate) id: Uuid,
    pub(crate) created: bool,
    /// `Some` when `created` is `true` AND a soft-deleted anchor for this
    /// repo identity was found with a live corpus still annotating it
    /// (issue #1173) — surfaced via `IngestReport`, never silent.
    pub(crate) orphan: Option<OrphanSignal>,
}

pub(crate) struct OrphanSignal {
    pub(crate) dead_project_id: Uuid,
    pub(crate) annotated_note_count: u64,
}

/// Find an existing `project` entity whose `properties.repo_slug` matches
/// the source's canonical repo identity (issue #1173), falling back to a
/// legacy `properties.repo_url` match for pre-existing anchors that predate
/// the slug property (backfilling it on match so future calls converge on
/// the slug lookup without a migration); create the anchor when neither
/// matches (ADR-088 Amendment 1 — auto-creation is reported via
/// `IngestReport.project_created`, never silent). The basename `name`
/// fallback from the original v1 match is intentionally gone: it is both
/// too weak (a differently-named legacy anchor is missed) and too broad (an
/// unrelated `project` entity sharing the basename would capture the
/// ingest) — see issue #1173.
async fn resolve_or_create_project(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    source: &DigestSource,
) -> Result<ProjectResolution, RuntimeError> {
    let repo_url = match source {
        DigestSource::Local(p) => p.to_string_lossy().to_string(),
        DigestSource::Remote { canonical, .. } => canonical.clone(),
    };
    let identity = repo_identity(source);
    let name = repo_basename(source);

    if let Some(id) = find_project_by_slug(runtime, token, &identity)
        .await
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?
    {
        return Ok(ProjectResolution {
            id,
            created: false,
            orphan: None,
        });
    }

    if let Some(id) = find_project_by_legacy_repo_url(runtime, token, &repo_url)
        .await
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?
    {
        registry
            .dispatch(
                "update",
                json!({
                    "id": id.to_string(),
                    "properties": { REPO_SLUG_PROPERTY: identity },
                }),
            )
            .await?;
        return Ok(ProjectResolution {
            id,
            created: false,
            orphan: None,
        });
    }

    let orphan = find_orphaned_anchor(runtime, token, &identity, &repo_url)
        .await
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

    let resp = registry
        .dispatch(
            "create",
            json!({
                "kind": "project",
                "name": name,
                "properties": { "repo_url": repo_url, REPO_SLUG_PROPERTY: identity },
            }),
        )
        .await?;
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| {
            RuntimeError::InvalidInput("create(kind=project) did not return an id".into())
        })?;
    Ok(ProjectResolution {
        id,
        created: true,
        orphan,
    })
}

async fn find_project_by_slug(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &str,
) -> anyhow::Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NULL \
                  AND json_extract(properties,'$.repo_slug')=?2 \
                  LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(identity.to_string()),
            ],
            label: Some("git_digest_find_project_by_slug".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| match r.get("id") {
        Some(SqlValue::Uuid(u)) => Some(*u),
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
        _ => None,
    }))
}

async fn find_project_by_legacy_repo_url(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    repo_url: &str,
) -> anyhow::Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NULL \
                  AND json_extract(properties,'$.repo_url')=?2 \
                  LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(repo_url.to_string()),
            ],
            label: Some("git_digest_find_project_by_legacy_repo_url".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| match r.get("id") {
        Some(SqlValue::Uuid(u)) => Some(*u),
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
        _ => None,
    }))
}

/// Look for a soft-deleted `project` anchor matching the resolved repo
/// identity (or its legacy `repo_url` spelling) that still has a live
/// `commit`/`issue`/`pull_request` corpus `annotates`-linked to it (issue
/// #1173 items 2/3). A hard-deleted anchor cannot be detected this way — its
/// row, including `properties.repo_slug`, is gone — this covers the soft
/// delete (the default; see ADR-014) case, where the identity survives.
async fn find_orphaned_anchor(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &str,
    repo_url: &str,
) -> anyhow::Result<Option<OrphanSignal>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NOT NULL \
                  AND (json_extract(properties,'$.repo_slug')=?2 \
                       OR json_extract(properties,'$.repo_url')=?3) \
                  ORDER BY deleted_at DESC LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(identity.to_string()),
                SqlValue::Text(repo_url.to_string()),
            ],
            label: Some("git_digest_find_orphaned_anchor".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let Some(dead_project_id) = row.and_then(|r| match r.get("id") {
        Some(SqlValue::Uuid(u)) => Some(*u),
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
        _ => None,
    }) else {
        return Ok(None);
    };

    let count = r
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM notes n \
                  JOIN graph_edges e ON e.source_id = n.id AND e.namespace = n.namespace \
                  WHERE n.namespace = ?1 AND n.deleted_at IS NULL \
                  AND n.kind IN ('commit', 'issue', 'pull_request') \
                  AND e.relation = 'annotates' AND e.target_id = ?2 AND e.deleted_at IS NULL"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(dead_project_id.to_string()),
            ],
            label: Some("git_digest_count_orphaned_notes".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let annotated_note_count = match count {
        Some(SqlValue::Integer(n)) => n as u64,
        _ => 0,
    };

    Ok(Some(OrphanSignal {
        dead_project_id,
        annotated_note_count,
    }))
}
