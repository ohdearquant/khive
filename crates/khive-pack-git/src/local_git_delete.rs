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

/// Whether stderr from `update-ref --no-deref -d <reference> <expected>` is git's refusal of the
/// compare: the ref is at another commit, or no longer exists. A delete reports that refusal
/// through `error:`, where a ref move dies through `fatal: update_ref failed`. Every other failure
/// of a delete stays unestablished, because the ref may already be gone.
pub(super) fn delete_refused(reference: &str, expected: &str, stderr: &[u8]) -> bool {
    let Ok(message) = std::str::from_utf8(stderr) else {
        return false;
    };
    let prefix = format!("error: cannot lock ref '{reference}': ");
    let Some(reason) = message
        .strip_prefix(&prefix)
        .and_then(|value| value.strip_suffix('\n'))
    else {
        return false;
    };
    let expected = expected.to_ascii_lowercase();
    if reason == format!("reference is missing but expected {expected}") {
        return true;
    }
    reason
        .strip_prefix("is at ")
        .and_then(|value| value.strip_suffix(&format!(" but expected {expected}")))
        .is_some_and(|observed| {
            crate::object_id::is_40_hex(observed.as_bytes())
                && !observed.eq_ignore_ascii_case(&expected)
        })
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
    run_git(
        program,
        repo,
        &["update-ref", "--no-deref", "-d", &reference, expected],
        None,
        None,
        true,
    )?;
    Ok(())
}

/// Delete `branch` if and only if it is at `expected`, in one compare-and-delete. A refusal of the
/// compare is `not_committed`; any other failure is ambiguous.
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
