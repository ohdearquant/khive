//! `git.pr_list` and `git.issue_list`: bounded listings from the platform.

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use serde_json::{json, Value};

use super::issue::{author, labels, line};
use super::{endpoint, observed_sha, observed_string};
use crate::local_handlers::{required, validate_keys, Failure};
use crate::{credentials, GitPack};

/// Rows the platform returns per page.
#[cfg(unix)]
const PAGE: u64 = 100;

/// Pages read per call: ten pages of one hundred, newest first. A longer listing is returned up
/// to that bound, and `truncated` says whether the platform holds more.
#[cfg(unix)]
const PAGES: u64 = 10;

/// What a listing resolved to before its first listing request.
#[cfg(unix)]
struct ListTarget {
    repo: String,
    index: usize,
    slug: String,
    state: String,
    secret: credentials::Secret,
}

fn refuse(failure: Failure) -> RuntimeError {
    super::pr_state::refuse(failure)
}

#[cfg(unix)]
fn requested_state(params: &Value) -> Result<&str, RuntimeError> {
    match params.get("state") {
        None => Ok("open"),
        Some(Value::String(state)) if matches!(state.as_str(), "open" | "closed" | "all") => {
            Ok(state.as_str())
        }
        _ => Err(RuntimeError::InvalidInput("invalid_params".into())),
    }
}

#[cfg(unix)]
fn positive_number(value: &Value, field: &str) -> Result<u64, Failure> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .filter(|number| *number > 0)
        .ok_or_else(|| Failure::refused("remote_response"))
}

/// The listing request for one page. The order is named in the request, never left to the
/// platform's default.
#[cfg(unix)]
fn page_path(slug: &str, resource: &str, state: &str, per_page: u64, page: u64) -> String {
    endpoint(
        slug,
        &format!(
            "{resource}?state={state}&sort=created&direction=desc&per_page={per_page}&page={page}"
        ),
    )
}

impl GitPack {
    #[cfg(not(unix))]
    pub(crate) async fn handle_pr_list(
        &self,
        _token: &NamespaceToken,
        _registry: &VerbRegistry,
        _params: Value,
    ) -> Result<Value, RuntimeError> {
        Err(refuse(Failure::refused("actor_unmapped")))
    }

    /// Validate arguments and policy before resolving credentials or reading the platform.
    #[cfg(unix)]
    pub(crate) async fn handle_pr_list(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let target = self
            .list_target(token, registry, "git.pr_list", params)
            .await?;
        let (rows, truncated) = self
            .read_list(target.secret.value(), &target.slug, "pulls", &target.state)
            .await
            .map_err(refuse)?;
        let mut altered = false;
        let pull_requests = rows
            .iter()
            .map(|pull_request| {
                let draft = pull_request
                    .get("draft")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| Failure::refused("remote_response"))?;
                Ok(json!({
                    "number": positive_number(pull_request, "number")?,
                    "title": line(observed_string(pull_request, "/title")?, &mut altered),
                    "state": observed_string(pull_request, "/state")?,
                    "draft": draft,
                    "author": author(pull_request, "/user/login", &mut altered)?,
                    "head_ref": line(observed_string(pull_request, "/head/ref")?, &mut altered),
                    "head_sha": observed_sha(pull_request, "/head/sha")?,
                    "base_ref": line(observed_string(pull_request, "/base/ref")?, &mut altered),
                    "created_at": observed_string(pull_request, "/created_at")?,
                    "updated_at": observed_string(pull_request, "/updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, Failure>>()
            .map_err(refuse)?;
        Ok(json!({
            "repo": target.repo,
            "gate": {"decision":"allow", "source":"git_write.allowed", "id": target.index},
            "slug": target.slug,
            "state": target.state,
            "pull_requests": pull_requests,
            "truncated": truncated,
            "text_altered": altered,
            "trust": "untrusted",
        }))
    }

    #[cfg(not(unix))]
    pub(crate) async fn handle_issue_list(
        &self,
        _token: &NamespaceToken,
        _registry: &VerbRegistry,
        _params: Value,
    ) -> Result<Value, RuntimeError> {
        Err(refuse(Failure::refused("actor_unmapped")))
    }

    /// Validate arguments and policy before resolving credentials or reading the platform.
    #[cfg(unix)]
    pub(crate) async fn handle_issue_list(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let target = self
            .list_target(token, registry, "git.issue_list", params)
            .await?;
        let (rows, truncated) = self
            .read_list(target.secret.value(), &target.slug, "issues", &target.state)
            .await
            .map_err(refuse)?;
        let mut altered = false;
        let mut issues = Vec::new();
        let mut pull_requests_omitted = 0u64;
        for issue in &rows {
            match issue.get("pull_request") {
                None | Some(Value::Null) => {}
                Some(Value::Object(_)) => {
                    pull_requests_omitted += 1;
                    continue;
                }
                _ => return Err(refuse(Failure::refused("remote_response"))),
            }
            let comments = issue
                .get("comments")
                .and_then(Value::as_u64)
                .ok_or_else(|| refuse(Failure::refused("remote_response")))?;
            issues.push(json!({
                "number": positive_number(issue, "number").map_err(refuse)?,
                "title": line(observed_string(issue, "/title").map_err(refuse)?, &mut altered),
                "state": observed_string(issue, "/state").map_err(refuse)?,
                "author": author(issue, "/user/login", &mut altered).map_err(refuse)?,
                "labels": labels(issue, &mut altered).map_err(refuse)?,
                "comments": comments,
                "created_at": observed_string(issue, "/created_at").map_err(refuse)?,
                "updated_at": observed_string(issue, "/updated_at").map_err(refuse)?,
            }));
        }
        Ok(json!({
            "repo": target.repo,
            "gate": {"decision":"allow", "source":"git_write.allowed", "id": target.index},
            "slug": target.slug,
            "state": target.state,
            "issues": issues,
            "pull_requests_omitted": pull_requests_omitted,
            "truncated": truncated,
            "text_altered": altered,
            "trust": "untrusted",
        }))
    }

    /// Arguments, the repo-only allowlist match, the verb's own policy, the repository mapping and
    /// the dispatching actor's credential, in that order, before any platform request.
    #[cfg(unix)]
    async fn list_target(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        verb: &'static str,
        params: Value,
    ) -> Result<ListTarget, RuntimeError> {
        let params = crate::params::parse(verb, params)?;
        let invalid = || RuntimeError::InvalidInput("invalid_params".into());
        validate_keys(&params, &["repo", "state"]).map_err(|_| invalid())?;
        let repo = required(&params, "repo").map_err(|_| invalid())?;
        let state = requested_state(&params)?.to_owned();
        let (canonical, index) = self.read_gate(token, registry, verb, repo).await?;
        let target = self.remote_repository(&canonical).map_err(refuse)?;
        if target.slug.is_empty() {
            return Err(refuse(Failure::refused("remote_scheme")));
        }
        if target.remote.trim_end_matches(".git") != format!("https://github.com/{}", target.slug) {
            return Err(refuse(Failure::refused("repository_identity")));
        }
        let config = &self.runtime().config().git_write;
        let (_, secret) = credentials::resolve_remote(config, &token.actor().label())
            .await
            .map_err(|_| refuse(Failure::refused("actor_unmapped")))?;
        let remote = self
            .api(
                secret.value(),
                "GET",
                format!("repos/{}", target.slug),
                None,
            )
            .await
            .map_err(refuse)?;
        let matches = observed_string(&remote, "/full_name").map_err(refuse)? == target.slug
            && observed_string(&remote, "/visibility").map_err(refuse)? == target.visibility;
        if !matches {
            return Err(refuse(Failure::refused("repository_identity_mismatch")));
        }
        Ok(ListTarget {
            repo: canonical.display().to_string(),
            index,
            slug: target.slug,
            state,
            secret,
        })
    }

    /// Pages of one hundred, newest created first, until a page holds fewer than one hundred or
    /// ten pages have been read. After ten full pages one request for the single next row says
    /// whether the platform holds more.
    #[cfg(unix)]
    async fn read_list(
        &self,
        secret: &str,
        slug: &str,
        resource: &str,
        state: &str,
    ) -> Result<(Vec<Value>, bool), Failure> {
        let mut rows = Vec::new();
        for page in 1..=PAGES {
            let reply = self
                .api(
                    secret,
                    "GET",
                    page_path(slug, resource, state, PAGE, page),
                    None,
                )
                .await?;
            let batch = reply
                .as_array()
                .filter(|batch| batch.len() as u64 <= PAGE)
                .ok_or_else(|| Failure::refused("remote_response"))?;
            rows.extend(batch.iter().cloned());
            if (batch.len() as u64) < PAGE {
                return Ok((rows, false));
            }
        }
        let next = self
            .api(
                secret,
                "GET",
                page_path(slug, resource, state, 1, PAGE * PAGES + 1),
                None,
            )
            .await?;
        let next = next
            .as_array()
            .ok_or_else(|| Failure::refused("remote_response"))?;
        Ok((rows, !next.is_empty()))
    }
}
