//! `git.issue`: one issue and its discussion as reported by the platform.

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use serde_json::{json, Value};

use super::{endpoint, number, observed_string};
use crate::local_handlers::{required, validate_keys, Failure};
use crate::{credentials, GitPack};

/// Comment pages read per call: ten pages of one hundred. A longer discussion is returned up to
/// that bound, and `comments_truncated` beside `comments_total` says so.
#[cfg(unix)]
const COMMENT_PAGES: u64 = 10;

fn refuse(failure: Failure) -> RuntimeError {
    super::pr_state::refuse(failure)
}

/// Text that may span lines (a body), cleaned as `git.check_log` cleans a log: line breaks and
/// tabs kept, terminal escapes and the hidden characters `git.pr_state` removes taken out.
/// Nothing is cut. `altered` is set when anything was removed.
#[cfg(unix)]
fn lines(text: &str, altered: &mut bool) -> String {
    let (cleaned, changed) = super::check_log::clean_log(text.as_bytes());
    if changed {
        *altered = true;
    }
    cleaned
}

/// One-line text (a title, a label, a login) with the hidden characters removed.
#[cfg(unix)]
fn line(text: &str, altered: &mut bool) -> String {
    let kept: String = text
        .chars()
        .filter(|c| !super::pr_state::hidden(*c))
        .collect();
    if kept != text {
        *altered = true;
    }
    kept
}

#[cfg(unix)]
fn body(value: &Value, pointer: &str, altered: &mut bool) -> Result<String, Failure> {
    match value.pointer(pointer) {
        Some(Value::String(body)) => Ok(lines(body, altered)),
        Some(Value::Null) | None => Ok(String::new()),
        _ => Err(Failure::refused("remote_response")),
    }
}

#[cfg(unix)]
fn author(value: &Value, pointer: &str, altered: &mut bool) -> Result<Value, Failure> {
    match value.pointer(pointer) {
        Some(Value::String(login)) => Ok(json!(line(login, altered))),
        Some(Value::Null) | None => Ok(Value::Null),
        _ => Err(Failure::refused("remote_response")),
    }
}

#[cfg(unix)]
fn labels(value: &Value, altered: &mut bool) -> Result<Vec<String>, Failure> {
    let labels = value
        .pointer("/labels")
        .and_then(Value::as_array)
        .ok_or_else(|| Failure::refused("remote_response"))?;
    labels
        .iter()
        .map(|label| observed_string(label, "/name").map(|name| line(name, altered)))
        .collect()
}

impl GitPack {
    #[cfg(not(unix))]
    pub(crate) async fn handle_issue(
        &self,
        _token: &NamespaceToken,
        _registry: &VerbRegistry,
        _params: Value,
    ) -> Result<Value, RuntimeError> {
        Err(refuse(Failure::refused("actor_unmapped")))
    }

    /// Validate arguments and policy before resolving credentials or reading the platform.
    #[cfg(unix)]
    pub(crate) async fn handle_issue(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let params = crate::params::parse("git.issue", params)?;
        let invalid = || RuntimeError::InvalidInput("invalid_params".into());
        validate_keys(&params, &["repo", "number"]).map_err(|_| invalid())?;
        let repo = required(&params, "repo").map_err(|_| invalid())?;
        let n = number(&params).map_err(|_| invalid())?;
        let (canonical, index) = self.read_gate(token, registry, "git.issue", repo).await?;
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
        let issue = self
            .read_issue(secret.value(), &target.slug, &target.visibility, n)
            .await
            .map_err(refuse)?;
        let mut result = json!({
            "repo": canonical.display().to_string(),
            "gate": {"decision":"allow", "source":"git_write.allowed", "id": index},
            "slug": target.slug,
            "number": n,
        });
        if let (Some(result), Value::Object(issue)) = (result.as_object_mut(), issue) {
            result.extend(issue);
        }
        Ok(result)
    }

    #[cfg(unix)]
    async fn read_issue(
        &self,
        secret: &str,
        slug: &str,
        visibility: &str,
        n: u64,
    ) -> Result<Value, Failure> {
        let remote = self
            .api(secret, "GET", format!("repos/{slug}"), None)
            .await?;
        if observed_string(&remote, "/full_name")? != slug
            || observed_string(&remote, "/visibility")? != visibility
        {
            return Err(Failure::refused("repository_identity_mismatch"));
        }

        let issue = self
            .api(secret, "GET", endpoint(slug, &format!("issues/{n}")), None)
            .await?;
        let comments_total = issue
            .pointer("/comments")
            .and_then(Value::as_u64)
            .ok_or_else(|| Failure::refused("remote_response"))?;
        let mut altered = false;
        let labels = labels(&issue, &mut altered)?;
        let is_pull_request = match issue.get("pull_request") {
            None | Some(Value::Null) => false,
            Some(Value::Object(_)) => true,
            _ => return Err(Failure::refused("remote_response")),
        };
        let mut comments = Vec::new();
        for page in 1..=COMMENT_PAGES {
            let reply = self
                .api(
                    secret,
                    "GET",
                    endpoint(
                        slug,
                        &format!("issues/{n}/comments?per_page=100&page={page}"),
                    ),
                    None,
                )
                .await?;
            let batch = reply
                .as_array()
                .ok_or_else(|| Failure::refused("remote_response"))?;
            for comment in batch {
                comments.push(json!({
                    "author": author(comment, "/user/login", &mut altered)?,
                    "created_at": observed_string(comment, "/created_at")?,
                    "body": body(comment, "/body", &mut altered)?,
                }));
            }
            if batch.len() < 100 {
                break;
            }
        }
        let body = body(&issue, "/body", &mut altered)?;
        let comments_truncated = (comments.len() as u64) < comments_total;

        Ok(json!({
            "title": line(observed_string(&issue, "/title")?, &mut altered),
            "state": observed_string(&issue, "/state")?,
            "author": author(&issue, "/user/login", &mut altered)?,
            "created_at": observed_string(&issue, "/created_at")?,
            "updated_at": observed_string(&issue, "/updated_at")?,
            "labels": labels,
            "body": body,
            "is_pull_request": is_pull_request,
            "comments": comments,
            "comments_total": comments_total,
            "comments_truncated": comments_truncated,
            "text_altered": altered,
            "trust": "untrusted",
        }))
    }
}
