//! The clone-side half of `git.fetch`: the ref grammar, what the clone offers as haves, the exact
//! value of a destination ref, and the compare-and-swap that moves it. The transfer itself runs in
//! a scratch repository (`remote_transport::fetch_native`) and writes no ref of the clone.

use std::path::Path;

use super::{
    blocking, cas_refused, receipt_marker, require_direct_ref, run_git_output, validate_oid,
    validate_ref_name, LocalGitError, Result, ZERO_OID,
};

/// At most this many of the clone's tips are offered as haves, newest first.
const MAX_HAVES: &str = "--count=256";

/// A pull request number as `pull/<N>` spells it: digits only, no leading zero, at most ten.
fn pull_number(text: &str) -> Option<u64> {
    if text.is_empty()
        || text.len() > 10
        || text.starts_with('0')
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

/// The remote ref a fetch of `requested` reads and the clone ref it writes, or `None` when
/// `requested` is neither a branch name nor `pull/<N>`. `pull/<N>` always means pull request N's
/// head, so a branch with that literal name is not fetchable by this verb.
pub(crate) fn fetch_refs(requested: &str) -> Option<(String, String)> {
    if let Some(number) = requested.strip_prefix("pull/") {
        let number = pull_number(number)?;
        return Some((
            format!("refs/pull/{number}/head"),
            format!("refs/fetched/pull/{number}"),
        ));
    }
    validate_ref_name("ref", requested).ok()?;
    Some((
        format!("refs/heads/{requested}"),
        format!("refs/remotes/origin/{requested}"),
    ))
}

/// The clone ref a `git.fetch` writes, when a ref given to another verb is spelled as one.
fn fetched_ref_of(requested: &str) -> Option<String> {
    if let Some(name) = requested.strip_prefix("refs/remotes/origin/") {
        return (!name.is_empty()).then(|| requested.to_string());
    }
    if let Some(name) = requested.strip_prefix("origin/") {
        return (!name.is_empty()).then(|| format!("refs/remotes/{requested}"));
    }
    let number = requested.strip_prefix("refs/fetched/pull/")?;
    pull_number(number).map(|_| requested.to_string())
}

/// The refusal for a ref that did not resolve. A ref spelled as one `git.fetch` writes, and absent
/// from the clone, is `not_fetched`, because the remedy is that call; every other failure keeps
/// its own reason.
pub(crate) async fn unresolved(
    program: &Path,
    repo: &Path,
    requested: &str,
    error: LocalGitError,
) -> LocalGitError {
    let Some(reference) = fetched_ref_of(requested) else {
        return error;
    };
    if error.code() != "git_failed" {
        return error;
    }
    match exact_ref(program, repo, &reference).await {
        Ok(None) => LocalGitError::new(
            "not_fetched",
            format!("{reference} is not in this clone; git.fetch brings it in"),
        ),
        _ => error,
    }
}

/// The clone's remote-tracking, fetched and branch tips, newest commit first, without repeats.
pub(crate) async fn fetch_haves(program: &Path, repo: &Path) -> Result<Vec<String>> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    blocking("for-each-ref", false, move || {
        let output = run_git_output(
            &program,
            &repo,
            &[
                "for-each-ref",
                MAX_HAVES,
                "--sort=-committerdate",
                "--format=%(objectname)",
                "refs/remotes/origin/",
                "refs/fetched/",
                "refs/heads/",
            ],
            None,
            None,
            false,
            None,
        )?;
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| LocalGitError::new("git_output", "invalid ref listing"))?;
        let mut haves: Vec<String> = Vec::new();
        for line in text.lines() {
            validate_oid(line, "ref tip")
                .map_err(|_| LocalGitError::new("git_output", "invalid ref listing"))?;
            let sha = line.to_ascii_lowercase();
            if !haves.contains(&sha) {
                haves.push(sha);
            }
        }
        Ok(haves)
    })
    .await
}

fn exact_ref_sync(program: &Path, repo: &Path, reference: &str) -> Result<Option<String>> {
    let output = run_git_output(
        program,
        repo,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            reference,
        ],
        None,
        None,
        false,
        None,
    )?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| LocalGitError::new("git_output", "invalid ref listing"))?;
    for line in text.lines() {
        let (sha, name) = line
            .split_once(' ')
            .ok_or_else(|| LocalGitError::new("git_output", "invalid ref listing"))?;
        // A listing pattern also matches refs below it; only the full name answers.
        if name == reference {
            validate_oid(sha, "ref")
                .map_err(|_| LocalGitError::new("git_output", "invalid ref listing"))?;
            return Ok(Some(sha.to_ascii_lowercase()));
        }
    }
    Ok(None)
}

/// The object id `reference` names exactly, or `None` when the clone holds no such ref.
pub(crate) async fn exact_ref(
    program: &Path,
    repo: &Path,
    reference: &str,
) -> Result<Option<String>> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let reference = reference.to_string();
    blocking("for-each-ref", false, move || {
        exact_ref_sync(&program, &repo, &reference)
    })
    .await
}

/// How a destination compare-and-swap ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchedRefUpdate {
    /// The ref now names the fetched commit.
    Moved,
    /// The ref did not hold the value read before the fetch, and was not changed.
    RefMoved,
    /// Another writer held the ref's lock, and the ref was not changed.
    RefLocked,
    /// Git did not say what happened; the caller reads the ref to settle it.
    Unknown,
}

/// The lock refusal `update-ref` prints when another writer holds `<reference>.lock`.
fn lock_held(reference: &str, stderr: &[u8]) -> bool {
    let Ok(message) = std::str::from_utf8(stderr) else {
        return false;
    };
    let prefix = format!(
        "fatal: update_ref failed for ref '{reference}': cannot lock ref '{reference}': Unable to create '"
    );
    message
        .strip_prefix(&prefix)
        .is_some_and(|rest| rest.contains(".lock': File exists."))
}

fn update_fetched_ref_sync(
    program: &Path,
    repo: &Path,
    reference: &str,
    new: &str,
    expected: &str,
    marker: &str,
) -> Result<FetchedRefUpdate> {
    require_direct_ref(program, repo, reference)?;
    let argv = [
        "update-ref",
        "--no-deref",
        "--create-reflog",
        "-m",
        marker,
        reference,
        new,
        expected,
    ];
    // Exit 128 is returned rather than raised, so the refusal can be told from a lost answer.
    let output = match run_git_output(program, repo, &argv, None, None, true, Some(128)) {
        Ok(output) => output,
        Err(error) if error.is_ambiguous() => return Ok(FetchedRefUpdate::Unknown),
        Err(error) => return Err(error),
    };
    if output.exit_code == 0 {
        return Ok(FetchedRefUpdate::Moved);
    }
    if cas_refused(&argv, &output.stderr) {
        return Ok(FetchedRefUpdate::RefMoved);
    }
    if lock_held(reference, &output.stderr) {
        return Ok(FetchedRefUpdate::RefLocked);
    }
    Ok(FetchedRefUpdate::Unknown)
}

/// Move `reference` from `previous` (absent when `None`) to `new`, and only from it.
pub(crate) async fn update_fetched_ref(
    program: &Path,
    repo: &Path,
    reference: &str,
    new: &str,
    previous: Option<&str>,
    receipt_id: &str,
) -> Result<FetchedRefUpdate> {
    validate_oid(new, "fetched commit")?;
    if new == ZERO_OID {
        return Err(LocalGitError::new(
            "invalid_params",
            "fetched commit must not be the zero object id",
        ));
    }
    let expected = previous.unwrap_or(ZERO_OID).to_string();
    validate_oid(&expected, "previous value")?;
    let marker = receipt_marker(receipt_id)?;
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let reference = reference.to_string();
    let new = new.to_string();
    match blocking("update-ref", true, move || {
        update_fetched_ref_sync(&program, &repo, &reference, &new, &expected, &marker)
    })
    .await
    {
        Err(error) if error.is_ambiguous() => Ok(FetchedRefUpdate::Unknown),
        other => other,
    }
}

#[cfg(test)]
#[path = "local_git_fetch_tests.rs"]
mod tests;
