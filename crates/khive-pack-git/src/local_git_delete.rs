//! The clone-side half of `git.branch_delete` (ADR-182 Amendment 21): what the branch holds,
//! whether a worktree has it checked out, and the compare-and-delete. The allowlist, the platform
//! read and the receipt belong to the handler.

use std::path::Path;

use super::{
    blocking, branch_ref, require_direct_ref, resolve_commit_sync, run_git, run_git_output,
    validate_oid, LocalGitError, Result, ZERO_OID,
};

/// A branch as `git.branch_delete` finds it before it decides anything.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BranchState {
    Absent,
    At(String),
}

fn branch_state_sync(program: &Path, repo: &Path, branch: &str) -> Result<BranchState> {
    let reference = branch_ref(branch)?;
    // A symbolic ref is refused before its existence is asked, so a dangling one never reads as
    // an absent branch.
    require_direct_ref(program, repo, &reference)?;
    let present = run_git_output(
        program,
        repo,
        &["show-ref", "--verify", "--quiet", &reference],
        None,
        None,
        false,
        Some(1),
    )?;
    if present.exit_code == 1 {
        return Ok(BranchState::Absent);
    }
    resolve_commit_sync(program, repo, &reference).map(BranchState::At)
}

pub(crate) async fn branch_state(program: &Path, repo: &Path, branch: &str) -> Result<BranchState> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let branch = branch.to_string();
    blocking("show-ref", false, move || {
        branch_state_sync(&program, &repo, &branch)
    })
    .await
}

/// Whether one `worktree list --porcelain -z` listing names `reference` as a worktree's branch.
/// Every attribute ends with NUL there, so a worktree path holding a line break cannot read as a
/// `branch` attribute of its own.
pub(super) fn listing_names(listing: &[u8], reference: &str) -> bool {
    let wanted = format!("branch {reference}");
    listing
        .split(|byte| *byte == 0)
        .any(|field| field == wanted.as_bytes())
}

/// Whether any worktree of the repository, the main one included, has `branch` checked out.
pub(crate) async fn checked_out(program: &Path, repo: &Path, branch: &str) -> Result<bool> {
    let reference = branch_ref(branch)?;
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    blocking("worktree", false, move || {
        let listing = run_git(
            &program,
            &repo,
            &["worktree", "list", "--porcelain", "-z"],
            None,
            None,
            false,
        )?;
        Ok(listing_names(&listing, &reference))
    })
    .await
}

/// How the delete's `update-ref --stdin` transaction ended, read from the status lines it printed.
/// Each of `start`, `prepare` and `commit` prints `<command>: ok` once it succeeds, and git stops at
/// the first that fails. Preparing takes the ref's lock and compares its value, and a prepare that
/// fails releases the lock, so a transaction that never printed `prepare: ok` changed no ref. Why it
/// stopped is never read from git's error text, which differs between git versions.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DeleteOutcome {
    Deleted,
    NotCommitted,
    Unknown,
}

pub(super) fn delete_outcome(exit_code: i32, stdout: &[u8]) -> DeleteOutcome {
    match (exit_code, stdout) {
        (0, b"start: ok\nprepare: ok\ncommit: ok\n") => DeleteOutcome::Deleted,
        (0, _) => DeleteOutcome::Unknown,
        (_, b"" | b"start: ok\n") => DeleteOutcome::NotCommitted,
        // Prepared and then failed, or a reply it does not know: the ref may already be gone.
        _ => DeleteOutcome::Unknown,
    }
}

fn delete_branch_sync(program: &Path, repo: &Path, branch: &str, expected: &str) -> Result<()> {
    validate_oid(expected, "expected")?;
    // `update-ref -d` reads the zero object id as "whatever the ref holds", which would delete
    // without a compare.
    if expected == ZERO_OID {
        return Err(LocalGitError::new(
            "invalid_params",
            "expected must not be the zero object id",
        ));
    }
    let reference = branch_ref(branch)?;
    // A symref installed after inspection must never redirect the delete to its target.
    require_direct_ref(program, repo, &reference)?;
    // `branch_ref` admits no whitespace and `expected` is 40 hex digits, so each is one field of
    // its line. git dies with status 128 on a failed command; any other status stays unknown.
    let input = format!("start\ndelete {reference} {expected}\nprepare\ncommit\n");
    let output = run_git_output(
        program,
        repo,
        &["update-ref", "--no-deref", "--stdin"],
        Some(input.as_bytes()),
        None,
        true,
        Some(128),
    )?;
    match delete_outcome(output.exit_code, &output.stdout) {
        DeleteOutcome::Deleted => Ok(()),
        DeleteOutcome::NotCommitted => Err(LocalGitError::new(
            "not_committed",
            "git update-ref stopped before preparing the delete",
        )),
        DeleteOutcome::Unknown => Err(LocalGitError::after_start("update-ref", true)),
    }
}

/// Delete `branch` if and only if it is at `expected`, in one compare-and-delete. A transaction
/// that stopped before its prepare is `not_committed`; any other failure is ambiguous.
pub(crate) async fn delete_branch(
    program: &Path,
    repo: &Path,
    branch: &str,
    expected: &str,
) -> Result<()> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let branch = branch.to_string();
    let expected = expected.to_ascii_lowercase();
    blocking("update-ref", true, move || {
        delete_branch_sync(&program, &repo, &branch, &expected)
    })
    .await
}

#[cfg(test)]
#[path = "local_git_delete_tests.rs"]
mod tests;
