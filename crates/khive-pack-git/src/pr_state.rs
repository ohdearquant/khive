//! `git.pr_state` (ADR-182 Amendment 14): what the platform says about one pull request.
//!
//! The verb reads and reports. It writes no receipt, changes nothing on the platform, and
//! returns no verdict of its own: `mergeable_state` and every check conclusion are the
//! platform's words, and a caller that needs a required-check ruling reads the rows.

use std::collections::BTreeMap;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use serde_json::{json, Value};

use super::{endpoint, number, observed_sha, observed_string};
use crate::local_handlers::{required, validate_keys, Failure};
use crate::{credentials, GitPack};

/// Check runs read per call: five pages of one hundred.
const CHECK_PAGES: u64 = 5;
/// Review pages read per call, the bound `git.pr_merge` uses for the same list.
const REVIEW_PAGES: u64 = 10;

/// Characters removed from platform text before it is returned: controls (Cc), format
/// characters (Cf) and the line and paragraph separators (Zl, Zp). A branch or check name is
/// written by whoever opened the pull request, and the reader of this result may be a
/// terminal or a model, so nothing invisible or direction-changing passes through. The test
/// is by category, never an allow-list: accented and CJK names are kept whole.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            u32::from(c),
            0x00AD
                | 0x0600..=0x0605
                | 0x061C
                | 0x06DD
                | 0x070F
                | 0x0890..=0x0891
                | 0x08E2
                | 0x180E
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x2064
                | 0x2066..=0x206F
                | 0xFEFF
                | 0xFFF9..=0xFFFB
                | 0x110BD
                | 0x110CD
                | 0x13430..=0x1343F
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0001
                | 0xE0020..=0xE007F
        )
}

/// Platform text with hidden characters removed and at most `limit` characters kept. `altered`
/// is set when either changed the text, so a caller can tell a returned name is not the
/// platform's exact one. A missing or non-string value stays null.
fn clean(value: Option<&Value>, limit: usize, altered: &mut bool) -> Value {
    let Some(text) = value.and_then(Value::as_str) else {
        return Value::Null;
    };
    let kept: String = text.chars().filter(|c| !hidden(*c)).take(limit).collect();
    if kept != text {
        *altered = true;
    }
    Value::String(kept)
}

fn refuse(failure: Failure) -> RuntimeError {
    RuntimeError::InvalidInput(failure.reason.into())
}

fn count(value: &Value, pointer: &str) -> Result<u64, Failure> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .ok_or_else(|| Failure::refused("remote_response"))
}

fn rows<'a>(value: &'a Value, pointer: &str) -> Result<&'a Vec<Value>, Failure> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| Failure::refused("remote_response"))
}

impl GitPack {
    #[cfg(not(unix))]
    pub(crate) async fn handle_pr_state(
        &self,
        _token: &NamespaceToken,
        _registry: &VerbRegistry,
        _params: Value,
    ) -> Result<Value, RuntimeError> {
        Err(refuse(Failure::refused("actor_unmapped")))
    }

    /// Order: arguments, allowlist match, policy decision, repository mapping, credential,
    /// platform reads. A local mapping refuses `remote_scheme` before any credential is
    /// resolved, as the pull request writes do.
    #[cfg(unix)]
    pub(crate) async fn handle_pr_state(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let params = crate::params::parse("git.pr_state", params)?;
        let invalid = || RuntimeError::InvalidInput("invalid_params".into());
        validate_keys(&params, &["repo", "number"]).map_err(|_| invalid())?;
        let repo = required(&params, "repo").map_err(|_| invalid())?;
        let n = number(&params).map_err(|_| invalid())?;
        let (canonical, index) = self
            .read_gate(token, registry, "git.pr_state", repo)
            .await?;
        let target = self.remote_repository(&canonical).map_err(refuse)?;
        if target.slug.is_empty() {
            return Err(refuse(Failure::refused("remote_scheme")));
        }
        // gh's GitHub endpoint must name the same repository as configured Git.
        if target.remote.trim_end_matches(".git") != format!("https://github.com/{}", target.slug) {
            return Err(refuse(Failure::refused("repository_identity")));
        }
        let config = &self.runtime().config().git_write;
        let (_, secret) = credentials::resolve_remote(config, &token.actor().label())
            .await
            .map_err(|_| refuse(Failure::refused("actor_unmapped")))?;
        let state = self
            .read_pr_state(secret.value(), &target.slug, &target.visibility, n)
            .await
            .map_err(refuse)?;
        let mut result = json!({
            "repo": canonical.display().to_string(),
            "gate": {"decision":"allow", "source":"git_write.allowed", "id": index},
            "slug": target.slug,
            "number": n,
        });
        if let (Some(result), Value::Object(state)) = (result.as_object_mut(), state) {
            result.extend(state);
        }
        Ok(result)
    }

    #[cfg(unix)]
    async fn read_pr_state(
        &self,
        secret: &str,
        slug: &str,
        visibility: &str,
        n: u64,
    ) -> Result<Value, Failure> {
        // A renamed or transferred repository answers under its new name with success, so
        // the name and the visibility are compared before anything is read from it.
        let remote = self
            .api(secret, "GET", format!("repos/{slug}"), None)
            .await?;
        if observed_string(&remote, "/full_name")? != slug
            || observed_string(&remote, "/visibility")? != visibility
        {
            return Err(Failure::refused("repository_identity_mismatch"));
        }
        let pr = self
            .api(secret, "GET", endpoint(slug, &format!("pulls/{n}")), None)
            .await?;
        let head = observed_sha(&pr, "/head/sha")?;
        let merged = pr.get("merged").and_then(Value::as_bool) == Some(true);
        let mut altered = false;

        let mut runs = Vec::new();
        let mut counts = BTreeMap::<String, u64>::new();
        let mut total = 0;
        for page in 1..=CHECK_PAGES {
            let reply = self
                .api(
                    secret,
                    "GET",
                    endpoint(
                        slug,
                        &format!("commits/{head}/check-runs?per_page=100&page={page}"),
                    ),
                    None,
                )
                .await?;
            total = count(&reply, "/total_count")?;
            let batch = rows(&reply, "/check_runs")?;
            for run in batch {
                let status = observed_string(run, "/status")?;
                let conclusion = run.get("conclusion").and_then(Value::as_str);
                // A run that has not completed has no conclusion; it is counted under its
                // status so "queued" and "in_progress" never read as a missing row.
                let key = if status == "completed" {
                    conclusion.unwrap_or("completed")
                } else {
                    status
                };
                *counts
                    .entry(key.chars().filter(|c| !hidden(*c)).take(40).collect())
                    .or_default() += 1;
                runs.push(json!({
                    "id": run.get("id").and_then(Value::as_u64),
                    "name": clean(run.get("name"), 200, &mut altered),
                    "status": clean(run.get("status"), 40, &mut altered),
                    "conclusion": clean(run.get("conclusion"), 40, &mut altered),
                    "app": clean(run.pointer("/app/slug"), 100, &mut altered),
                    "suite_id": run.pointer("/check_suite/id").and_then(Value::as_u64),
                    "started_at": clean(run.get("started_at"), 40, &mut altered),
                    "completed_at": clean(run.get("completed_at"), 40, &mut altered),
                }));
            }
            if batch.len() < 100 {
                break;
            }
        }

        let combined = self
            .api(
                secret,
                "GET",
                endpoint(slug, &format!("commits/{head}/status?per_page=100")),
                None,
            )
            .await?;
        let status_total = count(&combined, "/total_count")?;
        let contexts: Vec<Value> = rows(&combined, "/statuses")?
            .iter()
            .map(|status| {
                json!({
                    "context": clean(status.get("context"), 200, &mut altered),
                    "state": clean(status.get("state"), 40, &mut altered),
                    "updated_at": clean(status.get("updated_at"), 40, &mut altered),
                })
            })
            .collect();

        let mut latest = BTreeMap::<String, (String, String)>::new();
        let mut complete = false;
        for page in 1..=REVIEW_PAGES {
            let reply = self
                .api(
                    secret,
                    "GET",
                    endpoint(slug, &format!("pulls/{n}/reviews?per_page=100&page={page}")),
                    None,
                )
                .await?;
            let batch = reply
                .as_array()
                .ok_or_else(|| Failure::refused("remote_response"))?;
            for review in batch {
                let login = observed_string(review, "/user/login")?.to_ascii_lowercase();
                let state = observed_string(review, "/state")?.to_string();
                // Comment-only reviews do not dismiss a prior approval.
                if state != "COMMENTED" && state != "PENDING" {
                    latest.insert(login, (state, observed_sha(review, "/commit_id")?));
                }
            }
            if batch.len() < 100 {
                complete = true;
                break;
            }
        }
        if !complete {
            return Err(Failure::refused("review_evidence_limit"));
        }
        let reviews: Vec<Value> = latest
            .iter()
            .map(|(login, (state, sha))| {
                json!({
                    "login": clean(Some(&json!(login)), 100, &mut altered),
                    "state": clean(Some(&json!(state)), 40, &mut altered),
                    "commit_id": sha,
                    "at_head": *sha == head,
                })
            })
            .collect();

        let head_repo = pr.pointer("/head/repo/full_name").and_then(Value::as_str);
        Ok(json!({
            "state": clean(pr.get("state"), 40, &mut altered),
            "merged": merged,
            "draft": pr.get("draft").and_then(Value::as_bool),
            "opened_by": clean(pr.pointer("/user/login"), 100, &mut altered),
            "head": {
                "ref": clean(pr.pointer("/head/ref"), 255, &mut altered),
                "sha": head,
                "fork": head_repo.map(|name| name != slug),
            },
            "base": {
                "ref": clean(pr.pointer("/base/ref"), 255, &mut altered),
                "sha": pr.pointer("/base/sha").and_then(Value::as_str),
            },
            "mergeable": pr.get("mergeable").and_then(Value::as_bool),
            "mergeable_state": clean(pr.get("mergeable_state"), 40, &mut altered),
            // Before a merge the platform keeps a trial merge commit in this field.
            "merged_sha": if merged { Some(observed_sha(&pr, "/merge_commit_sha")?) } else { None },
            "checks": {
                "head_sha": head,
                "total": total,
                "runs": runs,
                "counts": counts,
                "truncated": total > runs.len() as u64,
            },
            "statuses": {
                "state": clean(combined.get("state"), 40, &mut altered),
                "total": status_total,
                "contexts": contexts,
                "truncated": status_total > contexts.len() as u64,
            },
            "reviews": reviews,
            "text_altered": altered,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{clean, hidden};
    use serde_json::{json, Value};

    #[test]
    fn hidden_covers_each_category_and_keeps_letters() {
        // One character from Cc, Cf (soft hyphen, zero width, bidi override, tag), Zl, Zp.
        for c in [
            '\u{0007}',
            '\u{009B}',
            '\u{00AD}',
            '\u{200B}',
            '\u{202E}',
            '\u{2066}',
            '\u{FEFF}',
            '\u{E0041}',
            '\u{2028}',
            '\u{2029}',
        ] {
            assert!(hidden(c), "U+{:04X} must be removed", u32::from(c));
        }
        // Accented, CJK, punctuation beyond ASCII, a space and an emoji are all kept.
        for c in ['é', '测', '—', '·', ' ', '🦁', '/', 'A'] {
            assert!(!hidden(c), "U+{:04X} must be kept", u32::from(c));
        }
    }

    #[test]
    fn clean_reports_removal_and_truncation_and_leaves_exact_text_alone() {
        let mut altered = false;
        assert_eq!(
            clean(Some(&json!("CI / 构建 (macos)")), 200, &mut altered),
            json!("CI / 构建 (macos)")
        );
        assert!(!altered, "an exact name must not be reported as altered");

        let mut altered = false;
        assert_eq!(
            clean(
                Some(&json!("ok\u{202E}txt.exe\u{001B}[2J")),
                200,
                &mut altered
            ),
            json!("oktxt.exe[2J")
        );
        assert!(altered);

        let mut altered = false;
        assert_eq!(clean(Some(&json!("abcdef")), 3, &mut altered), json!("abc"));
        assert!(altered);

        let mut altered = false;
        assert_eq!(clean(None, 3, &mut altered), Value::Null);
        assert_eq!(clean(Some(&json!(7)), 3, &mut altered), Value::Null);
        assert!(!altered);
    }
}
