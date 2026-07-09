//! `git.digest` verb handler (ADR-088 Amendment 1).
//!
//! Resolves the `source` argument (local path or `https://` URL, cloning/
//! fetching remote sources into the scratch cache), resolves or auto-creates
//! the repo-anchor `project` entity, then drives the shared
//! `ingest::run_ingest` core with a bounded, cursor-resumable pass.

use anyhow::anyhow;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::{SqlStatement, SqlValue};

use crate::cache;
use crate::ingest::{resolve_project_id, run_ingest, IngestInclude, IngestOptions};
use crate::source::{parse_source, repo_basename, DigestSource};
use crate::GitPack;

const DEFAULT_MAX_ITEMS: u64 = 500;
const MIN_MAX_ITEMS: u64 = 1;
const MAX_MAX_ITEMS: u64 = 2000;

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

        let max_items = params
            .get("max_items")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_ITEMS)
            .clamp(MIN_MAX_ITEMS, MAX_MAX_ITEMS);

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
        let (project_id, project_created) = match params.get("project").and_then(Value::as_str) {
            Some(raw) => {
                let id = resolve_project_id(self.runtime(), raw)
                    .await
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(format!(
                            "project {raw:?} did not resolve to an entity"
                        ))
                    })?;
                (id, false)
            }
            None => resolve_or_create_project(self.runtime(), registry, token, &source).await?,
        };

        let effective_include = IngestInclude {
            commits: include.commits,
            issues: include.issues && gh_capable,
            pull_requests: include.pull_requests && gh_capable,
        };

        let mut report = run_ingest(
            self.runtime(),
            token,
            registry,
            IngestOptions {
                repo: repo_path,
                project: project_id.to_string(),
                max_items: Some(max_items),
                include: effective_include,
            },
        )
        .await
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        report.warnings.extend(warnings);
        report.project_id = Some(project_id.to_string());
        report.project_created = project_created;

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

/// Find an existing `project` entity whose `properties.repo_url` matches the
/// source's canonical URL/path, or whose `name` matches the repo basename;
/// create the anchor when none is found (ADR-088 Amendment 1 — auto-creation
/// is reported via `IngestReport.project_created`, never silent).
async fn resolve_or_create_project(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    source: &DigestSource,
) -> Result<(Uuid, bool), RuntimeError> {
    let repo_url = match source {
        DigestSource::Local(p) => p.to_string_lossy().to_string(),
        DigestSource::Remote { canonical, .. } => canonical.clone(),
    };
    let name = repo_basename(source);

    if let Some(id) = find_project_by_repo(runtime, token, &repo_url, &name)
        .await
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?
    {
        return Ok((id, false));
    }

    let resp = registry
        .dispatch(
            "create",
            json!({
                "kind": "project",
                "name": name,
                "properties": { "repo_url": repo_url },
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
    Ok((id, true))
}

async fn find_project_by_repo(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    repo_url: &str,
    name: &str,
) -> anyhow::Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM entities WHERE kind='project' AND namespace=?1 \
                  AND deleted_at IS NULL \
                  AND (json_extract(properties,'$.repo_url')=?2 OR name=?3) \
                  LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(repo_url.to_string()),
                SqlValue::Text(name.to_string()),
            ],
            label: Some("git_digest_find_project_by_repo".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| match r.get("id") {
        Some(SqlValue::Uuid(u)) => Some(*u),
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
        _ => None,
    }))
}
