//! `git.branch_delete` (ADR-182 Amendment 21): delete one local branch at the tip the caller names,
//! when no worktree has it checked out and no open pull request on the mapped platform has it as
//! its head. Nothing on the remote is deleted.

use std::path::{Path, PathBuf};

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use serde_json::{json, Value};

#[cfg(unix)]
use super::{endpoint, observed_string, repository_refusal, segment};
#[cfg(unix)]
use crate::credentials;
use crate::local_git::{self, BranchState};
use crate::local_handlers::{
    checked_policy, denied, gate_reason, optional, required, validate_keys, Failure,
};
use crate::receipts::{self, Receipt};
use crate::write_argv::{validate_ref_name, validate_repo_path};
#[cfg(unix)]
use crate::write_handlers::repo_write_lock_async;
use crate::write_policy::{repo_access, GitWritePolicy};
use crate::GitPack;

const VERB: &str = "git.branch_delete";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

fn validate(params: &Value) -> Result<(), Failure> {
    crate::params::parse(VERB, params.clone())
        .map_err(|error| Failure::invalid(error.to_string()))?;
    validate_keys(params, &["repo", "name", "expected", "session_id"])?;
    optional(params, "session_id")?;
    if !Path::new(required(params, "repo")?).is_absolute() {
        return Err(Failure::refused("invalid_params"));
    }
    validate_ref_name("name", required(params, "name")?)
        .map_err(|error| Failure::invalid(error.to_string()))?;
    let expected = required(params, "expected")?;
    if expected.len() != 40 || !khive_types::is_lowercase_hex(expected) || expected == ZERO_OID {
        return Err(Failure::invalid(
            "expected must be a 40-character lowercase hexadecimal object id other than zero",
        ));
    }
    Ok(())
}

fn inputs(params: &Value) -> Value {
    let mut inputs = serde_json::Map::new();
    for key in ["name", "expected"] {
        if let Some(value) = params.get(key).and_then(Value::as_str) {
            inputs.insert(key.into(), Value::String(value.into()));
        }
    }
    Value::Object(inputs)
}

impl GitPack {
    /// Order: arguments, `tool.check`, the allowlist row with the branch, the repository mapping,
    /// the credential, then under the repository write lock the branch, the worktrees and the
    /// platform, and last the compare-and-delete. Every refusal leaves a receipt that was not
    /// committed and the branch as it was.
    pub(crate) async fn handle_branch_delete(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let actor = token.actor().label();
        let mut receipt = Receipt::new(
            token.namespace().as_str(),
            &actor,
            VERB,
            params
                .get("repo")
                .and_then(Value::as_str)
                .unwrap_or("<invalid-repo>"),
            inputs(&params),
            denied("invalid_params"),
            Value::Null,
        );
        receipt.session_id = params
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let preflight = self
            .branch_delete_preflight(token, registry, &params, &mut receipt)
            .await;
        self.require_receipt_storage(
            token,
            &receipt,
            receipts::insert(self.runtime(), &receipt).await,
        )
        .await?;
        let outcome = match preflight {
            Err(failure) => Err(failure),
            Ok(repo) => self.delete_branch(&params, &repo, &mut receipt).await,
        };
        self.finish_local(token, receipt, outcome).await
    }

    async fn branch_delete_preflight(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: &Value,
        receipt: &mut Receipt,
    ) -> Result<PathBuf, Failure> {
        validate(params)?;
        // As for `git.update_ref`, the use policy is asked before the repository is matched.
        receipt.policy = checked_policy(registry, token, VERB).await.unwrap_or_else(
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
        let allowlist = GitWritePolicy::from_config(&self.runtime().config().git_write);
        let (canonical, index) = allowlist
            .match_entry(
                Path::new(&receipt.repo),
                repo_access(VERB),
                params["name"].as_str(),
            )
            .map_err(|error| {
                let reason = gate_reason(&error);
                receipt.gate = denied(reason);
                let mut failure = Failure::refused(reason);
                failure.detail = Some(error.to_string());
                failure
            })?;
        receipt.repo = canonical.display().to_string();
        receipt.gate = json!({"decision":"allow", "source":"git_write.allowed", "id":index});
        validate_repo_path(&canonical).map_err(|error| Failure::invalid(error.to_string()))?;
        Ok(canonical)
    }

    #[cfg(not(unix))]
    async fn delete_branch(
        &self,
        _params: &Value,
        _repo: &Path,
        _receipt: &mut Receipt,
    ) -> Result<Value, Failure> {
        Err(Failure::refused("actor_unmapped"))
    }

    #[cfg(unix)]
    async fn delete_branch(
        &self,
        params: &Value,
        repo: &Path,
        receipt: &mut Receipt,
    ) -> Result<Value, Failure> {
        let name = required(params, "name")?;
        let expected = required(params, "expected")?;
        let repo_key = receipt.repo.clone();
        let target = self.remote_repository(repo).inspect_err(|failure| {
            if failure.reason == "repository_unmapped" {
                receipt.result = repository_refusal(&repo_key);
            }
        })?;
        if target.slug.is_empty() {
            // A local mapping names no platform, so no pull request can be asked about.
            receipt.credential = json!({"source":"none"});
            return Err(Failure::refused("remote_scheme"));
        }
        // gh's GitHub endpoint must name the same repository as configured Git.
        if target.remote.trim_end_matches(".git") != format!("https://github.com/{}", target.slug) {
            return Err(Failure::refused("repository_identity"));
        }
        let config = &self.runtime().config().git_write;
        let (identity, secret) = match credentials::resolve_remote(config, &receipt.actor).await {
            Ok(resolved) => resolved,
            Err(_) => {
                receipt.result = credentials::actor_refusal(config, &receipt.actor);
                return Err(Failure::refused("actor_unmapped"));
            }
        };
        receipt.credential = json!({"source":"actor", "ref":identity.credential_ref, "platform_identity":identity.platform_identity});
        receipts::persist(self.runtime(), receipt).await?;

        let program = config.git_program();
        let lock = repo_write_lock_async(repo)
            .await
            .map_err(|_| Failure::refused("git_failed"))?;
        let _guard = lock.lock().await;
        match local_git::branch_state(program, repo, name).await? {
            BranchState::Absent => return Err(Failure::refused("branch_absent")),
            BranchState::At(head) if head != expected => {
                return Err(Failure::refused("expected_head_mismatch"))
            }
            BranchState::At(_) => {}
        }
        if local_git::checked_out(program, repo, name).await? {
            return Err(Failure::refused("branch_checked_out"));
        }
        self.no_open_pull_request(secret.value(), &target.slug, &target.visibility, name)
            .await?;
        let result = json!({
            "repo": repo.display().to_string(),
            "ref": format!("refs/heads/{name}"),
            "from": expected,
            "receipt_id": receipt.id,
        });
        receipt.result = result.clone();
        // The tip being deleted is durable before the delete can have an effect.
        receipts::persist(self.runtime(), receipt).await?;
        #[cfg(test)]
        run_race_hook(repo, name);
        if let Err(error) = local_git::delete_branch(program, repo, name, expected).await {
            if error.code() != "not_committed" {
                return Err(error.into());
            }
            // The compare refused, so nothing was deleted: say whether the branch moved or went.
            return Err(match local_git::branch_state(program, repo, name).await {
                Ok(BranchState::Absent) => Failure::refused("branch_absent"),
                _ => Failure::refused("expected_head_mismatch"),
            });
        }
        if config.contract_faults
            && config.fault.as_deref() == Some("git.branch_delete:reply-lost-after-effect")
        {
            return Err(Failure::unknown("post_effect_fault"));
        }
        Ok(result)
    }

    /// Refuse when the platform reports an open pull request whose head is this branch of this
    /// repository. Any failed read refuses with the platform's reason: an unanswered question is
    /// never read as no pull request.
    #[cfg(unix)]
    async fn no_open_pull_request(
        &self,
        secret: &str,
        slug: &str,
        visibility: &str,
        name: &str,
    ) -> Result<(), Failure> {
        // A renamed or transferred repository answers under its new name with success, so the
        // name and the visibility are compared before its pull requests are read.
        let remote = self
            .api(secret, "GET", format!("repos/{slug}"), None)
            .await?;
        if observed_string(&remote, "/full_name")? != slug
            || observed_string(&remote, "/visibility")? != visibility
        {
            return Err(Failure::refused("repository_identity_mismatch"));
        }
        let owner = slug
            .split_once('/')
            .map(|(owner, _)| owner)
            .ok_or_else(|| Failure::refused("repository_identity"))?;
        let open = self
            .api(
                secret,
                "GET",
                endpoint(
                    slug,
                    &format!(
                        "pulls?state=open&head={}:{}&per_page=1",
                        segment(owner),
                        segment(name)
                    ),
                ),
                None,
            )
            .await?;
        let rows = open
            .as_array()
            .ok_or_else(|| Failure::refused("remote_response"))?;
        if let Some(row) = rows.first() {
            // A head filter the platform cannot parse is answered with every open pull request,
            // so a row is named only when it is this branch of this repository. Any other row
            // says nothing about this branch, and that is no answer.
            if row.pointer("/head/ref").and_then(Value::as_str) != Some(name)
                || row.pointer("/head/repo/full_name").and_then(Value::as_str) != Some(slug)
            {
                return Err(Failure::refused("remote_response"));
            }
            let mut failure = Failure::refused("pull_request_open");
            failure.detail = row
                .get("number")
                .and_then(Value::as_u64)
                .map(|n| format!("pull request {n} is open with this branch as its head"));
            return Err(failure);
        }
        Ok(())
    }

    /// `git.reconcile` of a delete receipt reports what the branch shows and settles nothing. Git
    /// drops a branch's reflog with the branch, so no receipt marker survives a delete: an absent
    /// branch shows the goal state and not who removed it, and a branch at the receipt's `from`
    /// may have been made again at that commit after this delete. Neither is evidence about this
    /// receipt, so an unknown receipt stays unknown, and it blocks no later call on the name.
    pub(crate) async fn reconcile_branch_delete(
        &self,
        repo: &Path,
        prior: &Receipt,
    ) -> Result<&'static str, Failure> {
        let name = prior
            .result
            .get("ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix("refs/heads/"))
            .map(str::to_owned);
        let from = prior
            .result
            .get("from")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // A receipt that recorded no intent never reached the delete.
        let (Some(name), Some(from)) = (name, from) else {
            return Ok("intent_unrecorded");
        };
        let state =
            local_git::branch_state(self.runtime().config().git_write.git_program(), repo, &name)
                .await?;
        Ok(match state {
            BranchState::Absent => "ref_absent",
            BranchState::At(head) if head == from => "ref_at_expected",
            BranchState::At(_) => "ref_moved",
        })
    }
}

#[cfg(test)]
pub(crate) type RaceHook = std::sync::Arc<dyn Fn(&Path, &str) + Send + Sync>;

#[cfg(test)]
static RACE_HOOK: std::sync::Mutex<Option<RaceHook>> = std::sync::Mutex::new(None);

/// Runs between the last check and the delete, with the repository write lock held, so a test
/// can move the branch where only the compare can see it.
#[cfg(test)]
fn run_race_hook(repo: &Path, branch: &str) {
    let hook = RACE_HOOK
        .lock()
        .expect("branch delete race hook mutex")
        .clone();
    if let Some(hook) = hook {
        assert!(
            crate::write_handlers::repo_write_lock(repo)
                .try_lock()
                .is_err(),
            "the delete was not protected by the repository write lock"
        );
        hook(repo, branch);
    }
}

#[cfg(test)]
pub(crate) fn set_race_hook(hook: Option<RaceHook>) {
    *RACE_HOOK.lock().expect("branch delete race hook mutex") = hook;
}
