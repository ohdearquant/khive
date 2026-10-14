//! Handler-level git-write policy allowlist (ADR-108 Amendment).
//!
//! `git.commit` / `git.branch` / `git.update_ref` / `git.push` fail closed at the handler when
//! no policy artifact is configured, or the artifact is empty — the same
//! enforcement class as [`crate::write_argv::reject_force`]'s unconditional
//! force-push denial: deliberately not dependent on Gate configuration. The
//! Rego/Gate policy path (ADR-018) still runs on top of this, unchanged;
//! this module only adds a handler-level precondition that must also pass.
//!
//! The policy is a closed allowlist of `(repo_path, branch_patterns)`
//! entries sourced from the `[git_write]` section of the standard khive
//! config file (`khive_runtime::engine_config::KhiveConfig`). Discovery
//! (`--config`/`KHIVE_CONFIG`, project `khive.toml`, db-anchored
//! `.khive/config.toml`, `~/.khive/config.toml`) happens exactly once, at
//! boot, in the transport layer (`khive-mcp::serve`) via
//! `khive_runtime::runtime_config_from_khive_config`, which threads the
//! resolved `[git_write]` section into `RuntimeConfig::git_write`. This
//! module never re-reads `KHIVE_CONFIG` or re-runs discovery itself —
//! [`GitWritePolicy::from_config`] is a pure, I/O-free conversion from the
//! already-resolved `RuntimeConfig::git_write` the handler reads via
//! `self.runtime().config().git_write` (ADR-108 review r2: a handler-level
//! reload ignored an explicit `--config` path that was not also exported as
//! `KHIVE_CONFIG`, so `kkernel mcp --config /path` silently failed closed).
//! An allowlisted repo is operator-declared trusted provenance — this is
//! the concrete boundary ADR-108's Open Question 4 (fork-content write
//! capability stays unbuilt) resolves to: only repos an operator has
//! explicitly named ever accept a khive-mediated write.

use std::path::{Path, PathBuf};

use khive_runtime::engine_config::GitWriteSectionConfig;

/// One allowlisted `(repo_path, branch_patterns)` entry.
#[derive(Debug, Clone)]
pub struct GitWritePolicyEntry {
    /// Absolute repo path as configured. Canonicalized at check time, not
    /// load time, so the check reflects the filesystem state at the moment
    /// of the write, not at daemon startup.
    pub repo_path: PathBuf,
    /// Non-empty list of exact branch names or single-`*`-wildcard globs
    /// (e.g. `"main"`, `"release-*"`, `"*"`).
    pub branch_patterns: Vec<String>,
}

/// The parsed `[git_write]` allowlist.
///
/// `allowed.is_empty()` is the fail-closed default — both "no `[git_write]`
/// section at all" and "`[git_write]` present with an empty `allowed` list"
/// collapse to the same empty policy, and [`GitWritePolicy::check`] denies
/// unconditionally in that state.
#[derive(Debug, Clone, Default)]
pub struct GitWritePolicy {
    pub allowed: Vec<GitWritePolicyEntry>,
}

/// Why a git-write policy check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitWritePolicyError {
    /// No `[git_write]` policy artifact is configured, or it is empty.
    NotConfigured,
    /// `repo`, canonicalized, does not exactly match any allowlisted
    /// entry's canonicalized `repo_path`.
    RepoNotAllowlisted(String),
    /// `repo` is allowlisted but `branch` matches none of that entry's
    /// `branch_patterns`.
    BranchNotAllowed { repo: String, branch: String },
}

impl std::fmt::Display for GitWritePolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(
                f,
                "this verb is unavailable until a git-write policy is configured \
                 ([git_write] in khive config, ADR-108 Amendment)"
            ),
            Self::RepoNotAllowlisted(repo) => write!(
                f,
                "repo {repo:?} is not in the configured [git_write] allowlist"
            ),
            Self::BranchNotAllowed { repo, branch } => write!(
                f,
                "branch {branch:?} in repo {repo:?} does not match any allowed \
                 branch pattern for that repo's [git_write] entry"
            ),
        }
    }
}

impl std::error::Error for GitWritePolicyError {}

impl GitWritePolicy {
    /// Deny-by-default check: fails with [`GitWritePolicyError::NotConfigured`]
    /// when the policy is empty, otherwise requires `repo` to canonicalize
    /// to an allowlisted entry and `branch` to match one of that entry's
    /// patterns. On success, returns the **canonical** repo path — callers
    /// must use this returned path for every subsequent git invocation for
    /// the call, never the raw `repo` argument (ADR-108 review r2 High
    /// finding): using the raw caller path after only canonicalizing it for
    /// the comparison is a symlink TOCTOU -- a symlink that resolved to an
    /// allowlisted repo at check time can be retargeted to an
    /// unallowlisted repo before the mutating git command runs, which would
    /// then silently operate on the retargeted path if handlers kept using
    /// `repo` instead of the resolved identity this function already
    /// computed.
    ///
    /// `repo` is canonicalized before comparison, and so is every
    /// allowlisted entry's `repo_path` — a symlink that resolves to an
    /// allowlisted repo's real path is accepted (it names the same repo);
    /// a symlink that resolves anywhere else is denied exactly as if the
    /// caller had passed that other path directly. Canonicalization never
    /// widens what is reachable, only normalizes how the same repo can be
    /// spelled.
    pub fn check(&self, repo: &Path, branch: &str) -> Result<PathBuf, GitWritePolicyError> {
        self.match_entry(repo, Some(branch)).map(|(repo, _)| repo)
    }

    /// Match a canonical repository and, for mutations, its target branch.
    /// The entry index is the stable identifier in the configured allowlist.
    pub fn match_entry(
        &self,
        repo: &Path,
        branch: Option<&str>,
    ) -> Result<(PathBuf, usize), GitWritePolicyError> {
        if self.allowed.is_empty() {
            return Err(GitWritePolicyError::NotConfigured);
        }
        let canonical_repo = std::fs::canonicalize(repo)
            .map_err(|_| GitWritePolicyError::RepoNotAllowlisted(repo.display().to_string()))?;
        let matched = self.allowed.iter().enumerate().find(|(_, entry)| {
            std::fs::canonicalize(&entry.repo_path)
                .is_ok_and(|candidate| candidate == canonical_repo)
        });
        let Some((index, entry)) = matched else {
            return Err(GitWritePolicyError::RepoNotAllowlisted(
                repo.display().to_string(),
            ));
        };
        if let Some(branch) = branch {
            if !entry
                .branch_patterns
                .iter()
                .any(|pattern| glob_match(pattern, branch))
            {
                return Err(GitWritePolicyError::BranchNotAllowed {
                    repo: repo.display().to_string(),
                    branch: branch.to_string(),
                });
            }
        }
        Ok((canonical_repo, index))
    }
}

/// Minimal glob: `*` matches any run of characters (including empty); every
/// other character must match literally. No `?`, character classes, or
/// escaping — deliberately kept to the smallest grammar that expresses
/// "exact name" (no `*`) and "prefix/suffix/contains" (exactly one `*`)
/// branch policies. ADR-108 authorizes exact name or a SINGLE wildcard only;
/// `KhiveConfig::validate` already rejects a multi-star pattern at config
/// load, but this guard defends the invariant here too, for any
/// `GitWritePolicy` built directly (e.g. by a future non-TOML config path)
/// rather than through `KhiveConfig::validate` — a pattern with two or more
/// `*` never matches anything instead of silently widening to a broader
/// grammar than the ADR authorizes.
fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern.matches('*').count() > 1 {
        return false;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = value;
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
        } else if i == last {
            if !rest.ends_with(part) {
                return false;
            }
        } else {
            match rest.find(part) {
                Some(pos) => rest = &rest[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

impl GitWritePolicy {
    /// Pure, I/O-free conversion from an already-resolved `[git_write]`
    /// section (`RuntimeConfig::git_write`, threaded in at boot from
    /// `KhiveConfig` — see the module doc). Performs no discovery and reads
    /// no environment variables: the handler passes in whatever
    /// `self.runtime().config().git_write` already holds.
    pub fn from_config(section: &GitWriteSectionConfig) -> Self {
        let allowed = section
            .allowed
            .iter()
            .map(|entry| GitWritePolicyEntry {
                repo_path: PathBuf::from(&entry.repo),
                branch_patterns: entry.branches.clone(),
            })
            .collect();
        GitWritePolicy { allowed }
    }
}

#[cfg(test)]
#[path = "write_policy_tests.rs"]
mod tests;
