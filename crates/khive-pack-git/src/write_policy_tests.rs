use std::collections::BTreeSet;

use super::*;

fn entry(repo: &Path, branches: &[&str]) -> GitWritePolicyEntry {
    GitWritePolicyEntry {
        repo_path: repo.to_path_buf(),
        branch_patterns: branches.iter().map(|s| s.to_string()).collect(),
        access: GitRepoAccess::Write,
    }
}

fn read_entry(repo: &Path) -> GitWritePolicyEntry {
    GitWritePolicyEntry {
        repo_path: repo.to_path_buf(),
        branch_patterns: Vec::new(),
        access: GitRepoAccess::Read,
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
        allowed: vec![
            GitWriteEntryConfig {
                repo: "/abs/path".to_string(),
                branches: vec!["main".to_string()],
                access: GitRepoAccess::Write,
            },
            GitWriteEntryConfig {
                repo: "/abs/other".to_string(),
                branches: Vec::new(),
                access: GitRepoAccess::Read,
            },
        ],
        ..GitWriteSectionConfig::default()
    };
    let policy = GitWritePolicy::from_config(&section);
    assert_eq!(policy.allowed.len(), 2);
    assert_eq!(policy.allowed[0].repo_path, PathBuf::from("/abs/path"));
    assert_eq!(policy.allowed[0].branch_patterns, vec!["main".to_string()]);
    assert_eq!(policy.allowed[0].access, GitRepoAccess::Write);
    assert_eq!(policy.allowed[1].access, GitRepoAccess::Read);
}

#[test]
fn a_read_entry_admits_a_read_and_refuses_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let policy = GitWritePolicy {
        allowed: vec![read_entry(dir.path())],
    };
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        policy.match_entry(dir.path(), GitRepoAccess::Read, None),
        Ok((canonical, 0))
    );
    assert!(matches!(
        policy.match_entry(dir.path(), GitRepoAccess::Write, None),
        Err(GitWritePolicyError::RepoReadOnly(_))
    ));
    assert!(matches!(
        policy.check(dir.path(), "main"),
        Err(GitWritePolicyError::RepoReadOnly(_))
    ));
}

#[test]
fn a_write_entry_also_admits_a_read() {
    let dir = tempfile::tempdir().unwrap();
    let policy = GitWritePolicy {
        allowed: vec![entry(dir.path(), &["main"])],
    };
    assert_eq!(
        policy
            .match_entry(dir.path(), GitRepoAccess::Read, None)
            .map(|(_, index)| index),
        Ok(0)
    );
}

#[test]
fn a_read_entry_listed_first_does_not_hide_a_write_entry_for_the_same_repo() {
    let dir = tempfile::tempdir().unwrap();
    let policy = GitWritePolicy {
        allowed: vec![read_entry(dir.path()), entry(dir.path(), &["main"])],
    };
    let index = |access, branch| {
        policy
            .match_entry(dir.path(), access, branch)
            .map(|(_, index)| index)
    };
    assert_eq!(index(GitRepoAccess::Read, None), Ok(0));
    assert_eq!(index(GitRepoAccess::Write, Some("main")), Ok(1));
    assert!(matches!(
        index(GitRepoAccess::Write, Some("feat/x")),
        Err(GitWritePolicyError::BranchNotAllowed { .. })
    ));
}

#[test]
fn a_read_entry_for_another_repo_leaves_this_one_unlisted() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let policy = GitWritePolicy {
        allowed: vec![read_entry(other.path())],
    };
    assert!(matches!(
        policy.check(dir.path(), "main"),
        Err(GitWritePolicyError::RepoNotAllowlisted(_))
    ));
}

/// Every verb the pack declares is listed here with what it needs from the entry
/// naming its repository, or `None` when it never consults the allowlist. A verb
/// added to the pack fails this test until it is classified in [`repo_access`].
#[test]
fn every_git_verb_is_classified_for_repository_access() {
    use GitRepoAccess::{Read, Write};
    const EXPECTED: &[(&str, Option<GitRepoAccess>)] = &[
        ("git.branch", Some(Write)),
        ("git.branch_delete", Some(Write)),
        ("git.check_log", Some(Read)),
        ("git.checkout", Some(Read)),
        ("git.commit", Some(Write)),
        ("git.diff", Some(Read)),
        ("git.digest", None),
        ("git.fetch", Some(Write)),
        ("git.gates", Some(Read)),
        ("git.ingest_cursor", None),
        ("git.init", Some(Write)),
        ("git.issue", Some(Read)),
        ("git.log", Some(Read)),
        ("git.pr_close", Some(Write)),
        ("git.pr_edit", Some(Write)),
        ("git.pr_merge", Some(Write)),
        ("git.pr_open", Some(Write)),
        ("git.pr_review", Some(Write)),
        ("git.pr_state", Some(Read)),
        ("git.push", Some(Write)),
        ("git.receipts", None),
        ("git.reconcile", Some(Read)),
        ("git.status", Some(Read)),
        ("git.update_ref", Some(Write)),
    ];
    let declared: BTreeSet<&str> = crate::vocab::GIT_HANDLERS
        .iter()
        .map(|handler| handler.name)
        .collect();
    let listed: BTreeSet<&str> = EXPECTED.iter().map(|(verb, _)| *verb).collect();
    assert_eq!(declared, listed, "classify every declared git verb here");
    for (verb, access) in EXPECTED {
        if let Some(access) = access {
            assert_eq!(repo_access(verb), *access, "{verb}");
        }
    }
    assert_eq!(repo_access("git.unknown"), Write);
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
