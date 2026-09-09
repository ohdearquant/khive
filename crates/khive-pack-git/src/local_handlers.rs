//! Local Git operations: allowlist, tool policy, durable intent, then object plumbing.

use std::path::{Path, PathBuf};

use khive_pack_tool::policy;
use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::EventOutcome;
use serde_json::{json, Value};

use crate::credentials;
use crate::local_git::{self, LocalGitError};
use crate::receipts::{self, Disposition, Receipt};
use crate::write_argv::{validate_message, validate_ref_name, validate_repo_path};
use crate::write_handlers::repo_write_lock;
use crate::write_policy::{GitWritePolicy, GitWritePolicyError};
use crate::GitPack;

pub(crate) struct Failure {
    pub(crate) reason: &'static str,
    pub(crate) ambiguous: bool,
    pub(crate) detail: Option<String>,
}

impl Failure {
    pub(crate) fn invalid(detail: impl Into<String>) -> Self {
        Self {
            reason: "invalid_params",
            ambiguous: false,
            detail: Some(detail.into()),
        }
    }

    pub(crate) fn refused(reason: &'static str) -> Self {
        Self {
            reason,
            ambiguous: false,
            detail: None,
        }
    }
}

impl From<LocalGitError> for Failure {
    fn from(error: LocalGitError) -> Self {
        Self {
            reason: error.code(),
            ambiguous: error.is_ambiguous(),
            detail: None,
        }
    }
}

impl From<RuntimeError> for Failure {
    fn from(_: RuntimeError) -> Self {
        Self {
            reason: "receipt_storage",
            ambiguous: true,
            detail: None,
        }
    }
}

pub(crate) fn required<'a>(params: &'a Value, key: &str) -> Result<&'a str, Failure> {
    match params.get(key) {
        None => Err(Failure::invalid(format!("{key} is required"))),
        Some(value) => value.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
            Failure::invalid(format!("{key} must be a string and must not be empty"))
        }),
    }
}

pub(crate) fn optional<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, Failure> {
    if params.get(key).is_none() {
        Ok(None)
    } else {
        required(params, key).map(Some)
    }
}

pub(crate) fn validate_keys(params: &Value, allowed: &[&str]) -> Result<(), Failure> {
    let map = params
        .as_object()
        .ok_or_else(|| Failure::refused("invalid_params"))?;
    if map.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Failure::refused("invalid_params"));
    }
    Ok(())
}

pub(crate) fn oid(value: &str) -> Result<(), Failure> {
    if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Failure::refused("invalid_params"))
    }
}

fn validate_operation(verb: &str, params: &Value) -> Result<(), Failure> {
    let keys: &[&str] = match verb {
        "git.checkout" => &["repo", "ref", "session_id"],
        "git.diff" => &["repo", "input_kind", "base", "head", "session_id"],
        "git.branch" => &["repo", "name", "from", "expected", "session_id"],
        "git.commit" => &[
            "repo",
            "branch",
            "tree",
            "message",
            "expected_head",
            "session_id",
        ],
        "git.reconcile" => &["receipt"],
        _ => return Err(Failure::refused("invalid_params")),
    };
    validate_keys(params, keys)?;
    optional(params, "session_id")?;
    if verb == "git.reconcile" {
        required(params, "receipt")?;
        return Ok(());
    }
    let repo = Path::new(required(params, "repo")?);
    validate_repo_path(repo).map_err(|_| Failure::refused("invalid_params"))?;
    match verb {
        "git.checkout" => {
            required(params, "ref")?;
        }
        "git.diff" => {
            if !matches!(required(params, "input_kind")?, "commits" | "trees") {
                return Err(Failure::refused("invalid_params"));
            }
            required(params, "base")?;
            required(params, "head")?;
        }
        "git.branch" => {
            validate_ref_name("name", required(params, "name")?)
                .map_err(|error| Failure::invalid(error.to_string()))?;
            optional(params, "from")?;
            if let Some(expected) = optional(params, "expected")? {
                oid(expected)?;
            }
        }
        "git.commit" => {
            validate_ref_name("branch", required(params, "branch")?)
                .map_err(|_| Failure::refused("invalid_params"))?;
            required(params, "tree")?;
            validate_message(required(params, "message")?)
                .map_err(|_| Failure::refused("invalid_params"))?;
            oid(required(params, "expected_head")?)?;
        }
        _ => {}
    }
    Ok(())
}

fn safe_inputs(verb: &str, params: &Value) -> Value {
    let keys: &[&str] = match verb {
        "git.diff" => &["input_kind", "base", "head"],
        "git.checkout" => &["ref"],
        "git.branch" => &["name", "from", "expected"],
        "git.commit" => &["branch", "tree", "message", "expected_head"],
        "git.reconcile" => &["receipt"],
        _ => &[],
    };
    let mut inputs = serde_json::Map::new();
    for key in keys {
        if let Some(value) = params.get(*key).and_then(Value::as_str) {
            inputs.insert((*key).into(), Value::String(value.into()));
        }
    }
    Value::Object(inputs)
}

pub(crate) fn denied(reason: &str) -> Value {
    json!({"decision":"deny", "source":"git_write.allowed", "id":format!("deny:{reason}")})
}

pub(crate) fn gate_reason(error: &GitWritePolicyError) -> &'static str {
    match error {
        GitWritePolicyError::NotConfigured => "not_configured",
        GitWritePolicyError::RepoNotAllowlisted(_) => "repo_not_allowlisted",
        GitWritePolicyError::BranchNotAllowed { .. } => "branch_not_allowed",
    }
}

fn wire_error(receipt: &Receipt) -> RuntimeError {
    let reason = receipt.reason.as_deref().unwrap_or("unknown");
    let message = format!("{reason}; receipt_id={}", receipt.id);
    if receipt.disposition == Disposition::Unknown {
        RuntimeError::Internal(message)
    } else {
        RuntimeError::InvalidInput(message)
    }
}

pub(crate) async fn checked_policy(
    registry: &VerbRegistry,
    token: &NamespaceToken,
    verb: &str,
) -> Result<Value, RuntimeError> {
    let actor = policy::actor_label(token);
    #[cfg(test)]
    record_policy_check(&actor);
    // The registry routes this call to ToolPack's assigned backend. The identity
    // comes from the resolved token, never a request override or a daemon label.
    let result = crate::dispatch_from_token(registry, token, "tool.check", json!({"tool":verb}))
        .await
        .map_err(|_| RuntimeError::Internal("policy_unavailable".into()))?;
    let decision = result
        .get("decision")
        .and_then(Value::as_str)
        .filter(|value| matches!(*value, "allow" | "deny" | "ask"));
    let source = result.get("source").and_then(Value::as_str);
    if result.get("actor").and_then(Value::as_str) != Some(actor.as_str()) {
        return Err(RuntimeError::Internal("policy_unavailable".into()));
    }
    let (Some(decision), Some(source)) = (decision, source) else {
        return Err(RuntimeError::Internal("policy_unavailable".into()));
    };
    let id = result
        .get("grant_id")
        .filter(|value| !value.is_null())
        .or_else(|| result.get("policy_id").filter(|value| !value.is_null()))
        .or_else(|| result.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    if !id.is_null() && !id.is_string() {
        return Err(RuntimeError::Internal("policy_unavailable".into()));
    }
    Ok(json!({"decision":decision,"source":source,"id":id}))
}

impl GitPack {
    pub(crate) async fn handle_local(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        verb: &str,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let actor = policy::actor_label(token);
        let namespace = token.namespace().as_str();
        let mut receipt = Receipt::new(
            namespace,
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
        if let Err(error) = validate_operation(verb, &params) {
            self.require_receipt_storage(
                token,
                &receipt,
                receipts::insert(self.runtime(), &receipt).await,
            )
            .await?;
            return self.finish_local(token, receipt, Err(error)).await;
        }

        // Reconcile reads only caller-owned intent before deciding the repo gate.
        let prior = if verb == "git.reconcile" {
            match receipts::load_owned(
                self.runtime(),
                namespace,
                &actor,
                params["receipt"].as_str().unwrap_or_default(),
            )
            .await
            {
                Ok(prior) => {
                    receipt.repo = prior.repo.clone();
                    Some(prior)
                }
                Err(error) => {
                    let failure = match error {
                        RuntimeError::NotFound(_) => Failure::refused("receipt_not_found"),
                        error => Failure::from(error),
                    };
                    receipt.gate = denied(failure.reason);
                    self.require_receipt_storage(
                        token,
                        &receipt,
                        receipts::insert(self.runtime(), &receipt).await,
                    )
                    .await?;
                    return self.finish_local(token, receipt, Err(failure)).await;
                }
            }
        } else {
            None
        };
        let branch = match verb {
            "git.commit" => params["branch"].as_str(),
            "git.branch" => params["name"].as_str(),
            _ => None,
        };
        let allowlist = GitWritePolicy::from_config(&self.runtime().config().git_write);
        let canonical = match allowlist.match_entry(Path::new(&receipt.repo), branch) {
            Ok((repo, index)) => {
                receipt.repo = repo.display().to_string();
                receipt.gate =
                    json!({"decision":"allow", "source":"git_write.allowed", "id":index});
                repo
            }
            Err(error) => {
                let reason = gate_reason(&error);
                receipt.gate = denied(reason);
                self.require_receipt_storage(
                    token,
                    &receipt,
                    receipts::insert(self.runtime(), &receipt).await,
                )
                .await?;
                let mut failure = Failure::refused(reason);
                failure.detail = Some(error.to_string());
                return self.finish_local(token, receipt, Err(failure)).await;
            }
        };
        // No receipt writer or repository lock is held across the nested decision.
        match checked_policy(registry, token, verb).await {
            Ok(decision) => receipt.policy = decision,
            Err(_) => {
                receipt.policy =
                    json!({"decision":"deny", "source":"policy_unavailable", "id":null});
                self.require_receipt_storage(
                    token,
                    &receipt,
                    receipts::insert(self.runtime(), &receipt).await,
                )
                .await?;
                return self
                    .finish_local(token, receipt, Err(Failure::refused("policy_unavailable")))
                    .await;
            }
        }
        self.require_receipt_storage(
            token,
            &receipt,
            receipts::insert(self.runtime(), &receipt).await,
        )
        .await?;
        if receipt.policy["decision"] != "allow" {
            return self
                .finish_local(token, receipt, Err(Failure::refused("policy_denied")))
                .await;
        }
        let lock = repo_write_lock(&canonical);
        let _guard = lock.lock().await;
        let outcome = self
            .perform_local(verb, &params, &canonical, &mut receipt, prior)
            .await;
        self.finish_local(token, receipt, outcome).await
    }

    pub(crate) async fn require_receipt_storage(
        &self,
        token: &NamespaceToken,
        receipt: &Receipt,
        stored: Result<(), RuntimeError>,
    ) -> Result<(), RuntimeError> {
        if stored.is_err() && matches!(receipt.verb.as_str(), "git.branch" | "git.commit") {
            self.emit_write_audit(
                token,
                &receipt.verb,
                Path::new(&receipt.repo),
                receipt
                    .inputs
                    .get("branch")
                    .or_else(|| receipt.inputs.get("name"))
                    .and_then(Value::as_str),
                receipt.gate["decision"].as_str().unwrap_or("deny"),
                EventOutcome::Error,
                None,
            )
            .await;
        }
        stored.map_err(|_| {
            RuntimeError::Internal(format!(
                "receipt storage unavailable; receipt_id={}",
                receipt.id
            ))
        })
    }

    async fn perform_local(
        &self,
        verb: &str,
        params: &Value,
        repo: &Path,
        receipt: &mut Receipt,
        prior: Option<Receipt>,
    ) -> Result<Value, Failure> {
        match verb {
            "git.checkout" => {
                let result =
                    local_git::checkout(self.runtime(), repo, required(params, "ref")?).await?;
                Ok(json!({"commit":result.commit, "tree":result.tree}))
            }
            "git.diff" => {
                let result = local_git::diff(
                    self.runtime(),
                    repo,
                    required(params, "input_kind")?,
                    required(params, "base")?,
                    required(params, "head")?,
                )
                .await?;
                Ok(
                    json!({"base":result.base, "head":result.head, "diff":result.diff, "summary":result.summary}),
                )
            }
            "git.branch" => {
                let name = required(params, "name")?;
                let from = optional(params, "from")?;
                let sha = local_git::resolve_commit(repo, from.unwrap_or("HEAD")).await?;
                if optional(params, "expected")?
                    .is_some_and(|expected| !sha.eq_ignore_ascii_case(expected))
                {
                    return Err(Failure::refused("expected_mismatch"));
                }
                let result = json!({"repo":repo.display().to_string(), "name":name, "from":from,
                    "sha":sha, "ref":format!("refs/heads/{name}"), "receipt_id":receipt.id});
                receipt.result = result.clone();
                receipts::persist(self.runtime(), receipt).await?;
                local_git::create_branch_ref(repo, name, &sha, &receipt.id).await?;
                Ok(result)
            }
            "git.commit" => {
                let branch = required(params, "branch")?;
                let expected = required(params, "expected_head")?.to_ascii_lowercase();
                if local_git::branch_head(repo, branch).await? != expected {
                    return Err(Failure::refused("expected_head_mismatch"));
                }
                let config = &self.runtime().config().git_write;
                let actor = credentials::resolve_actor(config, &receipt.actor)
                    .await
                    .map_err(|_| Failure::refused("actor_unmapped"))?;
                receipt.credential = json!({"source":"actor", "ref":actor.credential_ref, "platform_identity":actor.platform_identity});
                receipts::persist(self.runtime(), receipt).await?;
                let tree = required(params, "tree")?;
                let git_tree = local_git::write_manifest_tree(self.runtime(), repo, tree).await?;
                let sha = local_git::create_commit(
                    repo,
                    &git_tree,
                    &expected,
                    required(params, "message")?,
                    &actor.name,
                    &actor.email,
                )
                .await?;
                let result = json!({"repo":repo.display().to_string(), "sha":sha, "parent":expected,
                    "tree":tree, "ref":format!("refs/heads/{branch}"), "receipt_id":receipt.id});
                receipt.result = result.clone();
                // The candidate SHA is durable before update-ref can have an effect.
                receipts::persist(self.runtime(), receipt).await?;
                local_git::update_branch(repo, branch, &sha, &expected, &receipt.id).await?;
                Ok(result)
            }
            "git.reconcile" => {
                let prior = prior.ok_or_else(|| Failure::refused("receipt_not_found"))?;
                // The original operation may have settled while this call waited for the repo lock.
                let mut prior = receipts::load_owned(
                    self.runtime(),
                    &receipt.namespace,
                    &receipt.actor,
                    &prior.id,
                )
                .await?;
                if matches!(prior.verb.as_str(), "git.push" | "git.pr_merge") {
                    self.reconcile_remote(repo, &mut prior).await?;
                    return Ok(json!({"receipt":prior.to_value()}));
                }
                if !matches!(prior.verb.as_str(), "git.branch" | "git.commit") {
                    return Err(Failure::refused("local_receipt_required"));
                }
                if prior.disposition == Disposition::Unknown {
                    let branch = prior
                        .result
                        .get("ref")
                        .and_then(Value::as_str)
                        .and_then(|r| r.strip_prefix("refs/heads/"));
                    let sha = prior.result.get("sha").and_then(Value::as_str);
                    if let (Some(branch), Some(sha)) = (branch, sha) {
                        // Settlement requires both the operation marker and current reachability.
                        // Missing/pruned evidence or a rewound ref leaves the prior row unknown.
                        if local_git::operation_recorded(repo, branch, sha, &prior.id)
                            .await
                            .unwrap_or(false)
                        {
                            prior.disposition = Disposition::Committed;
                            prior.finished_at = Some(chrono::Utc::now().timestamp_micros());
                            prior.reason = None;
                            receipts::persist(self.runtime(), &prior).await?;
                        }
                    }
                }
                Ok(json!({"receipt":prior.to_value()}))
            }
            _ => Err(Failure::refused("invalid_params")),
        }
    }

    pub(crate) async fn finish_local(
        &self,
        token: &NamespaceToken,
        mut receipt: Receipt,
        outcome: Result<Value, Failure>,
    ) -> Result<Value, RuntimeError> {
        let mut detail = None;
        let outcome = match outcome {
            Ok(mut result) => {
                result["receipt_id"] = json!(receipt.id);
                receipt.result = result.clone();
                receipt.disposition = Disposition::Committed;
                receipt.reason = None;
                Ok(result)
            }
            Err(failure) => {
                detail = failure.detail;
                receipt.disposition = if failure.ambiguous {
                    Disposition::Unknown
                } else {
                    Disposition::NotCommitted
                };
                receipt.reason = Some(failure.reason.into());
                Err(())
            }
        };
        receipt.finished_at = Some(chrono::Utc::now().timestamp_micros());
        let settled = receipts::persist(self.runtime(), &receipt).await.is_ok();
        if matches!(
            receipt.verb.as_str(),
            "git.branch"
                | "git.commit"
                | "git.push"
                | "git.pr_open"
                | "git.pr_review"
                | "git.pr_merge"
        ) {
            self.emit_write_audit(
                token,
                &receipt.verb,
                Path::new(&receipt.repo),
                receipt
                    .inputs
                    .get("branch")
                    .or_else(|| receipt.inputs.get("name"))
                    .and_then(Value::as_str),
                receipt.gate["decision"].as_str().unwrap_or("deny"),
                if outcome.is_ok() {
                    EventOutcome::Success
                } else if receipt.disposition == Disposition::NotCommitted {
                    EventOutcome::Denied
                } else {
                    EventOutcome::Error
                },
                receipt.result.get("sha").and_then(Value::as_str),
            )
            .await;
        }
        if !settled {
            // The independent audit attempt above must not depend on receipt storage health.
            return Err(RuntimeError::Internal(format!(
                "receipt settlement unknown; receipt_id={}",
                receipt.id
            )));
        }
        if outcome.is_ok()
            && self.runtime().config().git_write.contract_faults
            && self.runtime().config().git_write.fault.as_deref()
                == Some(&format!("{}:audit-fails-after-effect", receipt.verb))
        {
            return Err(RuntimeError::Internal(format!(
                "audit append fault after committed effect; receipt_id={}",
                receipt.id
            )));
        }
        outcome.map_err(|()| {
            let mut error = wire_error(&receipt);
            if let (RuntimeError::InvalidInput(message), Some(detail)) = (&mut error, detail) {
                message.push_str(": ");
                message.push_str(&detail);
            }
            error
        })
    }

    pub(crate) async fn handle_receipts(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let actor = policy::actor_label(token);
        let invalid = || RuntimeError::InvalidInput("invalid_params".into());
        validate_keys(&params, &["repo", "actor", "session_id", "limit", "offset"])
            .map_err(|_| invalid())?;
        if optional(&params, "actor")
            .map_err(|_| invalid())?
            .is_some_and(|supplied| supplied != actor)
        {
            return Err(RuntimeError::InvalidInput("foreign_actor".into()));
        }
        let repo = optional(&params, "repo").map_err(|_| invalid())?;
        // Audit history is retained even if the repo disappears or its allowlist row is removed.
        let repo = repo.map(|raw| {
            std::fs::canonicalize(raw)
                .unwrap_or_else(|_| PathBuf::from(raw))
                .display()
                .to_string()
        });
        let session = optional(&params, "session_id").map_err(|_| invalid())?;
        let limit = match params.get("limit") {
            None => 100,
            Some(value) => value
                .as_u64()
                .filter(|n| (1..=500).contains(n))
                .ok_or_else(invalid)? as u32,
        };
        let offset = match params.get("offset") {
            None => 0,
            Some(value) => value.as_u64().ok_or_else(invalid)?,
        };
        let decision = checked_policy(registry, token, "git.receipts").await?;
        if decision["decision"] != "allow" {
            return Err(RuntimeError::InvalidInput("policy_denied".into()));
        }
        let page = receipts::list_owned(
            self.runtime(),
            token.namespace().as_str(),
            &actor,
            repo.as_deref(),
            session,
            limit,
            offset,
        )
        .await?;
        Ok(page.to_value())
    }

    pub(crate) async fn handle_gates(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let invalid = || RuntimeError::InvalidInput("invalid_params".into());
        validate_keys(&params, &["repo"]).map_err(|_| invalid())?;
        let repo = required(&params, "repo").map_err(|_| invalid())?;
        let config = &self.runtime().config().git_write;
        let allowlist = GitWritePolicy::from_config(config);
        let (canonical, index) = allowlist
            .match_entry(Path::new(repo), None)
            .map_err(|error| RuntimeError::InvalidInput(gate_reason(&error).into()))?;
        let decision = checked_policy(registry, token, "git.gates").await?;
        if decision["decision"] != "allow" {
            return Err(RuntimeError::InvalidInput("policy_denied".into()));
        }
        let rows: Vec<Value> = config.allowed.iter().enumerate().filter_map(|(id, row)| {
            std::fs::canonicalize(&row.repo).ok().filter(|path| *path == canonical)
                .map(|_| json!({"id":id, "repo":canonical.display().to_string(), "branches":row.branches}))
        }).collect();
        Ok(
            json!({"repo":canonical.display().to_string(), "gate":{"decision":"allow","source":"git_write.allowed","id":index},"gates":rows}),
        )
    }
}

// Counts actual local authorization attempts only in unit-test builds. Actor-scoped
// counters let concurrently running fixtures prove order without sharing a total.
#[cfg(test)]
static POLICY_CHECKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
fn record_policy_check(actor: &str) {
    *POLICY_CHECKS
        .lock()
        .expect("policy counter mutex")
        .entry(actor.into())
        .or_default() += 1;
}

#[cfg(test)]
pub(crate) fn policy_check_count(actor: &str) -> usize {
    POLICY_CHECKS
        .lock()
        .expect("policy counter mutex")
        .get(actor)
        .copied()
        .unwrap_or(0)
}
