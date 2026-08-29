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
    canonical_remote_identity, parse_source, redact_repo_url, remote_url_to_slug, repo_basename,
    repo_identity, DigestSource, REPO_SLUG_PROPERTY,
};
use crate::GitPack;

/// Recover the typed error when a digest ingest/resolution failure chain
/// carries a storage-class failure (for example a reader admission timeout
/// under concurrent load), so resource exhaustion is not reported as the
/// caller's invalid input. Every other failure keeps the established
/// invalid-input shape.
fn digest_failure_to_runtime(e: anyhow::Error) -> RuntimeError {
    let e = match e.downcast::<RuntimeError>() {
        Ok(rte @ RuntimeError::Storage(_)) => return rte,
        Ok(other) => return RuntimeError::InvalidInput(other.to_string()),
        Err(e) => e,
    };
    match e.downcast::<khive_storage::StorageError>() {
        Ok(se) => RuntimeError::Storage(se),
        Err(e) => RuntimeError::InvalidInput(e.to_string()),
    }
}

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
        let max_items_requested = match params.get("max_items") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_i64().ok_or_else(|| {
                RuntimeError::InvalidInput(format!("max_items must be an integer, got {v:?}"))
            })?),
        };
        let max_items = max_items_requested
            .unwrap_or(DEFAULT_MAX_ITEMS)
            .clamp(MIN_MAX_ITEMS, MAX_MAX_ITEMS) as u64;

        let include = match params.get("include") {
            None | Some(Value::Null) => IngestInclude::default(),
            Some(v) => parse_include(v)?,
        };

        // Resolve a local repo path -- remote sources clone/fetch into the
        // scratch cache first (ADR-088 Amendment 1 §Remote-URL mode).
        let (repo_path, expected_github_repo) = match &source {
            DigestSource::Local(p) => (p.clone(), None),
            DigestSource::Remote { canonical, gh_slug } => {
                let cloned = cache::ensure_clone(canonical).map_err(|e| {
                    RuntimeError::InvalidInput(format!(
                        "remote clone/fetch of {:?} failed: {e}",
                        redact_repo_url(canonical)
                    ))
                })?;
                (
                    cloned,
                    gh_slug
                        .as_ref()
                        .map(|(owner, repo)| format!("{owner}/{repo}")),
                )
            }
        };

        // Resolve or auto-create the repo-anchor `project` entity.
        let resolution = match params.get("project").and_then(Value::as_str) {
            Some(raw) => {
                let id = resolve_project_id(self.runtime(), raw)
                    .await
                    .map_err(digest_failure_to_runtime)?
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

        let opts = IngestOptions {
            repo: repo_path,
            expected_github_repo,
            project: project_id.to_string(),
            max_items: Some(max_items),
            // Preserve the caller's requested sources all the way into the
            // ingest core. Its source-bound `gh repo view` probe records an
            // unusable/non-GitHub source as `Skipped` + `gh_available:false`;
            // masking these bits here would instead make the source look
            // unrequested and silently fabricate completeness (#1617).
            include,
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
        .map_err(digest_failure_to_runtime)?;

        if !resolution.slug_duplicates.is_empty() {
            report.warnings.push(format!(
                "multiple live project anchors resolve to the same repo identity; selected {} by canonical resolution order; duplicate or conflicting anchors: {}",
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
        report.max_items_requested = max_items_requested;
        report.max_items_effective = Some(max_items);
        if let Some(requested) = max_items_requested {
            if requested != max_items as i64 {
                report.warnings.push(format!(
                    "max_items request {requested} was clamped to {max_items}"
                ));
            }
        }
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
    /// Additional live anchors whose exact slug or normalized `repo_url`
    /// resolves to the same identity as the deterministically selected one
    /// (ADR-088 Amendment 2, #1708) — surfaced as a report warning, never
    /// silent.
    pub(crate) slug_duplicates: Vec<Uuid>,
}

pub(crate) struct OrphanSignal {
    pub(crate) dead_project_id: Uuid,
    pub(crate) annotated_note_count: u64,
}

/// Find an existing `project` entity whose `properties.repo_slug` matches
/// the source's canonical repo identity (issue #1173), falling back to a
/// normalized `properties.repo_url` evidence for anchors whose slug is
/// absent or non-canonical (backfilling the canonical slug on a selected
/// match so future calls converge without a migration); create the anchor
/// when neither matches (ADR-088 Amendment 1 — auto-creation is reported via
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
        .map_err(digest_failure_to_runtime)?;
    if let Some((id, duplicates)) = slug_matches.split_first() {
        let selected = *id;
        // issue #6 item 2: a step-1 slug match used to return immediately
        // without consulting URL-equivalent anchors outside that slug tier.
        // #1708 extends that diagnostic to a present but non-canonical slug.
        // Selection is unchanged (the canonical slug tier still wins); this
        // only widens what gets reported alongside it.
        let mut slug_duplicates = duplicates.to_vec();
        append_unique_ids(
            &mut slug_duplicates,
            find_normalized_noncanonical_matches(runtime, token, &identity)
                .await
                .map_err(digest_failure_to_runtime)?
                .into_iter()
                .map(|(candidate, _)| candidate)
                .filter(|candidate| *candidate != selected),
        );
        return Ok(ProjectResolution {
            id: selected,
            created: false,
            orphan: None,
            slug_duplicates,
        });
    }

    let exact_matches = find_projects_by_legacy_repo_url(runtime, token, &repo_url)
        .await
        .map_err(digest_failure_to_runtime)?;
    if let Some((id, duplicates)) = exact_matches.split_first() {
        let id = *id;
        crate::dispatch_from_token(
            registry,
            token,
            "update",
            json!({
                    "id": id.to_string(),
                    // Backfill hits also redact the stored repo_url in the
                    // same patch (ADR-088 Amendment 2 step 2) -- the
                    // lazy-upgrade path closes out any credential-bearing
                    // legacy URL it touches.
                    "properties": {
                        REPO_SLUG_PROPERTY: identity.clone(),
                        "repo_url": redact_repo_url(&repo_url),
                    },
            }),
        )
        .await?;
        let mut slug_duplicates = duplicates.to_vec();
        append_unique_ids(
            &mut slug_duplicates,
            find_normalized_noncanonical_matches(runtime, token, &identity)
                .await
                .map_err(digest_failure_to_runtime)?
                .into_iter()
                .map(|(candidate, _)| candidate)
                .filter(|candidate| *candidate != id),
        );
        return Ok(ProjectResolution {
            id,
            created: false,
            orphan: None,
            slug_duplicates,
        });
    }

    // ADR-088 Amendment 2 step 2 plus #1708: re-derive identity from every
    // live anchor whose stored slug is absent or non-canonical. Normalized
    // repo_url evidence can therefore repair and reuse a hand-written or
    // stale slug instead of excluding the anchor and minting a duplicate.
    let normalized_matches = find_normalized_noncanonical_matches(runtime, token, &identity)
        .await
        .map_err(digest_failure_to_runtime)?;
    if let Some(((selected, selected_repo_url), duplicates)) = normalized_matches.split_first() {
        let selected = *selected;
        crate::dispatch_from_token(
            registry,
            token,
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
        .map_err(digest_failure_to_runtime)?;

    let resp = crate::dispatch_from_token(
        registry,
        token,
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

fn append_unique_ids(target: &mut Vec<Uuid>, candidates: impl IntoIterator<Item = Uuid>) {
    for candidate in candidates {
        if !target.contains(&candidate) {
            target.push(candidate);
        }
    }
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
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
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
        .map_err(anyhow::Error::new)?;
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
/// Anchors that already carry `repo_slug` are excluded from this raw-string
/// route: canonical ones are found by step 1, while differing slugs require
/// normalized URL evidence below. Raw equality alone never overwrites an
/// already-derived identity.
async fn find_projects_by_legacy_repo_url(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    repo_url: &str,
) -> anyhow::Result<Vec<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
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
        .map_err(anyhow::Error::new)?;
    Ok(rows
        .iter()
        .filter_map(|r| match r.get("id") {
            Some(SqlValue::Uuid(u)) => Some(*u),
            Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
            _ => None,
        })
        .collect())
}

/// Fetch every live `project` anchor whose slug is absent or differs from
/// the source identity. These are candidates for step-2 normalized URL
/// reconciliation; exact canonical-slug anchors remain exclusively in the
/// higher-precedence step-1 lookup. SQL ordering makes candidate selection
/// deterministic before the async normalization pass (#1708).
async fn find_projects_without_canonical_slug(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &str,
) -> anyhow::Result<Vec<(Uuid, String)>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id, json_extract(properties,'$.repo_url') AS repo_url \
                  FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NULL \
                  AND json_extract(properties,'$.repo_url') IS NOT NULL \
                  AND (json_extract(properties,'$.repo_slug') IS NULL \
                       OR json_extract(properties,'$.repo_slug')<>?2) \
                  ORDER BY created_at ASC, id ASC"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(identity.to_string()),
            ],
            label: Some("git_digest_find_projects_without_canonical_slug".into()),
        })
        .await
        .map_err(anyhow::Error::new)?;
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

/// Fetch every soft-deleted anchor outside the canonical slug tier and carry
/// each row's own `deleted_at` so callers can normalize, deduplicate, and
/// merge-sort tombstones by recency alongside an exact-match set. This is the
/// tombstone counterpart of the live step-2 candidate scan: both absent and
/// present-but-noncanonical slugs remain URL-reconciliation candidates.
async fn find_soft_deleted_projects_without_canonical_slug(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &str,
) -> anyhow::Result<Vec<(Uuid, String, i64)>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id, deleted_at, json_extract(properties,'$.repo_url') AS repo_url \
                  FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NOT NULL \
                  AND json_extract(properties,'$.repo_url') IS NOT NULL \
                  AND (json_extract(properties,'$.repo_slug') IS NULL \
                       OR json_extract(properties,'$.repo_slug')<>?2)"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(identity.to_string()),
            ],
            label: Some("git_digest_find_soft_deleted_projects_without_canonical_slug".into()),
        })
        .await
        .map_err(anyhow::Error::new)?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let id = match r.get("id") {
                Some(SqlValue::Uuid(u)) => Some(*u),
                Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
                _ => None,
            }?;
            let deleted_at = match r.get("deleted_at") {
                Some(SqlValue::Integer(n)) => *n,
                _ => 0,
            };
            let url = match r.get("repo_url") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            }?;
            Some((id, url, deleted_at))
        })
        .collect())
}

/// Return live anchors outside the canonical slug tier whose own stored
/// `repo_url` normalizes to the caller's identity. This includes both legacy
/// pre-slug rows and #1708's present-but-noncanonical slug rows. Candidate
/// ordering is retained so normalized-only selection remains oldest-first.
async fn find_normalized_noncanonical_matches(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    identity: &str,
) -> anyhow::Result<Vec<(Uuid, String)>> {
    let candidates = find_projects_without_canonical_slug(runtime, token, identity).await?;
    let mut matches = Vec::new();
    for (id, candidate_repo_url) in candidates {
        if normalize_stored_repo_url(&candidate_repo_url)
            .await
            .as_deref()
            == Some(identity)
        {
            matches.push((id, candidate_repo_url));
        }
    }
    Ok(matches)
}

/// Re-derive the repo-identity slug an anchor's stored `repo_url` resolves to
/// today (ADR-088 Amendment 2 step 2). A sluggable remote spelling normalizes
/// directly via `remote_url_to_slug`; an accepted but unsluggable HTTPS source
/// reproduces `repo_identity(Remote)`'s shared credential-redacted,
/// slash/`.git`-normalized URL fallback. A
/// path-shaped value (an absolute local path, stored verbatim by the pre-#1173
/// local-source resolution path) is treated as a local clone and resolved the
/// same way `repo_identity` resolves a `DigestSource::Local` -- via its current
/// `origin` remote -- so a legacy local-path anchor reconciles with a later
/// remote-URL digest of the same repository. A remote-less local path's
/// `local:<canonical-path>` fallback is itself canonical evidence and is
/// retained for equality comparison. Returns `None` when the stored value is
/// neither a recognized remote spelling, an accepted HTTPS source, nor an
/// absolute local path.
async fn normalize_stored_repo_url(repo_url: &str) -> Option<String> {
    if let Some(slug) = remote_url_to_slug(repo_url) {
        return Some(slug);
    }
    if repo_url.trim().starts_with("https://") {
        let candidate = parse_source(repo_url).ok()?;
        if let DigestSource::Remote { canonical, .. } = candidate {
            return Some(canonical_remote_identity(&canonical));
        }
    }
    if repo_url.starts_with('/') {
        let candidate = DigestSource::Local(std::path::PathBuf::from(repo_url));
        return Some(repo_identity(&candidate).await);
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
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id, deleted_at FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NOT NULL \
                  AND (json_extract(properties,'$.repo_slug')=?2 \
                       OR json_extract(properties,'$.repo_url')=?3)"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(identity.to_string()),
                SqlValue::Text(repo_url.to_string()),
            ],
            label: Some("git_digest_find_orphaned_anchor".into()),
        })
        .await
        .map_err(anyhow::Error::new)?;
    let mut dead_projects: Vec<(Uuid, i64)> = rows
        .iter()
        .filter_map(|r| {
            let id = match r.get("id") {
                Some(SqlValue::Uuid(u)) => Some(*u),
                Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
                _ => None,
            }?;
            let deleted_at = match r.get("deleted_at") {
                Some(SqlValue::Integer(n)) => *n,
                _ => 0,
            };
            Some((id, deleted_at))
        })
        .collect();

    // issue #6 item 1: the exact-match scan above misses a soft-deleted
    // legacy anchor whose stored repo_url is an alternate spelling of the
    // same repository (one that would normalize to the same identity).
    // Extend the tombstone scan with the same normalize-and-compare step
    // the live step-2 path already uses, scoped to soft-deleted rows.
    let normalized_tombstones =
        find_soft_deleted_projects_without_canonical_slug(runtime, token, identity).await?;
    let mut already_matched: std::collections::HashSet<Uuid> =
        dead_projects.iter().map(|(id, _)| *id).collect();
    for (id, candidate_repo_url, deleted_at) in normalized_tombstones {
        if !already_matched.insert(id) {
            continue;
        }
        if normalize_stored_repo_url(&candidate_repo_url)
            .await
            .as_deref()
            == Some(identity)
        {
            dead_projects.push((id, deleted_at));
        }
    }
    // Newest-deleted-first across both sources: the signal below fires for
    // the first matching tombstone (newest first) that still has a live
    // annotating corpus, per the doc comment above.
    dead_projects.sort_by_key(|(_, deleted_at)| std::cmp::Reverse(*deleted_at));
    let dead_project_ids = dead_projects.into_iter().map(|(id, _)| id);

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
            .map_err(anyhow::Error::new)?;
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
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(rt.clone()));
        builder.register(GitPack::new(rt.clone()));
        builder.with_event_store(rt.events(&token).expect("event store"));
        let registry = builder.build().expect("registry builds");
        rt.install_edge_rules(registry.all_edge_rules());
        registry.apply_schema_plans(rt.backend());
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

    /// Initialize a remote-less repository with `user.*` configured and one
    /// commit. `git log` (and thus `git.digest`) needs a real commit to walk;
    /// a freshly-initialized repo with zero commits fails before anchor
    /// resolution is exercised.
    fn init_repo_with_one_commit(dir: &Path) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q"])
            .status()
            .expect("spawn git");
        assert!(status.success(), "git init failed");
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

    fn init_repo_with_origin_and_one_commit(dir: &Path, origin: &str) {
        init_repo_with_one_commit(dir);
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["remote", "add", "origin", origin])
            .status()
            .expect("spawn git");
        assert!(status.success(), "git remote add failed");
    }

    async fn dispatch_local_commit_digest(registry: &VerbRegistry, dir: &Path) -> Value {
        registry
            .dispatch(
                "git.digest",
                json!({
                    "source": dir.to_string_lossy(),
                    "include": ["commits"],
                    "max_items": 1,
                }),
            )
            .await
            .expect("git.digest dispatch")
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
        let duplicate_id = ids[1].to_string();
        let warning = warnings
            .iter()
            .filter_map(Value::as_str)
            .find(|warning| {
                warning.contains("duplicate")
                    && warning.contains(&selected_id)
                    && warning.contains(&duplicate_id)
            })
            .unwrap_or_else(|| {
                panic!("duplicate warning must name selected and duplicate ids: {warnings:?}")
            });
        assert_eq!(
            warning.matches(&duplicate_id).count(),
            1,
            "cross-route aggregation must not repeat an anchor id: {warning}"
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
        let duplicate_id = ids[1].to_string();
        let warning = warnings
            .iter()
            .filter_map(Value::as_str)
            .find(|warning| {
                warning.contains("duplicate")
                    && warning.contains(&selected_id)
                    && warning.contains(&duplicate_id)
            })
            .unwrap_or_else(|| {
                panic!("duplicate warning must name selected and duplicate ids: {warnings:?}")
            });
        assert_eq!(
            warning.matches(&duplicate_id).count(),
            1,
            "exact and normalized evidence must not repeat an anchor id: {warning}"
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

    /// Issue #6 item 1: a soft-deleted legacy (pre-slug) anchor whose stored
    /// `repo_url` is an alternate spelling (here, a `.git` suffix) of the
    /// same repository must still be detected by the orphan scan -- the
    /// exact-match tombstone query alone would miss it.
    #[tokio::test]
    async fn orphaned_anchor_with_alternate_spelling_repo_url_is_still_flagged() {
        let (rt, token, registry) = fixture().await;

        let source = DigestSource::Remote {
            canonical: "https://github.com/org/normalized-orphan-repo".to_string(),
            gh_slug: Some(("org".to_string(), "normalized-orphan-repo".to_string())),
        };

        let dead = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "normalized-orphan-repo",
                    "properties": {
                        "repo_url": "https://github.com/org/normalized-orphan-repo.git",
                    },
                }),
            )
            .await
            .expect("create dead anchor");
        let dead_id = Uuid::parse_str(dead["id"].as_str().unwrap()).expect("uuid");

        create_note_annotating(&registry, "commit", "c1", dead_id).await;

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
            "no live anchor matches -- a fresh one is minted"
        );
        let orphan = resolution.orphan.expect(
            "issue #6: an alternate-spelling soft-deleted anchor must still be flagged as orphan",
        );
        assert_eq!(orphan.dead_project_id, dead_id);
        assert_eq!(orphan.annotated_note_count, 1);
    }

    /// #1708 public-surface tombstone regression: normalized orphan lookup
    /// must include a soft-deleted anchor whose slug is present but differs
    /// from the canonical identity. The wire report must identify its live
    /// corpus rather than silently creating a replacement anchor beside it.
    #[tokio::test]
    async fn git_digest_reports_noncanonical_slug_tombstone_orphan() {
        let (rt, token, registry) = fixture().await;
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_origin_and_one_commit(
            dir.path(),
            "https://github.com/org/noncanonical-tombstone-repo",
        );

        // Seed below the public secret gate: this row represents legacy data
        // that predates URL-userinfo admission checks. The behavior under test
        // is safe reconciliation of that already-persisted evidence, not
        // permission for a caller to create it today.
        let dead = khive_storage::Entity::new(
            "local",
            "project",
            "noncanonical-tombstone-repo-old",
        )
        .with_properties(json!({
            "repo_url": "  https://legacy:token@github.com/org/noncanonical-tombstone-repo.git?view=old  ",
            "repo_slug": "org/noncanonical-tombstone-repo",
        }));
        let dead_id = dead.id;
        rt.entities(&token)
            .expect("entity store")
            .upsert_entity(dead)
            .await
            .expect("seed legacy noncanonical tombstone anchor");
        create_note_annotating(&registry, "issue", "#1708 orphan", dead_id).await;
        assert!(rt
            .delete_entity(&token, dead_id, false)
            .await
            .expect("soft delete"));

        let response = dispatch_local_commit_digest(&registry, dir.path()).await;
        assert_eq!(response["project_created"], json!(true), "{response}");
        assert_eq!(
            response["orphaned_corpus_detected"],
            json!(true),
            "{response}"
        );
        assert_eq!(
            response["orphaned_project_id"],
            json!(dead_id.to_string()),
            "{response}"
        );
        assert_eq!(response["orphaned_note_count"], json!(1), "{response}");
    }

    /// Issue #6 item 2: when a step-1 slug match wins resolution, a live
    /// legacy (pre-slug) anchor for an alternate spelling of the same
    /// repository must still surface in `slug_duplicates` -- previously,
    /// resolution returned at step 1 without ever consulting legacy anchors.
    #[tokio::test]
    async fn slug_tier_match_surfaces_cross_tier_legacy_duplicate() {
        let (rt, token, registry) = fixture().await;

        let source = DigestSource::Remote {
            canonical: "https://github.com/org/cross-tier-repo".to_string(),
            gh_slug: Some(("org".to_string(), "cross-tier-repo".to_string())),
        };

        let winner = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "cross-tier-repo",
                    "properties": {
                        "repo_url": "https://github.com/org/cross-tier-repo",
                        "repo_slug": "github.com/org/cross-tier-repo",
                    },
                }),
            )
            .await
            .expect("create winner anchor");
        let winner_id = Uuid::parse_str(winner["id"].as_str().unwrap()).expect("uuid");

        // A LIVE legacy (pre-slug) anchor, alternate `.git`-suffixed
        // spelling of the same repo -- a different resolution tier than
        // the step-1 winner above.
        let legacy = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "cross-tier-repo-legacy",
                    "properties": {
                        "repo_url": "https://github.com/org/cross-tier-repo.git",
                    },
                }),
            )
            .await
            .expect("create legacy anchor");
        let legacy_id = Uuid::parse_str(legacy["id"].as_str().unwrap()).expect("uuid");

        let resolution = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve");
        assert_eq!(resolution.id, winner_id, "slug tier still wins selection");
        assert!(!resolution.created);
        assert!(
            resolution.slug_duplicates.contains(&legacy_id),
            "issue #6: a live legacy-tier anchor for the same identity must surface \
             as a cross-tier duplicate; got {:?}",
            resolution.slug_duplicates
        );
    }

    /// #1708: a present but non-canonical slug used to exclude an otherwise
    /// matching anchor from every resolution tier. Normalized `repo_url`
    /// evidence must repair and reuse that anchor, including same-patch
    /// redaction of its stored display URL.
    #[tokio::test]
    async fn noncanonical_slug_anchor_is_repaired_reused_and_redacted() {
        let (rt, token, registry) = fixture().await;
        let source = DigestSource::Remote {
            canonical: "https://github.com/org/noncanonical-repo".to_string(),
            gh_slug: Some(("org".to_string(), "noncanonical-repo".to_string())),
        };
        let existing = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "noncanonical-repo",
                    "properties": {
                        "repo_url": "  legacy-token-user@github.com:org/noncanonical-repo.git?view=compact#top  ",
                        "repo_slug": "org/noncanonical-repo",
                    },
                }),
            )
            .await
            .expect("create non-canonical anchor");
        let existing_id = Uuid::parse_str(existing["id"].as_str().unwrap()).expect("uuid");

        let resolution = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve non-canonical anchor");
        assert_eq!(resolution.id, existing_id);
        assert!(
            !resolution.created,
            "matching URL evidence must prevent create"
        );
        assert!(resolution.slug_duplicates.is_empty());

        let repaired = rt
            .get_entity(&token, existing_id)
            .await
            .expect("repaired anchor remains readable");
        let properties = repaired.properties.as_ref().expect("properties present");
        assert_eq!(
            properties.get("repo_slug").and_then(Value::as_str),
            Some("github.com/org/noncanonical-repo")
        );
        assert_eq!(
            properties.get("repo_url").and_then(Value::as_str),
            Some("github.com:org/noncanonical-repo.git"),
            "the full repair path must redact SCP-style userinfo"
        );

        let again = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve repaired anchor again");
        assert_eq!(again.id, existing_id);
        assert!(!again.created);
        assert_eq!(
            find_projects_by_slug(&rt, &token, "github.com/org/noncanonical-repo")
                .await
                .expect("canonical lookup"),
            vec![existing_id]
        );
    }

    /// #1708 public-surface regression: an older URL-equivalent anchor with
    /// a conflicting slug must not displace the canonical slug winner, but it
    /// must be named in the `git.digest` warning rather than silently hidden.
    #[tokio::test]
    async fn git_digest_warns_for_noncanonical_slug_conflict_with_canonical_winner() {
        let (rt, token, registry) = fixture().await;
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_origin_and_one_commit(
            dir.path(),
            "https://github.com/org/conflicting-slug-repo",
        );

        let conflicting = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "conflicting-slug-repo-old",
                    "properties": {
                        "repo_url": "https://github.com/org/conflicting-slug-repo.git",
                        "repo_slug": "org/conflicting-slug-repo",
                    },
                }),
            )
            .await
            .expect("create conflicting anchor");
        let conflicting_id = Uuid::parse_str(conflicting["id"].as_str().unwrap()).expect("uuid");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let winner = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "conflicting-slug-repo",
                    "properties": {
                        "repo_url": "https://github.com/org/conflicting-slug-repo",
                        "repo_slug": "github.com/org/conflicting-slug-repo",
                    },
                }),
            )
            .await
            .expect("create canonical winner");
        let winner_id = Uuid::parse_str(winner["id"].as_str().unwrap()).expect("uuid");

        let response = registry
            .dispatch(
                "git.digest",
                json!({
                    "source": dir.path().to_string_lossy(),
                    "include": ["commits"],
                    "max_items": 1,
                }),
            )
            .await
            .expect("git.digest dispatch");

        assert_eq!(response["project_created"], json!(false), "{response}");
        assert_eq!(
            response["project_id"],
            json!(winner_id.to_string()),
            "canonical slug tier must win even when its anchor is newer"
        );
        let warning = response["warnings"]
            .as_array()
            .expect("warnings array")
            .iter()
            .filter_map(Value::as_str)
            .find(|warning| warning.contains(&conflicting_id.to_string()))
            .expect("conflicting anchor must reach the public warning");
        assert!(warning.contains(&winner_id.to_string()), "{warning}");
        assert!(warning.contains("canonical resolution order"), "{warning}");
        assert!(
            warning.contains("duplicate or conflicting anchors"),
            "{warning}"
        );

        assert_eq!(
            find_projects_by_slug(&rt, &token, "github.com/org/conflicting-slug-repo")
                .await
                .expect("canonical lookup"),
            vec![winner_id],
            "warning must not rewrite or mint another canonical anchor"
        );
        let conflicting = rt
            .get_entity(&token, conflicting_id)
            .await
            .expect("conflicting anchor remains readable");
        assert_eq!(
            conflicting
                .properties
                .as_ref()
                .and_then(|properties| properties.get("repo_slug"))
                .and_then(Value::as_str),
            Some("org/conflicting-slug-repo"),
            "a canonical winner makes the conflict diagnostic-only"
        );
    }

    /// #1708 remote-less-local regression: `local:<canonical-path>` is a real
    /// canonical identity, not a failed normalization. A noncanonical slug on
    /// the same stored path must be repaired and reused through `git.digest`.
    #[tokio::test]
    async fn git_digest_repairs_noncanonical_slug_for_remote_less_local_repo() {
        let (rt, token, registry) = fixture().await;
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_one_commit(dir.path());
        let path = dir.path().to_string_lossy().to_string();
        let identity = repo_identity(&DigestSource::Local(dir.path().to_path_buf())).await;
        assert!(identity.starts_with("local:"), "{identity}");

        let existing = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "remote-less-local",
                    "properties": {
                        "repo_url": path,
                        "repo_slug": "hand-written-local-slug",
                    },
                }),
            )
            .await
            .expect("create noncanonical local anchor");
        let existing_id = Uuid::parse_str(existing["id"].as_str().unwrap()).expect("uuid");

        let response = dispatch_local_commit_digest(&registry, dir.path()).await;
        assert_eq!(response["project_created"], json!(false), "{response}");
        assert_eq!(response["project_id"], json!(existing_id.to_string()));
        assert_eq!(
            find_projects_by_slug(&rt, &token, &identity)
                .await
                .expect("canonical lookup"),
            vec![existing_id],
            "the repaired anchor must be the sole canonical winner"
        );
        let repaired = rt
            .get_entity(&token, existing_id)
            .await
            .expect("repaired anchor remains readable");
        assert_eq!(
            repaired
                .properties
                .as_ref()
                .and_then(|properties| properties.get("repo_slug"))
                .and_then(Value::as_str),
            Some(identity.as_str())
        );
    }

    /// #1708 remote-less-local conflict regression: an exact canonical slug
    /// winner keeps precedence, while an older same-path anchor with a
    /// conflicting slug reaches the public warning and remains unchanged.
    #[tokio::test]
    async fn git_digest_warns_for_remote_less_local_noncanonical_slug_conflict() {
        let (rt, token, registry) = fixture().await;
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_one_commit(dir.path());
        let path = dir.path().to_string_lossy().to_string();
        let identity = repo_identity(&DigestSource::Local(dir.path().to_path_buf())).await;
        assert!(identity.starts_with("local:"), "{identity}");

        let conflicting = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "remote-less-local-old",
                    "properties": {
                        "repo_url": path.clone(),
                        "repo_slug": "hand-written-local-slug",
                    },
                }),
            )
            .await
            .expect("create conflicting local anchor");
        let conflicting_id = Uuid::parse_str(conflicting["id"].as_str().unwrap()).expect("uuid");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let winner = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "remote-less-local",
                    "properties": {
                        "repo_url": path,
                        "repo_slug": identity.clone(),
                    },
                }),
            )
            .await
            .expect("create canonical local anchor");
        let winner_id = Uuid::parse_str(winner["id"].as_str().unwrap()).expect("uuid");

        let response = dispatch_local_commit_digest(&registry, dir.path()).await;
        assert_eq!(response["project_created"], json!(false), "{response}");
        assert_eq!(response["project_id"], json!(winner_id.to_string()));
        let warning = response["warnings"]
            .as_array()
            .expect("warnings array")
            .iter()
            .filter_map(Value::as_str)
            .find(|warning| warning.contains(&conflicting_id.to_string()))
            .expect("conflicting local anchor must reach the public warning");
        assert!(warning.contains(&winner_id.to_string()), "{warning}");
        assert!(warning.contains("canonical resolution order"), "{warning}");
        assert_eq!(
            find_projects_by_slug(&rt, &token, &identity)
                .await
                .expect("canonical lookup"),
            vec![winner_id],
            "warning must not rewrite the conflicting row"
        );
        let conflicting = rt
            .get_entity(&token, conflicting_id)
            .await
            .expect("conflicting anchor remains readable");
        assert_eq!(
            conflicting
                .properties
                .as_ref()
                .and_then(|properties| properties.get("repo_slug"))
                .and_then(Value::as_str),
            Some("hand-written-local-slug")
        );
    }

    /// #1708 accepted-unsluggable-remote regression: a single-segment HTTPS
    /// path uses `repo_identity(Remote)`'s redacted-URL fallback. Stored URL
    /// normalization must reproduce that identity and repair a present but
    /// noncanonical slug rather than minting another anchor.
    #[tokio::test]
    async fn unsluggable_https_noncanonical_slug_anchor_is_repaired_and_reused() {
        let (rt, token, registry) = fixture().await;
        let source =
            parse_source("https://source-user@example.com/repo?view=source#source-fragment")
                .expect("accepted unsluggable HTTPS source");
        let identity = repo_identity(&source).await;
        assert_eq!(identity, "https://example.com/repo");

        let existing = registry
            .dispatch(
                "create",
                json!({
                    "kind": "project",
                    "name": "unsluggable-remote",
                    "properties": {
                        "repo_url": "  https://legacy-user@example.com/repo.git/?view=legacy#legacy-fragment  ",
                        "repo_slug": "example.com/repo",
                    },
                }),
            )
            .await
            .expect("create noncanonical unsluggable anchor");
        let existing_id = Uuid::parse_str(existing["id"].as_str().unwrap()).expect("uuid");

        let resolution = resolve_or_create_project(&rt, &registry, &token, &source)
            .await
            .expect("resolve accepted unsluggable source");
        assert_eq!(resolution.id, existing_id);
        assert!(!resolution.created, "matching fallback identity must reuse");
        assert!(resolution.slug_duplicates.is_empty());

        let repaired = rt
            .get_entity(&token, existing_id)
            .await
            .expect("repaired anchor remains readable");
        let properties = repaired.properties.as_ref().expect("properties present");
        assert_eq!(
            properties.get("repo_slug").and_then(Value::as_str),
            Some(identity.as_str())
        );
        assert_eq!(
            properties.get("repo_url").and_then(Value::as_str),
            Some("https://example.com/repo.git/"),
            "repair must redact the display URL without replacing it with the identity fallback"
        );
        assert_eq!(
            find_projects_by_slug(&rt, &token, &identity)
                .await
                .expect("canonical lookup"),
            vec![existing_id]
        );
    }

    #[tokio::test]
    async fn stored_unsluggable_https_equivalent_spellings_share_fallback_identity() {
        for spelling in [
            "https://legacy@example.com/repo.git?view=old",
            "https://example.com/repo/?view=old#fragment",
            "https://example.com/repo.git/?view=old",
        ] {
            assert_eq!(
                normalize_stored_repo_url(spelling).await.as_deref(),
                Some("https://example.com/repo"),
                "stored equivalent spelling must reproduce the live fallback: {spelling:?}"
            );
        }
    }

    #[tokio::test]
    async fn stored_remote_fallback_rejects_non_source_shapes() {
        for malformed in ["relative/repo", "user@example.com/repo", "https://"] {
            assert_eq!(
                normalize_stored_repo_url(malformed).await,
                None,
                "arbitrary malformed value must not become identity evidence: {malformed}"
            );
        }
    }

    #[test]
    fn digest_failure_preserves_storage_class_and_flattens_the_rest() {
        // A storage-class failure inside the ingest chain (here: a
        // writer-handle admission timeout under load) must surface typed,
        // not as the caller's invalid input.
        let admission = anyhow::Error::new(RuntimeError::Storage(
            khive_storage::StorageError::AdmissionTimeout {
                operation: "sql_bridge.writer_handle".into(),
                timeout_ms: 30_000,
            },
        ));
        let recovered = digest_failure_to_runtime(admission);
        assert!(
            matches!(
                &recovered,
                RuntimeError::Storage(khive_storage::StorageError::AdmissionTimeout {
                    operation,
                    timeout_ms: 30_000,
                }) if operation.as_ref() == "sql_bridge.writer_handle"
            ),
            "storage-class ingest failure must stay typed; got {recovered:?}"
        );

        // A bare StorageError (no RuntimeError wrapper) is recovered too.
        let bare = anyhow::Error::new(khive_storage::StorageError::Timeout {
            operation: "search".into(),
        });
        assert!(matches!(
            digest_failure_to_runtime(bare),
            RuntimeError::Storage(khive_storage::StorageError::Timeout { .. })
        ));

        // Non-storage runtime failures keep the established invalid-input
        // shape, message intact.
        let not_found = anyhow::Error::new(RuntimeError::NotFound("proj-x".to_string()));
        match digest_failure_to_runtime(not_found) {
            RuntimeError::InvalidInput(msg) => assert!(msg.contains("proj-x"), "{msg}"),
            other => panic!("non-storage failure must flatten to InvalidInput, got {other:?}"),
        }

        // Plain anyhow context errors keep the established shape as well.
        match digest_failure_to_runtime(anyhow!("gh probe failed")) {
            RuntimeError::InvalidInput(msg) => assert_eq!(msg, "gh probe failed"),
            other => panic!("untyped failure must flatten to InvalidInput, got {other:?}"),
        }
    }
}
