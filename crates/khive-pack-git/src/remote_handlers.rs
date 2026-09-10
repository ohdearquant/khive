use std::collections::BTreeMap;
use std::path::Path;

use khive_pack_tool::policy;
use khive_runtime::engine_config::GitWriteRepositoryConfig;
use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};

use crate::local_handlers::{
    checked_policy, denied, gate_reason, oid, optional, required, validate_keys, Failure,
};
use crate::receipts::{self, Disposition, Receipt};
use crate::remote_transport::{ApiRequest, PushRequest, RemoteError};
use crate::write_argv::{validate_ref_name, validate_repo_path};
use crate::{credentials, local_git, GitPack};

impl Failure {
    fn unknown(reason: &'static str) -> Self {
        Self {
            reason,
            ambiguous: true,
            detail: None,
        }
    }
}

impl From<RemoteError> for Failure {
    fn from(error: RemoteError) -> Self {
        match error {
            RemoteError::Refused => Self::refused("remote_refused"),
            RemoteError::Unavailable => Self::refused("remote_unavailable"),
            RemoteError::InvalidResponse => Self::refused("remote_response"),
            RemoteError::Unknown => Self::unknown("remote_unknown"),
        }
    }
}

fn keys(verb: &str) -> &'static [&'static str] {
    match verb {
        "git.push" => &[
            "repo",
            "branch",
            "expected_local",
            "expected_remote",
            "session_id",
        ],
        "git.pr_open" => &[
            "repo",
            "head",
            "base",
            "title",
            "body",
            "expected_head",
            "session_id",
        ],
        "git.pr_review" => &[
            "repo",
            "number",
            "verdict",
            "body",
            "expected_head",
            "session_id",
        ],
        "git.pr_merge" => &[
            "repo",
            "number",
            "method",
            "subject",
            "body",
            "expected_head",
            "session_id",
        ],
        _ => &[],
    }
}

fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str, Failure> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::invalid(format!("{name} must be a string")))
}

fn number(params: &Value) -> Result<u64, Failure> {
    params
        .get("number")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or_else(|| Failure::invalid("number must be a positive integer"))
}

fn validate(verb: &str, params: &Value) -> Result<(), Failure> {
    validate_keys(params, keys(verb))?;
    validate_repo_path(Path::new(required(params, "repo")?))
        .map_err(|_| Failure::invalid("repo must be an absolute repository path"))?;
    optional(params, "session_id")?;
    // Bound durable input and CLI stdin, including malformed calls.
    if params.to_string().len() > 1024 * 1024 {
        return Err(Failure::invalid("inputs exceed limit"));
    }
    if verb == "git.push" {
        validate_ref_name("branch", required(params, "branch")?)
            .map_err(|_| Failure::invalid("invalid branch"))?;
        oid(required(params, "expected_local")?)?;
        match params.get("expected_remote") {
            None => {
                return Err(Failure::invalid(
                    "expected_remote is required; use null only for an absent remote branch",
                ))
            }
            Some(Value::Null) => {}
            Some(Value::String(sha)) => oid(sha)?,
            _ => {
                return Err(Failure::invalid(
                    "expected_remote must be a 40-hex SHA or null",
                ))
            }
        }
    } else {
        oid(required(params, "expected_head")?)?;
        text(params, "body")?;
        if verb == "git.pr_open" {
            for field in ["head", "base"] {
                validate_ref_name(field, required(params, field)?)
                    .map_err(|_| Failure::invalid("invalid branch"))?;
            }
            required(params, "title")?;
        } else {
            number(params)?;
            match verb {
                "git.pr_review"
                    if matches!(
                        required(params, "verdict")?,
                        "approve" | "request_changes" | "comment"
                    ) => {}
                "git.pr_merge" if matches!(required(params, "method")?, "squash" | "merge") => {
                    required(params, "subject")?;
                }
                _ => return Err(Failure::invalid("unsupported verdict or merge method")),
            }
        }
    }
    Ok(())
}

fn safe_inputs(verb: &str, params: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for key in keys(verb)
        .iter()
        .filter(|key| !matches!(**key, "repo" | "session_id"))
    {
        if let Some(value) = params.get(*key) {
            if value.is_null()
                || value.is_u64()
                || value.as_str().is_some_and(|s| s.len() <= 1024 * 1024)
            {
                result.insert((*key).into(), value.clone());
            }
        }
    }
    Value::Object(result)
}

fn endpoint(slug: &str, suffix: &str) -> String {
    format!("repos/{slug}/{suffix}")
}
fn segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn observed_string<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, Failure> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Failure::refused("remote_response"))
}
fn observed_sha(value: &Value, pointer: &str) -> Result<String, Failure> {
    let sha = observed_string(value, pointer)?;
    oid(sha).map_err(|_| Failure::refused("remote_response"))?;
    Ok(sha.to_ascii_lowercase())
}
fn after_effect(error: Failure) -> Failure {
    Failure::unknown(error.reason)
}

fn decode_file_path(path: &str) -> Result<String, Failure> {
    if !path.starts_with('/') {
        return Err(Failure::refused("remote_scheme"));
    }
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|b| char::from(b).to_digit(16));
            let low = bytes.next().and_then(|b| char::from(b).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => decoded.push((high * 16 + low) as u8),
                _ => return Err(Failure::refused("remote_scheme")),
            }
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).map_err(|_| Failure::refused("remote_scheme"))
}

impl GitPack {
    pub(crate) fn remote_repository(
        &self,
        repo: &Path,
    ) -> Result<GitWriteRepositoryConfig, Failure> {
        let configured = &self.runtime().config().git_write.repositories;
        let mut matches = configured
            .iter()
            .filter(|(path, _)| std::fs::canonicalize(path).ok().as_deref() == Some(repo));
        let row = matches
            .next()
            .map(|(_, row)| row.clone())
            .ok_or_else(|| Failure::refused("repository_unmapped"))?;
        if matches.next().is_some() {
            return Err(Failure::refused("repository_ambiguous"));
        }
        // Git decodes file URL paths before opening them; plain paths keep literal '%'.
        let path = match row.remote.strip_prefix("file://") {
            Some(path) => decode_file_path(path)?,
            None => row.remote.clone(),
        };
        if row.slug.is_empty()
            && path.starts_with('/')
            && !path.starts_with("//")
            && !path.chars().any(char::is_control)
        {
            if !matches!(row.visibility.as_str(), "public" | "private" | "internal") {
                return Err(Failure::refused("repository_identity"));
            }
            return Ok(row);
        }
        let rest = row
            .remote
            .strip_prefix("https://")
            .ok_or_else(|| Failure::refused("remote_scheme"))?;
        let (host, path) = rest
            .split_once('/')
            .ok_or_else(|| Failure::refused("remote_scheme"))?;
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
            || path.is_empty()
            || !path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._/".contains(&b))
            || path.split('/').any(|s| matches!(s, "" | "." | ".."))
        {
            return Err(Failure::refused("remote_scheme"));
        }
        if row.slug.split('/').count() != 2
            || row.slug.split('/').any(|s| {
                s.is_empty()
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
            })
            || !matches!(row.visibility.as_str(), "public" | "private" | "internal")
        {
            return Err(Failure::refused("repository_identity"));
        }
        Ok(row)
    }

    pub(crate) async fn handle_remote(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        verb: &str,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let actor = policy::actor_label(token);
        let mut receipt = Receipt::new(
            token.namespace().as_str(),
            &actor,
            verb,
            params
                .get("repo")
                .and_then(Value::as_str)
                .unwrap_or("<invalid-repo>"),
            safe_inputs(verb, &params),
            denied("invalid_params"),
            Value::Null,
        );
        receipt.session_id = params
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let preflight = self
            .remote_preflight(token, registry, verb, &params, &mut receipt)
            .await;
        self.require_receipt_storage(
            token,
            &receipt,
            receipts::insert(self.runtime(), &receipt).await,
        )
        .await?;
        let outcome = match preflight {
            Err(error) => Err(error),
            Ok(()) => {
                self.perform_remote(token, registry, verb, &params, &mut receipt)
                    .await
            }
        };
        let outcome = if outcome.is_ok() && self.runtime().config().git_write.contract_faults {
            match self.runtime().config().git_write.fault.as_deref() {
                Some(fault) if fault == format!("{verb}:reply-lost-after-effect") => {
                    Err(Failure::unknown("post_effect_fault"))
                }
                _ => outcome,
            }
        } else {
            outcome
        };
        self.finish_local(token, receipt, outcome).await
    }

    async fn remote_preflight(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        verb: &str,
        params: &Value,
        receipt: &mut Receipt,
    ) -> Result<(), Failure> {
        validate(verb, params)?;
        let branch = match verb {
            "git.push" => params["branch"].as_str(),
            "git.pr_open" => params["head"].as_str(),
            _ => None,
        };
        let policy =
            crate::write_policy::GitWritePolicy::from_config(&self.runtime().config().git_write);
        let (canonical, index) = policy
            .match_entry(Path::new(&receipt.repo), branch)
            .map_err(|error| {
                let reason = gate_reason(&error);
                receipt.gate = denied(reason);
                Failure::refused(reason)
            })?;
        receipt.repo = canonical.display().to_string();
        receipt.gate = json!({"decision":"allow", "source":"git_write.allowed", "id":index});
        receipt.policy = checked_policy(registry, token, verb).await.unwrap_or_else(
            |_| json!({"decision":"deny", "source":"policy_unavailable", "id":null}),
        );
        if receipt.policy["decision"] != "allow" {
            return Err(Failure::refused(
                if receipt.policy["source"] == "policy_unavailable" {
                    "policy_unavailable"
                } else {
                    "policy_denied"
                },
            ));
        }
        Ok(())
    }

    async fn api(
        &self,
        secret: &str,
        method: &'static str,
        path: String,
        body: Option<Value>,
    ) -> Result<Value, Failure> {
        self.remote_transport()
            .api(secret, ApiRequest { method, path, body })
            .await
            .map_err(Failure::from)
    }

    #[cfg(not(unix))]
    async fn perform_remote(
        &self,
        _token: &NamespaceToken,
        _registry: &VerbRegistry,
        _verb: &str,
        _params: &Value,
        _receipt: &mut Receipt,
    ) -> Result<Value, Failure> {
        Err(Failure::refused("actor_unmapped"))
    }

    #[cfg(unix)]
    async fn perform_remote(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        verb: &str,
        params: &Value,
        receipt: &mut Receipt,
    ) -> Result<Value, Failure> {
        let repo = std::path::PathBuf::from(&receipt.repo);
        let target = self.remote_repository(&repo)?;
        if target.slug.is_empty() {
            receipt.credential = json!({"source":"none"});
            if verb != "git.push" {
                return Err(Failure::refused("remote_scheme"));
            }
        }
        if verb == "git.push" {
            let (version, supported) = local_git::push_marker_support(&repo).await?;
            if !supported {
                receipt.result = json!({"toolchain":{"git_version":version,"missing_capability":"reflog write"}});
                return Err(Failure::refused("unsupported_toolchain"));
            }
            let expected = required(params, "expected_local")?.to_ascii_lowercase();
            if local_git::branch_head(&repo, required(params, "branch")?).await? != expected {
                return Err(Failure::refused("expected_local_mismatch"));
            }
        }
        if target.slug.is_empty() {
            return self.push_exact(None, &target, params, receipt).await;
        }
        let (identity, secret) =
            credentials::resolve_remote(&self.runtime().config().git_write, &receipt.actor)
                .await
                .map_err(|_| Failure::refused("actor_unmapped"))?;
        receipt.credential = json!({"source":"actor", "ref":identity.credential_ref, "platform_identity":identity.platform_identity});
        receipts::persist(self.runtime(), receipt).await?;
        let secret = secret.value();
        if verb == "git.push" {
            return self
                .push_exact(Some(secret), &target, params, receipt)
                .await;
        }
        // gh's GitHub endpoint must name the same repository as configured Git.
        if target.remote.trim_end_matches(".git") != format!("https://github.com/{}", target.slug) {
            return Err(Failure::refused("repository_identity"));
        }
        let user = self.api(secret, "GET", "user".into(), None).await?;
        let login = observed_string(&user, "/login")?;
        if !login.eq_ignore_ascii_case(&identity.platform_identity) {
            return Err(Failure::refused("platform_identity_mismatch"));
        }
        let remote = self
            .api(secret, "GET", format!("repos/{}", target.slug), None)
            .await?;
        if observed_string(&remote, "/full_name")? != target.slug
            || observed_string(&remote, "/visibility")? != target.visibility
        {
            return Err(Failure::refused("repository_identity_mismatch"));
        }
        if verb == "git.pr_open" {
            return self.open_pr(secret, &target, params, receipt).await;
        }
        let n = number(params)?;
        let pr = self
            .api(
                secret,
                "GET",
                endpoint(&target.slug, &format!("pulls/{n}")),
                None,
            )
            .await?;
        let head = observed_sha(&pr, "/head/sha")?;
        let expected = required(params, "expected_head")?.to_ascii_lowercase();
        if head != expected {
            return Err(Failure::refused("expected_head_mismatch"));
        }
        if pr.get("merged").and_then(Value::as_bool) == Some(true)
            || pr.get("state").and_then(Value::as_str) != Some("open")
        {
            return Err(Failure::refused("pull_request_closed"));
        }
        if observed_string(&pr, "/base/repo/full_name")? != target.slug {
            return Err(Failure::refused("repository_identity_mismatch"));
        }
        let author = observed_string(&pr, "/user/login")?;
        let fork = observed_string(&pr, "/head/repo/full_name")? != target.slug;
        if fork && (verb == "git.pr_merge" || params["verdict"] == "approve") {
            receipt.fork_policy = checked_policy(registry, token, &format!("{verb}.fork"))
                .await
                .unwrap_or_else(
                    |_| json!({"decision":"deny", "source":"policy_unavailable", "id":null}),
                );
            receipts::persist(self.runtime(), receipt).await?;
            if receipt.fork_policy["decision"] != "allow" {
                return Err(Failure::refused("fork_policy_denied"));
            }
        }
        let last_pusher = if verb == "git.pr_merge" || params["verdict"] == "approve" {
            self.last_pusher(receipt, &pr, &expected).await?
        } else {
            Value::Null
        };
        receipt.result = json!({"number":n, "head_sha":expected, "last_pusher":last_pusher});
        if verb == "git.pr_review" {
            if params["verdict"] == "approve" {
                if last_pusher["platform_identity"]
                    .as_str()
                    .is_some_and(|pusher| login.eq_ignore_ascii_case(pusher))
                {
                    return Err(Failure::refused("last_pusher"));
                }
                // A different runtime actor on the same platform account is not a reviewer.
                if login.eq_ignore_ascii_case(author) {
                    return Err(Failure::refused("self_approval"));
                }
                if self
                    .opened_by_actor_or_reference(receipt, n, &identity.credential_ref)
                    .await?
                {
                    return Err(Failure::refused("self_approval"));
                }
            }
            let verdict = match required(params, "verdict")? {
                "approve" => "APPROVE",
                "request_changes" => "REQUEST_CHANGES",
                _ => "COMMENT",
            };
            receipt.result["reviewer"] = json!(login);
            receipts::persist(self.runtime(), receipt).await?;
            let reply = self
                .api(
                    secret,
                    "POST",
                    endpoint(&target.slug, &format!("pulls/{n}/reviews")),
                    Some(
                        json!({"event":verdict, "body":text(params,"body")?, "commit_id":expected}),
                    ),
                )
                .await?;
            let sha = observed_sha(&reply, "/commit_id").map_err(after_effect)?;
            let state = observed_string(&reply, "/state")
                .map_err(after_effect)?
                .to_ascii_lowercase();
            let id = reply
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| Failure::unknown("remote_response"))?;
            if sha != expected
                || state
                    != match verdict {
                        "APPROVE" => "approved",
                        "REQUEST_CHANGES" => "changes_requested",
                        _ => "commented",
                    }
            {
                return Err(Failure::unknown("remote_response"));
            }
            let result =
                json!({"review_id":id, "head_sha":sha, "state":state, "last_pusher":last_pusher});
            receipt.result = result.clone();
            return Ok(result);
        }
        // Repository merge dispatch refusals (ADR-182 Amendment 7), named on
        // the receipt and decided before any platform write.
        if target.refuses_merge_by("opener") {
            let evidence = if login.eq_ignore_ascii_case(author) {
                Some("platform_login")
            } else if self
                .opened_by_actor_or_reference(receipt, n, &identity.credential_ref)
                .await?
            {
                Some("pr_open_receipt")
            } else {
                None
            };
            if let Some(evidence) = evidence {
                receipt.result["merge_refusal"] = json!({"name":"merge_by_opener",
                    "source":"git_write.repositories.merge_refusals", "evidence":evidence});
                return Err(Failure::refused("merge_by_opener"));
            }
        }
        if target.refuses_merge_by("last_pusher")
            && last_pusher["platform_identity"]
                .as_str()
                .is_some_and(|pusher| login.eq_ignore_ascii_case(pusher))
        {
            receipt.result["merge_refusal"] = json!({"name":"merge_by_last_pusher",
                "source":"git_write.repositories.merge_refusals", "evidence":"git.push.receipt"});
            return Err(Failure::refused("merge_by_last_pusher"));
        }
        let review = self
            .remote_transport()
            .review_decision(secret, &target.slug, n)
            .await?;
        receipt.result["review_decision"] = json!({
            "source":"github.pullRequest.reviewDecision",
            "value":review.get("reviewDecision"),
            "head_sha":review.get("headRefOid"),
        });
        if observed_sha(&review, "/headRefOid")? != expected {
            return Err(Failure::refused("expected_head_mismatch"));
        }
        if review.get("reviewDecision").and_then(Value::as_str) != Some("APPROVED") {
            return Err(Failure::refused("review_decision"));
        }
        self.require_review(secret, &target.slug, n, &expected, author, &last_pusher)
            .await?;
        receipt.result["merged_head_sha"] = json!(expected);
        receipt.result["slug"] = json!(target.slug);
        receipt.result["remote"] = json!(target.remote);
        receipts::persist(self.runtime(), receipt).await?;
        let merged = self.api(secret, "PUT", endpoint(&target.slug, &format!("pulls/{n}/merge")), Some(json!({"merge_method":params["method"], "commit_title":params["subject"], "commit_message":params["body"], "sha":expected}))).await?;
        match merged.get("merged").and_then(Value::as_bool) {
            Some(true) => {}
            Some(false) => return Err(Failure::refused("merge_refused")),
            None => return Err(Failure::unknown("remote_response")),
        }
        let sha = observed_sha(&merged, "/sha").map_err(after_effect)?;
        receipt.result["merged_sha"] = json!(sha);
        Ok(receipt.result.clone())
    }

    async fn push_exact(
        &self,
        secret: Option<&str>,
        target: &GitWriteRepositoryConfig,
        params: &Value,
        receipt: &mut Receipt,
    ) -> Result<Value, Failure> {
        let branch = required(params, "branch")?;
        let expected = required(params, "expected_local")?.to_ascii_lowercase();
        let old = params["expected_remote"]
            .as_str()
            .map(str::to_ascii_lowercase);
        if old.as_deref() == Some(expected.as_str()) {
            return Err(Failure::refused("already_at_target"));
        }
        let observed = self
            .remote_transport()
            .remote_ref(secret, &target.remote, branch)
            .await?;
        if observed != old {
            return Err(Failure::refused("expected_remote_mismatch"));
        }
        if let Some(old) = &old {
            if !local_git::is_ancestor(Path::new(&receipt.repo), old, &expected).await? {
                return Err(Failure::refused("non_fast_forward"));
            }
        }
        let result = json!({"ref":format!("refs/heads/{branch}"), "sha":expected, "remote":target.remote, "refspec":format!("{expected}:refs/heads/{branch}")});
        receipt.result = result.clone();
        receipts::persist(self.runtime(), receipt).await?;
        self.remote_transport()
            .push(
                secret,
                PushRequest {
                    repo: receipt.repo.clone().into(),
                    remote: target.remote.clone(),
                    branch: branch.into(),
                    expected_local: expected.clone(),
                    expected_remote: old,
                },
            )
            .await?;
        let observed = self
            .remote_transport()
            .remote_ref(secret, &target.remote, branch)
            .await
            .map_err(|_| Failure::unknown("remote_readback_unavailable"))?;
        if observed.as_deref() != Some(&expected) {
            return Err(Failure::unknown("remote_readback_mismatch"));
        }
        // A marker records an acknowledged effect, never an intention to push.
        local_git::record_push_marker(Path::new(&receipt.repo), branch, &expected, &receipt.id)
            .await
            .map_err(|_| Failure::unknown("marker_unavailable"))?;
        Ok(result)
    }

    async fn open_pr(
        &self,
        secret: &str,
        target: &GitWriteRepositoryConfig,
        params: &Value,
        receipt: &mut Receipt,
    ) -> Result<Value, Failure> {
        let head = required(params, "head")?;
        let expected = required(params, "expected_head")?.to_ascii_lowercase();
        let branch = self
            .api(
                secret,
                "GET",
                endpoint(&target.slug, &format!("git/ref/heads/{}", segment(head))),
                None,
            )
            .await?;
        if observed_sha(&branch, "/object/sha")? != expected {
            return Err(Failure::refused("expected_head_mismatch"));
        }
        let reply = self.api(secret,"POST", endpoint(&target.slug,"pulls"), Some(json!({"head":head,"base":params["base"],"title":params["title"],"body":params["body"]}))).await?;
        let sha = observed_sha(&reply, "/head/sha").map_err(after_effect)?;
        let n = reply
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| Failure::unknown("remote_response"))?;
        let url = observed_string(&reply, "/html_url").map_err(after_effect)?;
        if url != format!("https://github.com/{}/pull/{n}", target.slug) {
            return Err(Failure::unknown("remote_response"));
        }
        let result = json!({"number":n,"url":url,"head_sha":sha});
        receipt.result = result.clone();
        if sha != expected {
            return Err(Failure::unknown("opened_head_mismatch"));
        }
        Ok(result)
    }

    async fn opened_by_actor_or_reference(
        &self,
        receipt: &Receipt,
        number: u64,
        reference: &str,
    ) -> Result<bool, Failure> {
        let mut reader = self
            .runtime()
            .sql()
            .reader()
            .await
            .map_err(RuntimeError::from)?;
        let rows = reader.query_all(SqlStatement {
            sql:"SELECT actor, credential FROM git_receipts WHERE namespace=?1 AND repo=?2 AND verb='git.pr_open' AND disposition != 'not_committed' AND json_extract(result,'$.number')=?3 LIMIT 1001".into(),
            params:vec![SqlValue::Text(receipt.namespace.clone()),SqlValue::Text(receipt.repo.clone()),SqlValue::Integer(i64::try_from(number).map_err(|_| Failure::invalid("number too large"))?)],
            label:Some("git_pr_opener".into()),
        }).await.map_err(RuntimeError::from)?;
        if rows.len() > 1000 {
            return Err(Failure::refused("opener_evidence_limit"));
        }
        for row in rows {
            if matches!(row.get("actor"),Some(SqlValue::Text(actor)) if actor==&receipt.actor) {
                return Ok(true);
            }
            let credential = match row.get("credential") {
                Some(SqlValue::Text(value)) => serde_json::from_str::<Value>(value)
                    .map_err(|_| Failure::refused("opener_evidence_invalid"))?,
                Some(SqlValue::Json(value)) => value.clone(),
                _ => Value::Null,
            };
            if credential.get("ref").and_then(Value::as_str) == Some(reference) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Cross-actor evidence is intentionally restricted to this namespace and
    /// the PR's exact head repository/ref/SHA, including fork heads.
    async fn last_pusher(
        &self,
        receipt: &Receipt,
        pr: &Value,
        expected: &str,
    ) -> Result<Value, Failure> {
        let slug = observed_string(pr, "/head/repo/full_name")?;
        let branch = observed_string(pr, "/head/ref")?;
        let remote = format!("https://github.com/{slug}").to_ascii_lowercase();
        let mut reader = self
            .runtime()
            .sql()
            .reader()
            .await
            .map_err(RuntimeError::from)?;
        let rows = reader.query_all(SqlStatement {
            sql: "SELECT id, repo, disposition, credential FROM git_receipts \
                  WHERE namespace=?1 AND verb='git.push' AND disposition IN ('committed','unknown') \
                  AND lower(json_extract(result,'$.sha'))=?2 AND json_extract(result,'$.ref')=?3 \
                  AND lower(json_extract(result,'$.remote')) IN (?4,?5) \
                  ORDER BY rowid DESC LIMIT 1001".into(),
            params: vec![SqlValue::Text(receipt.namespace.clone()),SqlValue::Text(expected.into()),
                SqlValue::Text(format!("refs/heads/{branch}")),SqlValue::Text(remote.clone()),
                SqlValue::Text(format!("{remote}.git"))],
            label: Some("git_last_pusher".into()),
        }).await.map_err(RuntimeError::from)?;
        // No SQL reader is held across local Git process execution.
        drop(reader);
        for row in rows.iter().take(1000) {
            let column = |key| match row.get(key) {
                Some(SqlValue::Text(value)) => Ok(value.as_str()),
                _ => Err(Failure::refused("push_evidence_invalid")),
            };
            let id = column("id")?;
            if column("disposition")? != "committed"
                && !local_git::operation_recorded(Path::new(column("repo")?), branch, expected, id)
                    .await
                    .unwrap_or(false)
            {
                // A durable intent or a lost transport ACK is not push evidence.
                continue;
            }
            let credential = match row.get("credential") {
                Some(SqlValue::Text(value)) => serde_json::from_str::<Value>(value)
                    .map_err(|_| Failure::refused("push_evidence_invalid"))?,
                Some(SqlValue::Json(value)) => value.clone(),
                _ => return Err(Failure::refused("push_evidence_invalid")),
            };
            let login = credential
                .get("platform_identity")
                .and_then(Value::as_str)
                .filter(|login| !login.is_empty())
                .ok_or_else(|| Failure::refused("push_evidence_invalid"))?;
            return Ok(json!({"state":"known","source":"git.push.receipt",
                "platform_identity":login,"push_receipt_id":id}));
        }
        if rows.len() > 1000 {
            return Err(Failure::refused("push_evidence_limit"));
        }
        Ok(json!({"state":"unknown","reason":"no_push_receipt"}))
    }

    async fn require_review(
        &self,
        secret: &str,
        slug: &str,
        number: u64,
        expected: &str,
        author: &str,
        last_pusher: &Value,
    ) -> Result<(), Failure> {
        let mut latest = BTreeMap::<String, (String, String)>::new();
        for page in 1..=10 {
            let rows = self
                .api(
                    secret,
                    "GET",
                    endpoint(
                        slug,
                        &format!("pulls/{number}/reviews?per_page=100&page={page}"),
                    ),
                    None,
                )
                .await?;
            let rows = rows
                .as_array()
                .ok_or_else(|| Failure::refused("remote_response"))?;
            for review in rows {
                let login = observed_string(review, "/user/login")?.to_ascii_lowercase();
                let state = observed_string(review, "/state")?.to_string();
                // Comment-only reviews do not dismiss a prior approval.
                if state != "COMMENTED" && state != "PENDING" {
                    latest.insert(login, (state, observed_sha(review, "/commit_id")?));
                }
            }
            if rows.len() < 100 {
                let approved: Vec<_> = latest
                    .iter()
                    .filter(|(login, (state, sha))| {
                        !login.eq_ignore_ascii_case(author)
                            && state == "APPROVED"
                            && sha == expected
                    })
                    .collect();
                if approved.iter().any(|(login, _)| {
                    !last_pusher["platform_identity"]
                        .as_str()
                        .is_some_and(|pusher| login.eq_ignore_ascii_case(pusher))
                }) {
                    return Ok(());
                }
                if !approved.is_empty() {
                    return Err(Failure::refused("last_pusher"));
                }
                return Err(Failure::refused("missing_review"));
            }
        }
        Err(Failure::refused("review_evidence_limit"))
    }

    #[cfg(unix)]
    pub(crate) async fn reconcile_remote(
        &self,
        repo: &Path,
        prior: &mut Receipt,
    ) -> Result<(), Failure> {
        if prior.disposition != Disposition::Unknown {
            return Ok(());
        }
        let target = self.remote_repository(repo)?;
        let secret = if target.slug.is_empty() {
            if prior.verb != "git.push" {
                return Err(Failure::refused("remote_scheme"));
            }
            None
        } else {
            Some(
                credentials::resolve_remote(&self.runtime().config().git_write, &prior.actor)
                    .await
                    .map_err(|_| Failure::refused("actor_unmapped"))?
                    .1,
            )
        };
        let committed = match prior.verb.as_str() {
            "git.push" => {
                let branch = required(&prior.inputs, "branch")?;
                let sha = required(&prior.inputs, "expected_local")?.to_ascii_lowercase();
                if prior.result.get("remote").and_then(Value::as_str) != Some(&target.remote) {
                    return Ok(());
                }
                local_git::operation_recorded(repo, branch, &sha, &prior.id)
                    .await
                    .unwrap_or(false)
                    && self
                        .remote_transport()
                        .remote_ref(
                            secret.as_ref().map(|secret| secret.value()),
                            &target.remote,
                            branch,
                        )
                        .await
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some(sha.as_str())
            }
            "git.pr_merge" => {
                if prior.result.get("slug").and_then(Value::as_str) != Some(&target.slug)
                    || prior.result.get("remote").and_then(Value::as_str) != Some(&target.remote)
                {
                    return Ok(());
                }
                let n = number(&prior.inputs)?;
                let pr = self
                    .api(
                        secret
                            .as_ref()
                            .ok_or_else(|| Failure::refused("remote_scheme"))?
                            .value(),
                        "GET",
                        endpoint(&target.slug, &format!("pulls/{n}")),
                        None,
                    )
                    .await?;
                if pr.get("merged").and_then(Value::as_bool) == Some(true)
                    && observed_sha(&pr, "/head/sha")?
                        == required(&prior.inputs, "expected_head")?.to_ascii_lowercase()
                {
                    prior.result["number"] = json!(n);
                    prior.result["merged_head_sha"] = pr["head"]["sha"].clone();
                    prior.result["merged_sha"] = json!(observed_sha(&pr, "/merge_commit_sha")?);
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if committed {
            prior.disposition = Disposition::Committed;
            prior.finished_at = Some(chrono::Utc::now().timestamp_micros());
            prior.reason = None;
            receipts::persist(self.runtime(), prior).await?;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub(crate) async fn reconcile_remote(
        &self,
        _repo: &Path,
        _prior: &mut Receipt,
    ) -> Result<(), Failure> {
        Err(Failure::refused("actor_unmapped"))
    }
}
