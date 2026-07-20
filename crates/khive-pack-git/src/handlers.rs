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
use crate::source::{
    parse_source, redact_repo_url, remote_url_to_slug, repo_basename, repo_identity, DigestSource,
    REPO_SLUG_PROPERTY,
};
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
                    slug_duplicates: Vec::new(),
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
        if !resolution.slug_duplicates.is_empty() {
            report.warnings.push(format!(
                "multiple live project anchors match the same repo identity; selected oldest {}; duplicates: {}",
                project_id,
                resolution
                    .slug_duplicates
                    .iter()
                    .map(Uuid::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
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
    /// Additional live anchors carrying the same `repo_slug` beyond the
    /// deterministically selected one (ADR-088 Amendment 2 step-1
    /// multi-match) — surfaced as a report warning, never silent.
    pub(crate) slug_duplicates: Vec<Uuid>,
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
    let identity = repo_identity(source).await;
    let name = repo_basename(source);

    let slug_matches = find_projects_by_slug(runtime, token, &identity)
        .await
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
    if let Some((id, duplicates)) = slug_matches.split_first() {
        return Ok(ProjectResolution {
            id: *id,
            created: false,
            orphan: None,
            slug_duplicates: duplicates.to_vec(),
        });
    }

    let exact_matches = find_projects_by_legacy_repo_url(runtime, token, &repo_url)
        .await
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
    if let Some((id, duplicates)) = exact_matches.split_first() {
        let id = *id;
        registry
            .dispatch(
                "update",
                json!({
                    "id": id.to_string(),
                    // Backfill hits also redact the stored repo_url in the
                    // same patch (ADR-088 Amendment 2 step 2) -- the
                    // lazy-upgrade path closes out any credential-bearing
                    // legacy URL it touches.
                    "properties": {
                        REPO_SLUG_PROPERTY: identity,
                        "repo_url": redact_repo_url(&repo_url),
                    },
                }),
            )
            .await?;
        return Ok(ProjectResolution {
            id,
            created: false,
            orphan: None,
            slug_duplicates: duplicates.to_vec(),
        });
    }

    // ADR-088 Amendment 2 step 2: a legacy anchor that predates `repo_slug`
    // entirely (so its stored `repo_url` is some other spelling of the same
    // repository -- e.g. a bare local clone path from before this identity
    // model existed) is reconciled by re-deriving each such anchor's own
    // identity from its stored `repo_url` and comparing it to this source's
    // resolved `identity`, not by a second exact-string match.
    let legacy_candidates = find_legacy_projects_without_slug(runtime, token)
        .await
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
    let mut normalized_matches: Vec<(Uuid, String)> = Vec::new();
    for (id, candidate_repo_url) in legacy_candidates {
        if normalize_legacy_repo_url(&candidate_repo_url)
            .await
            .as_deref()
            == Some(identity.as_str())
        {
            normalized_matches.push((id, candidate_repo_url));
        }
    }
    if let Some(((selected, selected_repo_url), duplicates)) = normalized_matches.split_first() {
        let selected = *selected;
        registry
            .dispatch(
                "update",
                json!({
                    "id": selected.to_string(),
                    // Same-patch redaction of the matched anchor's own stored
                    // repo_url (ADR-088 Amendment 2 step 2).
                    "properties": {
                        REPO_SLUG_PROPERTY: identity,
                        "repo_url": redact_repo_url(selected_repo_url),
                    },
                }),
            )
            .await?;
        return Ok(ProjectResolution {
            id: selected,
            created: false,
            orphan: None,
            slug_duplicates: duplicates.iter().map(|(id, _)| *id).collect(),
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
                "properties": {
                    "repo_url": redact_repo_url(&repo_url),
                    REPO_SLUG_PROPERTY: identity,
                },
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
        slug_duplicates: Vec::new(),
    })
}

// Multiple live anchors can carry one slug when two legacy anchors holding
// different URL spellings of the same repository were each exact-matched and
// backfilled on separate ingests. Selection must be deterministic (oldest
// `created_at`, id tie-break) and the condition surfaced as a report warning,
// never an arbitrary or silent pick (ADR-088 Amendment 2).
async fn find_projects_by_slug(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &str,
) -> anyhow::Result<Vec<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NULL \
                  AND json_extract(properties,'$.repo_slug')=?2 \
                  ORDER BY created_at ASC, id ASC"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(identity.to_string()),
            ],
            label: Some("git_digest_find_projects_by_slug".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(rows
        .iter()
        .filter_map(|r| match r.get("id") {
            Some(SqlValue::Uuid(u)) => Some(*u),
            Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
            _ => None,
        })
        .collect())
}

/// Exact step-2 legacy match (ADR-088 Amendment 2): every live pre-slug
/// anchor whose stored `repo_url` equals the source spelling verbatim,
/// ordered `created_at ASC, id ASC` so multi-match cases select the oldest
/// deterministically and surface the remainder as a report warning -- the
/// same contract as the step-1 slug lookup and the normalized step-2 route.
/// Anchors that already carry `repo_slug` are excluded: they are found (if
/// at all) via `find_projects_by_slug`, and an exact `repo_url` hit must
/// never overwrite an already-derived identity.
async fn find_projects_by_legacy_repo_url(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    repo_url: &str,
) -> anyhow::Result<Vec<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NULL \
                  AND json_extract(properties,'$.repo_slug') IS NULL \
                  AND json_extract(properties,'$.repo_url')=?2 \
                  ORDER BY created_at ASC, id ASC"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(repo_url.to_string()),
            ],
            label: Some("git_digest_find_projects_by_legacy_repo_url".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(rows
        .iter()
        .filter_map(|r| match r.get("id") {
            Some(SqlValue::Uuid(u)) => Some(*u),
            Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
            _ => None,
        })
        .collect())
}

/// Fetch every live `project` anchor in this namespace that has no
/// `repo_slug` yet -- candidates for step-2 normalization reconciliation
/// (ADR-088 Amendment 2). An anchor that already carries `repo_slug` was
/// either created post-#1173 or already backfilled by the exact-match path
/// above; it is found (if at all) via `find_projects_by_slug` instead.
async fn find_legacy_projects_without_slug(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> anyhow::Result<Vec<(Uuid, String)>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id, json_extract(properties,'$.repo_url') AS repo_url \
                  FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NULL \
                  AND json_extract(properties,'$.repo_slug') IS NULL \
                  AND json_extract(properties,'$.repo_url') IS NOT NULL \
                  ORDER BY created_at ASC, id ASC"
                .into(),
            params: vec![SqlValue::Text(token.namespace().as_str().to_string())],
            label: Some("git_digest_find_legacy_projects_without_slug".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let id = match r.get("id") {
                Some(SqlValue::Uuid(u)) => Some(*u),
                Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
                _ => None,
            }?;
            let url = match r.get("repo_url") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            }?;
            Some((id, url))
        })
        .collect())
}

/// Re-derive the repo-identity slug a legacy anchor's stored `repo_url`
/// would resolve to today (ADR-088 Amendment 2 step 2). A URL-shaped value
/// normalizes directly via `remote_url_to_slug`. A path-shaped value (an
/// absolute local path, stored verbatim by the pre-#1173 local-source
/// resolution path) is treated as a local clone and resolved the same way
/// `repo_identity` resolves a `DigestSource::Local` -- via its current
/// `origin` remote -- so a legacy local-path anchor reconciles with a
/// later remote-URL digest of the same repository. Returns `None` when
/// neither path yields a URL-shaped identity (e.g. the path no longer
/// exists, or has no matching origin -- there is nothing to reconcile
/// against).
async fn normalize_legacy_repo_url(repo_url: &str) -> Option<String> {
    if let Some(slug) = remote_url_to_slug(repo_url) {
        return Some(slug);
    }
    if repo_url.starts_with('/') {
        let candidate = DigestSource::Local(std::path::PathBuf::from(repo_url));
        let slug = repo_identity(&candidate).await;
        if !slug.starts_with("local:") {
            return Some(slug);
        }
    }
    None
}

/// Look for a soft-deleted `project` anchor matching the resolved repo
/// identity (or its legacy `repo_url` spelling) that still has a live
/// `commit`/`issue`/`pull_request` corpus `annotates`-linked to it (issue
/// #1173 items 2/3). A hard-deleted anchor cannot be detected this way — its
/// row, including `properties.repo_slug`, is gone — this covers the soft
/// delete (the default; see ADR-014) case, where the identity survives.
///
/// Multiple soft-deleted tombstones can share the same identity (repeated
/// delete/re-ingest cycles). The most-recently-deleted one is not
/// necessarily the one still holding the live corpus — e.g. a later
/// tombstone created by an empty re-ingest-then-delete has zero annotating
/// notes while an older one still has the original corpus. Every matching
/// tombstone (newest first) is checked in turn; the signal fires only for
/// the first one found with at least one live annotating note. A tombstone
/// with zero live notes is not an orphan — it is a routine delete of an
/// already-empty anchor — so `None` is returned instead of `Some` with a
/// zero count.
async fn find_orphaned_anchor(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &str,
    repo_url: &str,
) -> anyhow::Result<Option<OrphanSignal>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NOT NULL \
                  AND (json_extract(properties,'$.repo_slug')=?2 \
                       OR json_extract(properties,'$.repo_url')=?3) \
                  ORDER BY deleted_at DESC"
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
    let dead_project_ids = rows.into_iter().filter_map(|r| match r.get("id") {
        Some(SqlValue::Uuid(u)) => Some(*u),
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
        _ => None,
    });

    for dead_project_id in dead_project_ids {
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
        if annotated_note_count > 0 {
            return Ok(Some(OrphanSignal {
                dead_project_id,
                annotated_note_count,
            }));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use khive_runtime::{Namespace, VerbRegistryBuilder};

    use super::*;

    async fn fixture() -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(rt.clone()));
        builder.register(GitPack::new(rt.clone()));
        let registry = builder.build().expect("registry builds");
        rt.install_edge_rules(registry.all_edge_rules());
        registry.apply_schema_plans(rt.backend());
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        (rt, token, registry)
    }

    async fn create_note_annotating(
        registry: &VerbRegistry,
        kind: &str,
        name: &str,
        project_id: Uuid,
    ) -> Uuid {
        let properties = match kind {
            "commit" => json!({ "sha": "deadbeef".repeat(5) }),
            "issue" | "pull_request" => {
                json!({ "number": 1, "project_id": project_id.to_string() })
            }
            other => panic!("unsupported note kind in test helper: {other}"),
        };
        let resp = registry
            .dispatch(
                "create",
                json!({
                    "kind": kind,
                    "name": name,
                    "content": format!("{name} body"),
                    "properties": properties,
                    "annotates": [project_id.to_string()],
                }),
            )
            .await
            .expect("create note ok");
        Uuid::parse_str(resp["id"].as_str().expect("id present")).expect("id is uuid")
    }

    /// Regression for the 505-dup incident shape (issue #1173): a repo
    /// digested once via a local clone path and once via its remote https
    /// URL must converge on ONE anchor, not mint a second one that then
    /// re-ingests the whole corpus from an empty start.
    #[tokio::test]
    async fn same_repo_via_local_and_remote_spelling_resolves_to_one_anchor() {
        let (rt, token, registry) = fixture().await;

        let dir = tempfile::tempdir().expect("tempdir");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .expect("git init");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/org/dupe-repo",
            ])
            .status()
            .expect("git remote add");
        assert!(status.success());

        let local_source = DigestSource::Local(dir.path().to_path_buf());
        let remote_source = DigestSource::Remote {
            canonical: "https://github.com/org/dupe-repo".to_string(),
            gh_slug: Some(("org".to_string(), "dupe-repo".to_string())),
        };

        let first = resolve_or_create_project(&rt, &registry, &token, &local_source)
            .await
            .expect("first resolve");
        assert!(first.created);

        let second = resolve_or_create_project(&rt, &registry, &token, &remote_source)
            .await
            .expect("second resolve");
        assert!(!second.created, "second spelling must match, not re-create");
        assert_eq!(first.id, second.id);
    }

    /// An unrelated `project` entity that happens to share the repo's
    /// basename must NOT capture the ingest (issue #1173 item 1 -- the
    /// basename fallback is dropped entirely).
    #[tokio::test]
    async fn basename_collision_with_unrelated_project_is_not_captured() {
        let (rt, token, registry) = fixture().await;

        let unrelated_id = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "collide-repo",
                    "properties": { "repo_url": "https://example.com/totally/unrelated" },
                }),
            )
            .await
            .expect("create unrelated project");
        let unrelated_id = Uuid::parse_str(unrelated_id["id"].as_str().unwrap()).expect("uuid");

        let source = DigestSource::Remote {
            canonical: "https://github.com/org/collide-repo".to_string(),
            gh_slug: Some(("org".to_string(), "collide-repo".to_string())),
        };
        let resolution = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve");
        assert!(
            resolution.created,
            "basename collision must not capture an unrelated anchor"
        );
        assert_ne!(resolution.id, unrelated_id);
    }

    /// A pre-existing anchor created before this fix (only `properties.repo_url`,
    /// no `repo_slug`) is matched by legacy `repo_url` lookup and backfilled
    /// with `repo_slug`, so subsequent calls converge on the slug match
    /// without a migration (issue #1173 item 1).
    #[tokio::test]
    async fn legacy_anchor_without_slug_is_matched_and_backfilled() {
        let (rt, token, registry) = fixture().await;

        let source = DigestSource::Remote {
            canonical: "https://github.com/org/legacy-repo".to_string(),
            gh_slug: Some(("org".to_string(), "legacy-repo".to_string())),
        };
        let repo_url = "https://github.com/org/legacy-repo";

        let legacy_id = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "legacy-repo",
                    "properties": { "repo_url": repo_url },
                }),
            )
            .await
            .expect("create legacy project");
        let legacy_id = Uuid::parse_str(legacy_id["id"].as_str().unwrap()).expect("uuid");

        let resolution = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve");
        assert!(
            !resolution.created,
            "legacy repo_url match must not re-create"
        );
        assert_eq!(resolution.id, legacy_id);

        let entity = rt
            .get_entity(&token, legacy_id)
            .await
            .expect("fetch legacy entity");
        assert_eq!(
            entity
                .properties
                .as_ref()
                .and_then(|p| p.get("repo_slug"))
                .and_then(Value::as_str),
            Some("github.com/org/legacy-repo"),
            "repo_slug must be backfilled on the legacy anchor"
        );
    }

    /// Two live anchors sharing one `repo_slug` (each backfilled from a
    /// different legacy `repo_url` spelling) must resolve deterministically
    /// to one of them with the rest surfaced as duplicates, never an
    /// arbitrary or silent pick (ADR-088 Amendment 2 step-1 multi-match).
    #[tokio::test]
    async fn duplicate_slug_anchors_resolve_deterministically_with_signal() {
        let (rt, token, registry) = fixture().await;

        let slug = "github.com/org/dup-repo";
        let mut ids = Vec::new();
        for repo_url in [
            "https://github.com/org/dup-repo",
            "git@github.com:org/dup-repo.git",
        ] {
            let resp = registry
                .dispatch(
                    "create",
                    json!({
                        "kind": "project",
                        "name": "dup-repo",
                        "properties": { "repo_url": repo_url, "repo_slug": slug },
                    }),
                )
                .await
                .expect("create anchor");
            ids.push(Uuid::parse_str(resp["id"].as_str().unwrap()).expect("uuid"));
        }

        let source = DigestSource::Remote {
            canonical: "https://github.com/org/dup-repo".to_string(),
            gh_slug: Some(("org".to_string(), "dup-repo".to_string())),
        };
        let first = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve");
        assert!(!first.created, "multi-match must not create a third anchor");
        assert!(
            ids.contains(&first.id),
            "selected anchor must be one of the existing pair"
        );
        assert_eq!(
            first.slug_duplicates,
            ids.iter()
                .copied()
                .filter(|id| *id != first.id)
                .collect::<Vec<_>>(),
            "the unselected anchor must be surfaced as a duplicate"
        );

        let second = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve again");
        assert_eq!(
            second.id, first.id,
            "selection must be deterministic across calls"
        );
    }

    /// A hard-deleted-vs-soft-deleted anchor whose corpus is still
    /// `annotates`-linked surfaces a distinct, non-silent signal instead of
    /// quietly minting a fresh anchor over an orphaned corpus (issue #1173
    /// items 2/3).
    #[tokio::test]
    async fn orphaned_anchor_is_flagged_not_silently_reminted() {
        let (rt, token, registry) = fixture().await;

        let source = DigestSource::Remote {
            canonical: "https://github.com/org/orphan-repo".to_string(),
            gh_slug: Some(("org".to_string(), "orphan-repo".to_string())),
        };

        let dead = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "orphan-repo",
                    "properties": {
                        "repo_url": "https://github.com/org/orphan-repo",
                        "repo_slug": "github.com/org/orphan-repo",
                    },
                }),
            )
            .await
            .expect("create dead anchor");
        let dead_id = Uuid::parse_str(dead["id"].as_str().unwrap()).expect("uuid");

        create_note_annotating(&registry, "commit", "c1", dead_id).await;
        create_note_annotating(&registry, "issue", "#1 bug", dead_id).await;

        let deleted = rt
            .delete_entity(&token, dead_id, false)
            .await
            .expect("soft delete");
        assert!(deleted);

        let resolution = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve");
        assert!(
            resolution.created,
            "no live anchor for this slug -- a fresh one is minted"
        );
        assert_ne!(resolution.id, dead_id);
        let orphan = resolution
            .orphan
            .expect("orphaned corpus must be flagged, not silent");
        assert_eq!(orphan.dead_project_id, dead_id);
        assert_eq!(orphan.annotated_note_count, 2);
    }

    /// A soft-deleted anchor with zero live annotating notes is a routine
    /// delete of an already-empty anchor, not an orphaned corpus -- it must
    /// not raise the signal (issue #1185 finding 3).
    #[tokio::test]
    async fn tombstone_with_zero_live_notes_is_not_flagged_as_orphan() {
        let (rt, token, registry) = fixture().await;

        let source = DigestSource::Remote {
            canonical: "https://github.com/org/empty-tombstone-repo".to_string(),
            gh_slug: Some(("org".to_string(), "empty-tombstone-repo".to_string())),
        };

        let dead = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "empty-tombstone-repo",
                    "properties": {
                        "repo_url": "https://github.com/org/empty-tombstone-repo",
                        "repo_slug": "github.com/org/empty-tombstone-repo",
                    },
                }),
            )
            .await
            .expect("create dead anchor");
        let dead_id = Uuid::parse_str(dead["id"].as_str().unwrap()).expect("uuid");

        // No annotating notes created -- this tombstone never had a corpus.
        let deleted = rt
            .delete_entity(&token, dead_id, false)
            .await
            .expect("soft delete");
        assert!(deleted);

        let resolution = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve");
        assert!(resolution.created);
        assert!(
            resolution.orphan.is_none(),
            "zero live notes must not raise the orphan signal"
        );
    }

    /// When several soft-deleted tombstones share the same repo identity,
    /// the signal must select the one that actually still has a live
    /// annotating corpus -- not merely the most-recently-deleted one (issue
    /// #1185 finding 3).
    #[tokio::test]
    async fn orphan_signal_selects_tombstone_with_live_corpus_among_several() {
        let (rt, token, registry) = fixture().await;

        let source = DigestSource::Remote {
            canonical: "https://github.com/org/multi-tombstone-repo".to_string(),
            gh_slug: Some(("org".to_string(), "multi-tombstone-repo".to_string())),
        };
        let properties = json!({
            "repo_url": "https://github.com/org/multi-tombstone-repo",
            "repo_slug": "github.com/org/multi-tombstone-repo",
        });

        // Older tombstone: still has a live annotating corpus.
        let old_dead = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "multi-tombstone-repo",
                    "properties": properties.clone(),
                }),
            )
            .await
            .expect("create old dead anchor");
        let old_dead_id = Uuid::parse_str(old_dead["id"].as_str().unwrap()).expect("uuid");
        create_note_annotating(&registry, "commit", "c-old", old_dead_id).await;
        rt.delete_entity(&token, old_dead_id, false)
            .await
            .expect("soft delete old");

        // deleted_at ordering must be distinct for the two tombstones.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        // Newer tombstone: an empty re-ingest-then-delete cycle, zero live notes.
        let new_dead = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "multi-tombstone-repo",
                    "properties": properties,
                }),
            )
            .await
            .expect("create new dead anchor");
        let new_dead_id = Uuid::parse_str(new_dead["id"].as_str().unwrap()).expect("uuid");
        rt.delete_entity(&token, new_dead_id, false)
            .await
            .expect("soft delete new");

        let resolution = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve");
        assert!(resolution.created);
        let orphan = resolution
            .orphan
            .expect("orphaned corpus must be flagged, not silent");
        assert_eq!(
            orphan.dead_project_id, old_dead_id,
            "signal must point at the tombstone with the live corpus, not merely the most recent one"
        );
        assert_eq!(orphan.annotated_note_count, 1);
    }

    /// Persisted `repo_url` must never carry userinfo or a query-string
    /// token (ADR-088 Amendment 2) -- the in-memory canonical (used only
    /// for the identity slug and any clone/gh operation) is unaffected.
    #[tokio::test]
    async fn persisted_repo_url_is_credential_and_query_redacted() {
        let (rt, token, registry) = fixture().await;

        let source = DigestSource::Remote {
            canonical: "https://user:tok3n@github.com/org/redact-repo?token=SECRETQUERY#frag"
                .to_string(),
            gh_slug: Some(("org".to_string(), "redact-repo".to_string())),
        };

        let resolution = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve");
        assert!(resolution.created);

        let entity = rt
            .get_entity(&token, resolution.id)
            .await
            .expect("fetch entity");
        let stored_url = entity
            .properties
            .as_ref()
            .and_then(|p| p.get("repo_url"))
            .and_then(Value::as_str)
            .expect("repo_url present");
        assert!(!stored_url.contains("tok3n"), "{stored_url}");
        assert!(!stored_url.contains("SECRETQUERY"), "{stored_url}");
        assert!(!stored_url.contains('#'), "{stored_url}");
        assert_eq!(stored_url, "https://github.com/org/redact-repo");

        assert_eq!(
            entity
                .properties
                .as_ref()
                .and_then(|p| p.get("repo_slug"))
                .and_then(Value::as_str),
            Some("github.com/org/redact-repo")
        );
    }

    fn init_bare_repo_with_origin(dir: &Path, origin: &str) {
        for args in [vec!["init", "-q"], vec!["remote", "add", "origin", origin]] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        }
    }

    /// Same as [`init_bare_repo_with_origin`] but with `user.*` configured
    /// and one commit -- `git log` (and thus `git.digest`) needs a real
    /// commit to walk; a freshly-inited repo with zero commits fails at
    /// the `git log` step before anchor resolution is even exercised.
    fn init_repo_with_origin_and_one_commit(dir: &Path, origin: &str) {
        init_bare_repo_with_origin(dir, origin);
        for args in [
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test User"],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        }
        std::fs::write(dir.join("README.md"), "hello\n").expect("write file");
        for args in [
            vec!["add", "README.md"],
            vec!["commit", "-q", "-m", "Initial commit"],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        }
    }

    /// A legacy anchor created before `repo_slug` existed at all, from a
    /// LOCAL path source (so its `repo_url` is a bare filesystem path with
    /// no `repo_slug`), is reconciled by a later remote-URL digest of the
    /// same repository via step-2 normalization (ADR-088 Amendment 2).
    #[tokio::test]
    async fn legacy_local_path_anchor_reconciled_by_later_remote_digest() {
        let (rt, token, registry) = fixture().await;

        let dir = tempfile::tempdir().expect("tempdir");
        init_bare_repo_with_origin(dir.path(), "https://github.com/org/legacy-local-repo");

        let path_str = dir.path().to_string_lossy().to_string();
        let legacy_id = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "legacy-local-repo",
                    "properties": { "repo_url": path_str },
                }),
            )
            .await
            .expect("create legacy local anchor");
        let legacy_id = Uuid::parse_str(legacy_id["id"].as_str().unwrap()).expect("uuid");

        let remote_source = DigestSource::Remote {
            canonical: "https://github.com/org/legacy-local-repo".to_string(),
            gh_slug: Some(("org".to_string(), "legacy-local-repo".to_string())),
        };
        let resolution = resolve_or_create_project(&rt, &registry, &token, &remote_source)
            .await
            .expect("resolve");
        assert!(
            !resolution.created,
            "legacy local-path anchor must be reconciled, not re-created"
        );
        assert_eq!(resolution.id, legacy_id);

        let entity = rt
            .get_entity(&token, legacy_id)
            .await
            .expect("fetch entity");
        assert_eq!(
            entity
                .properties
                .as_ref()
                .and_then(|p| p.get("repo_slug"))
                .and_then(Value::as_str),
            Some("github.com/org/legacy-local-repo"),
            "repo_slug must be backfilled via step-2 normalization"
        );
    }

    /// Public-surface regression (ADR-088 Amendment 2 round-2 finding): the
    /// duplicate-anchor warning and selected id, and all three orphan
    /// report fields (including the no-orphan defaults), must be observable
    /// on the real `git.digest` verb's serialized `IngestReport` -- not
    /// merely on the private `resolve_or_create_project` helper's return
    /// value. Driven through `registry.dispatch("git.digest", ...)` over a
    /// LOCAL (no-network) source so it needs no real remote clone.
    #[tokio::test]
    async fn git_digest_public_surface_reports_duplicate_and_selects_oldest_no_third_anchor() {
        let (rt, token, registry) = fixture().await;

        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_origin_and_one_commit(dir.path(), "https://github.com/org/pub-dup-repo");

        let slug = "github.com/org/pub-dup-repo";
        let mut ids = Vec::new();
        for repo_url in [
            "https://github.com/org/pub-dup-repo",
            "git@github.com:org/pub-dup-repo.git",
        ] {
            let resp = registry
                .dispatch(
                    "create",
                    json!({
                        "kind": "project",
                        "name": "pub-dup-repo",
                        "properties": { "repo_url": repo_url, "repo_slug": slug },
                    }),
                )
                .await
                .expect("create anchor");
            ids.push(Uuid::parse_str(resp["id"].as_str().unwrap()).expect("uuid"));
        }

        let source_path = dir.path().to_string_lossy().to_string();
        let resp = registry
            .dispatch(
                "git.digest",
                json!({
                    "source": source_path,
                    "include": ["commits"],
                    "max_items": 1,
                }),
            )
            .await
            .expect("git.digest dispatch");

        assert_eq!(resp["project_created"], json!(false), "{resp}");
        let selected_id = resp["project_id"]
            .as_str()
            .expect("project_id present")
            .to_string();
        assert!(
            ids.iter().any(|id| id.to_string() == selected_id),
            "selected id must be one of the pre-seeded pair: {resp}"
        );
        assert_eq!(selected_id, ids[0].to_string(), "must select the oldest");

        let warnings = resp["warnings"].as_array().expect("warnings array");
        assert!(
            warnings.iter().any(|w| {
                let w = w.as_str().unwrap_or("");
                w.contains("duplicate")
                    && w.contains(&selected_id)
                    && w.contains(&ids[1].to_string())
            }),
            "duplicate warning must name the selected and duplicate ids: {warnings:?}"
        );

        // No-orphan defaults must be present on the wire shape.
        assert_eq!(resp["orphaned_corpus_detected"], json!(false), "{resp}");
        assert_eq!(resp["orphaned_project_id"], json!(null), "{resp}");
        assert_eq!(resp["orphaned_note_count"], json!(0), "{resp}");

        // No third anchor was minted.
        let live = find_projects_by_slug(&rt, &token, slug)
            .await
            .expect("find_projects_by_slug");
        assert_eq!(live.len(), 2, "no third anchor should be minted: {live:?}");
    }

    /// Exact step-2 multi-match (ADR-088 Amendment 2 round-3 finding): two
    /// live pre-slug anchors sharing the source's exact `repo_url` spelling
    /// must resolve to the oldest deterministically, surface the remainder
    /// in the report warning, and mint no third anchor -- observed on the
    /// public `git.digest` wire shape, not the private helper.
    #[tokio::test]
    async fn git_digest_exact_legacy_multi_match_selects_oldest_and_warns() {
        let (rt, token, registry) = fixture().await;

        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_origin_and_one_commit(dir.path(), "https://github.com/org/exact-dup-repo");
        let source_path = dir.path().to_string_lossy().to_string();

        let mut ids = Vec::new();
        for _ in 0..2 {
            let resp = registry
                .dispatch(
                    "create",
                    json!({
                        "kind": "project",
                        "name": "exact-dup-repo",
                        // Pre-slug anchors: repo_url only, exactly the local
                        // source spelling the handler will match on.
                        "properties": { "repo_url": source_path.clone() },
                    }),
                )
                .await
                .expect("create anchor");
            ids.push(Uuid::parse_str(resp["id"].as_str().unwrap()).expect("uuid"));
        }

        let resp = registry
            .dispatch(
                "git.digest",
                json!({
                    "source": source_path,
                    "include": ["commits"],
                    "max_items": 1,
                }),
            )
            .await
            .expect("git.digest dispatch");

        assert_eq!(resp["project_created"], json!(false), "{resp}");
        let selected_id = resp["project_id"]
            .as_str()
            .expect("project_id present")
            .to_string();
        assert_eq!(selected_id, ids[0].to_string(), "must select the oldest");

        let warnings = resp["warnings"].as_array().expect("warnings array");
        assert!(
            warnings.iter().any(|w| {
                let w = w.as_str().unwrap_or("");
                w.contains("duplicate")
                    && w.contains(&selected_id)
                    && w.contains(&ids[1].to_string())
            }),
            "duplicate warning must name the selected and duplicate ids: {warnings:?}"
        );

        // The selected anchor was backfilled with the origin-derived slug;
        // the duplicate remains pre-slug and untouched; no third anchor.
        let slugged = find_projects_by_slug(&rt, &token, "github.com/org/exact-dup-repo")
            .await
            .expect("find_projects_by_slug");
        assert_eq!(
            slugged,
            vec![ids[0]],
            "only the selected anchor gains the slug"
        );
        let still_legacy = find_projects_by_legacy_repo_url(&rt, &token, &source_path)
            .await
            .expect("find_projects_by_legacy_repo_url");
        assert_eq!(
            still_legacy,
            vec![ids[1]],
            "duplicate stays pre-slug; no third anchor minted"
        );
    }
}
