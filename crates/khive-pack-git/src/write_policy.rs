//! Handler-level git-write policy allowlist (ADR-108 Amendment).
//!
//! `git.commit` / `git.branch` / `git.push` fail closed at the handler when
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
        if self.allowed.is_empty() {
            return Err(GitWritePolicyError::NotConfigured);
        }
        let canonical_repo = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
        let entry = self.allowed.iter().find(|e| {
            std::fs::canonicalize(&e.repo_path)
                .map(|c| c == canonical_repo)
                .unwrap_or(false)
        });
        let Some(entry) = entry else {
            return Err(GitWritePolicyError::RepoNotAllowlisted(
                repo.display().to_string(),
            ));
        };
        if entry
            .branch_patterns
            .iter()
            .any(|pattern| glob_match(pattern, branch))
        {
            Ok(canonical_repo)
        } else {
            Err(GitWritePolicyError::BranchNotAllowed {
                repo: repo.display().to_string(),
                branch: branch.to_string(),
            })
        }
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
mod tests {
    use super::*;

    fn entry(repo: &Path, branches: &[&str]) -> GitWritePolicyEntry {
        GitWritePolicyEntry {
            repo_path: repo.to_path_buf(),
            branch_patterns: branches.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn empty_policy_denies_not_configured() {
        let policy = GitWritePolicy::default();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            policy.check(dir.path(), "main"),
            Err(GitWritePolicyError::NotConfigured)
        );
    }

    #[test]
    fn allowlisted_repo_and_branch_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let policy = GitWritePolicy {
            allowed: vec![entry(dir.path(), &["main", "release-*"])],
        };
        assert!(policy.check(dir.path(), "main").is_ok());
        assert!(policy.check(dir.path(), "release-1.2.3").is_ok());
    }

    #[test]
    fn non_allowlisted_repo_denied() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let policy = GitWritePolicy {
            allowed: vec![entry(dir.path(), &["main"])],
        };
        let err = policy.check(other.path(), "main").unwrap_err();
        assert!(matches!(err, GitWritePolicyError::RepoNotAllowlisted(_)));
    }

    #[test]
    fn branch_outside_patterns_denied() {
        let dir = tempfile::tempdir().unwrap();
        let policy = GitWritePolicy {
            allowed: vec![entry(dir.path(), &["main"])],
        };
        let err = policy.check(dir.path(), "feat/x").unwrap_err();
        assert!(matches!(err, GitWritePolicyError::BranchNotAllowed { .. }));
    }

    #[test]
    fn glob_match_exact_no_wildcard() {
        assert!(glob_match("main", "main"));
        assert!(!glob_match("main", "mainx"));
    }

    #[test]
    fn glob_match_prefix_and_suffix_and_contains() {
        assert!(glob_match("feat/*", "feat/x"));
        assert!(!glob_match("feat/*", "fix/x"));
        assert!(glob_match("*-stable", "v1-stable"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("rel-*-final", "rel-1.2-final"));
        assert!(!glob_match("rel-*-final", "rel-1.2"));
    }

    // ADR-108: exact name or a SINGLE wildcard only -- a pattern with two or
    // more `*` must never match, even if it slipped past config validation.
    #[test]
    fn glob_match_rejects_multi_star_patterns() {
        assert!(!glob_match("**", "anything"));
        assert!(!glob_match("**", ""));
        assert!(!glob_match("rel-*-*-final", "rel-1.2-final"));
    }

    #[test]
    fn glob_match_single_star_boundary() {
        assert!(glob_match("a*b", "a-mid-b"));
        assert!(!glob_match("a*b*c", "a-x-b-y-c"));
    }

    #[test]
    fn check_returns_canonical_repo_path_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let policy = GitWritePolicy {
            allowed: vec![entry(dir.path(), &["main"])],
        };
        let canonical = policy.check(dir.path(), "main").expect("allowed");
        assert_eq!(canonical, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn from_config_converts_section() {
        use khive_runtime::engine_config::GitWriteEntryConfig;
        let section = GitWriteSectionConfig {
            allowed: vec![GitWriteEntryConfig {
                repo: "/abs/path".to_string(),
                branches: vec!["main".to_string()],
            }],
        };
        let policy = GitWritePolicy::from_config(&section);
        assert_eq!(policy.allowed.len(), 1);
        assert_eq!(policy.allowed[0].repo_path, PathBuf::from("/abs/path"));
        assert_eq!(policy.allowed[0].branch_patterns, vec!["main".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_resolving_to_allowlisted_repo_succeeds() {
        let real = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let link = parent.path().join("link-to-real");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let policy = GitWritePolicy {
            allowed: vec![entry(real.path(), &["main"])],
        };
        assert!(
            policy.check(&link, "main").is_ok(),
            "a symlink that canonicalizes to an allowlisted repo must be accepted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_resolving_elsewhere_denied() {
        let real = tempfile::tempdir().unwrap();
        let decoy = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let link = parent.path().join("link-to-decoy");
        std::os::unix::fs::symlink(decoy.path(), &link).unwrap();

        let policy = GitWritePolicy {
            allowed: vec![entry(real.path(), &["main"])],
        };
        let err = policy.check(&link, "main").unwrap_err();
        assert!(
            matches!(err, GitWritePolicyError::RepoNotAllowlisted(_)),
            "a symlink resolving outside the allowlist must not be usable as a bypass"
        );
    }

    /// TOCTOU regression (ADR-108 review r2 High finding): a symlink pointing
    /// at the allowlisted repo when `check()` runs, then retargeted to a
    /// decoy repo before execution, must not let the retargeted path be
    /// used. `check()` returns the canonical path resolved at check time;
    /// callers that use ONLY that returned path (never the raw symlink
    /// again) are unaffected by any later retarget, because the returned
    /// `PathBuf` is an ordinary absolute path string, not a live symlink
    /// traversal — retargeting `link` after this point has no way to change
    /// what the already-returned canonical path resolves to.
    #[cfg(unix)]
    #[test]
    fn canonical_path_returned_at_check_time_is_immune_to_later_retarget() {
        let real = tempfile::tempdir().unwrap();
        let decoy = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let policy = GitWritePolicy {
            allowed: vec![entry(real.path(), &["main"])],
        };
        let canonical_at_check = policy.check(&link, "main").expect("allowed at check time");
        assert_eq!(
            canonical_at_check,
            std::fs::canonicalize(real.path()).unwrap()
        );

        // Retarget the symlink to the decoy repo -- simulates an attacker
        // racing the check.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(decoy.path(), &link).unwrap();

        // The path a correct caller would now use for execution is the
        // value already returned by `check()`, not `link` -- it still names
        // the real (allowlisted) repo, unaffected by the retarget.
        assert_eq!(
            canonical_at_check,
            std::fs::canonicalize(real.path()).unwrap()
        );
        assert_ne!(
            canonical_at_check,
            std::fs::canonicalize(decoy.path()).unwrap()
        );
    }
}
