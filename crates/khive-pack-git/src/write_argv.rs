//! Hardened argv construction for the git write verbs (ADR-108).
//!
//! Every caller-supplied value that can reach `std::process::Command::args`
//! for `git.commit` / `git.branch` / `git.push` passes through this module
//! first. Nothing here ever touches a shell: `Command::new(program)` spawns the
//! operator-selected binary directly and `.args([...])` passes each element as one literal,
//! unparsed argv entry — there is no string interpolation anywhere in this
//! crate's write path for a caller-supplied value to escape.
//!
//! This module owns exactly two responsibilities, both binding conditions of
//! ADR-108's Fork (b) resolution:
//!
//! 1. Validation on every caller-supplied string before it can reach an argv
//!    array. Commit paths use Git's internally-added `:(literal)` pathspec
//!    magic so valid filesystem names are not mistaken for caller-controlled
//!    pathspec syntax.
//! 2. A fixed subcommand + flag allowlist — the `build_*_argv` functions
//!    below are the only argv shapes the write handlers ever construct.
//!
//! Force-push is rejected unconditionally by [`reject_force`]: no code path
//! in this module can produce `--force`, `-f`, or `--force-with-lease` in a
//! push argv, regardless of caller input or Gate policy (ADR-108 hard rule
//! 1). This module is scoped for isolated, dedicated adversarial review per
//! ADR-108's binding requirement — keep new git-write argv construction here,
//! not inlined into the handlers.

use std::fmt;
use std::path::Path;

/// Maximum lengths, chosen generously above any real git identifier while
/// still bounding argv/memory size against a malicious caller.
pub const MAX_REF_LEN: usize = 255;
pub const MAX_REMOTE_LEN: usize = 255;
pub const MAX_PATH_LEN: usize = 4096;
pub const MAX_MESSAGE_LEN: usize = 16_384;
pub const MAX_AUTHOR_LEN: usize = 320;

/// Everything that can go wrong constructing a git write argv. Every variant
/// carries enough context to build a caller-facing error without ever
/// echoing back characters that were rejected specifically because they were
/// suspicious (the raw value is still included for legitimate debugging --
/// none of the rejected classes here are secret-shaped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitArgError {
    Empty(&'static str),
    TooLong(&'static str, usize),
    InvalidCharacter(&'static str, String),
    LeadingDash(&'static str, String),
    PathTraversal(&'static str, String),
    ForceDenied,
    NotARepo(String),
    NotAbsolute(String),
}

impl fmt::Display for GitArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(field) => write!(f, "{field} must not be empty"),
            Self::TooLong(field, max) => {
                write!(f, "{field} exceeds the maximum length of {max}")
            }
            Self::InvalidCharacter(field, value) => write!(
                f,
                "{field} {value:?} contains characters outside the allowed set"
            ),
            Self::LeadingDash(field, value) => write!(
                f,
                "{field} {value:?} must not start with '-' (rejected: could be parsed as a flag)"
            ),
            Self::PathTraversal(field, value) => {
                write!(f, "{field} {value:?} must not contain a '..' segment")
            }
            Self::ForceDenied => {
                write!(
                    f,
                    "force-push is never permitted through this verb (ADR-108)"
                )
            }
            Self::NotARepo(path) => write!(f, "repo {path:?} does not contain a .git entry"),
            Self::NotAbsolute(path) => write!(f, "repo {path:?} must be an absolute path"),
        }
    }
}

impl std::error::Error for GitArgError {}

/// Validates a branch/ref-shaped identifier: `name` (git.branch), `from`
/// (git.branch's optional start point), and `branch` (the git.push and
/// git.update_ref targets). Deliberately more restrictive than git's own `check-ref-format` --
/// this only needs to admit the identifiers a legitimate caller would ever
/// pass, not the full ref grammar.
pub fn validate_ref_name(field: &'static str, value: &str) -> Result<(), GitArgError> {
    if value.is_empty() {
        return Err(GitArgError::Empty(field));
    }
    if value.len() > MAX_REF_LEN {
        return Err(GitArgError::TooLong(field, MAX_REF_LEN));
    }
    if value.starts_with('-') {
        return Err(GitArgError::LeadingDash(field, value.to_string()));
    }
    if value.contains("..") {
        return Err(GitArgError::PathTraversal(field, value.to_string()));
    }
    let charset_ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if !charset_ok {
        return Err(GitArgError::InvalidCharacter(field, value.to_string()));
    }
    if value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.starts_with('.')
        || value.ends_with('.')
        || value.ends_with(".lock")
        || value.contains("@{")
    {
        return Err(GitArgError::InvalidCharacter(field, value.to_string()));
    }
    Ok(())
}

/// Validates the `remote` argument (git.push). Narrower than a ref name --
/// remote names never contain `/`.
pub fn validate_remote_name(value: &str) -> Result<(), GitArgError> {
    const FIELD: &str = "remote";
    if value.is_empty() {
        return Err(GitArgError::Empty(FIELD));
    }
    if value.len() > MAX_REMOTE_LEN {
        return Err(GitArgError::TooLong(FIELD, MAX_REMOTE_LEN));
    }
    if value.starts_with('-') {
        return Err(GitArgError::LeadingDash(FIELD, value.to_string()));
    }
    if value.contains("..") {
        return Err(GitArgError::PathTraversal(FIELD, value.to_string()));
    }
    let charset_ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !charset_ok {
        return Err(GitArgError::InvalidCharacter(FIELD, value.to_string()));
    }
    Ok(())
}

/// Validates one entry of `git.commit`'s `paths` argument as a bounded,
/// repository-relative filename. Git parses path arguments as pathspecs even
/// after `--`, so [`build_add_argv`] and [`build_commit_argv`] prepend the
/// fixed, internally-constructed `:(literal)` signature after validation.
/// Caller text such as `:(top)`, `*`, `?`, brackets, leading dashes, control
/// characters, and Unicode therefore remains filename text rather than Git
/// syntax. NUL is rejected because operating-system argv cannot represent it.
pub fn validate_commit_path(value: &str) -> Result<(), GitArgError> {
    const FIELD: &str = "paths[]";
    if value.is_empty() {
        return Err(GitArgError::Empty(FIELD));
    }
    if value.len() > MAX_PATH_LEN {
        return Err(GitArgError::TooLong(FIELD, MAX_PATH_LEN));
    }
    if Path::new(value).is_absolute() {
        return Err(GitArgError::InvalidCharacter(FIELD, value.to_string()));
    }
    if value.as_bytes().contains(&0) {
        return Err(GitArgError::InvalidCharacter(FIELD, value.to_string()));
    }
    if value.split('/').any(|seg| seg == "..") {
        return Err(GitArgError::PathTraversal(FIELD, value.to_string()));
    }
    Ok(())
}

fn literal_pathspec(value: &str) -> String {
    format!(":(literal){value}")
}

/// Validates the commit message. Passed to git as a single argv element
/// bound to `-m`'s value slot, so leading `-` is not an injection vector
/// here (unlike ref/remote/path names, which can appear as bare positional
/// argv entries) -- only NUL bytes (illegal in a process argv on every
/// target platform) and an unreasonable length are rejected.
pub fn validate_message(value: &str) -> Result<(), GitArgError> {
    const FIELD: &str = "message";
    if value.trim().is_empty() {
        return Err(GitArgError::Empty(FIELD));
    }
    if value.len() > MAX_MESSAGE_LEN {
        return Err(GitArgError::TooLong(FIELD, MAX_MESSAGE_LEN));
    }
    if value.as_bytes().contains(&0) {
        return Err(GitArgError::InvalidCharacter(
            FIELD,
            "<contains NUL byte>".to_string(),
        ));
    }
    Ok(())
}

/// Validates the `author` argument. Bound into a single `--author=<value>`
/// argv token (see [`build_commit_argv`]), so a leading dash inside `value`
/// cannot itself become a new flag -- rejected anyway, defensively, since a
/// concatenated `--author=--upload-pack=x` value has no legitimate reason to
/// start with `-`.
pub fn validate_author(value: &str) -> Result<(), GitArgError> {
    const FIELD: &str = "author";
    if value.trim().is_empty() {
        return Err(GitArgError::Empty(FIELD));
    }
    if value.len() > MAX_AUTHOR_LEN {
        return Err(GitArgError::TooLong(FIELD, MAX_AUTHOR_LEN));
    }
    if value.starts_with('-') {
        return Err(GitArgError::LeadingDash(FIELD, value.to_string()));
    }
    if value.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') {
        return Err(GitArgError::InvalidCharacter(
            FIELD,
            "<contains control character>".to_string(),
        ));
    }
    Ok(())
}

/// ADR-108 hard rule 1: force-push is always denied, unconditionally, at the
/// handler -- not left to Gate policy. `force: Some(true)` (an explicit,
/// loud request) is rejected; absent or `Some(false)` proceeds normally.
/// There is no argv path anywhere in this module that can add a force flag.
pub fn reject_force(force: Option<bool>) -> Result<(), GitArgError> {
    if force == Some(true) {
        return Err(GitArgError::ForceDenied);
    }
    Ok(())
}

/// Validates the `repo` argument for repository write operations: it must be
/// an absolute local path containing a `.git` entry (mirrors `git.digest`'s
/// local-source validation in `src/source.rs`).
pub fn validate_repo_path(path: &Path) -> Result<(), GitArgError> {
    if !path.is_absolute() {
        return Err(GitArgError::NotAbsolute(path.display().to_string()));
    }
    if !path.join(".git").exists() {
        return Err(GitArgError::NotARepo(path.display().to_string()));
    }
    Ok(())
}

/// Builds the argv for the `git add` pre-stage step of `git.commit` when
/// `paths` is non-empty. Fixed shape: `["add", "--", literal-pathspecs...]`.
pub fn build_add_argv(paths: &[String]) -> Result<Vec<String>, GitArgError> {
    for p in paths {
        validate_commit_path(p)?;
    }
    let mut argv = vec!["add".to_string(), "--".to_string()];
    argv.extend(paths.iter().map(|path| literal_pathspec(path)));
    Ok(argv)
}

/// Builds the argv for `git commit`.
///
/// - `paths` empty: `["commit", "-a", "-m", message]` (or with `--author=`)
///   -- commits everything currently staged/modified in tracked files, never
///   auto-adds new untracked files.
/// - `paths` non-empty: `["commit", "-m", message, "--", paths...]` (or with
///   `--author=`) -- scoped to exactly the given paths, which the caller
///   must have already staged via the paired `git add` step
///   ([`build_add_argv`]).
pub fn build_commit_argv(
    message: &str,
    paths: &[String],
    author: Option<&str>,
) -> Result<Vec<String>, GitArgError> {
    validate_message(message)?;
    for p in paths {
        validate_commit_path(p)?;
    }
    if let Some(a) = author {
        validate_author(a)?;
    }

    let mut argv = vec!["commit".to_string()];
    if paths.is_empty() {
        argv.push("-a".to_string());
    }
    if let Some(a) = author {
        argv.push(format!("--author={a}"));
    }
    argv.push("-m".to_string());
    argv.push(message.to_string());
    if !paths.is_empty() {
        argv.push("--".to_string());
        argv.extend(paths.iter().map(|path| literal_pathspec(path)));
    }
    Ok(argv)
}

/// Builds the argv for `git branch`: `["branch", "--", name]` or
/// `["branch", "--", name, from]`.
pub fn build_branch_argv(name: &str, from: Option<&str>) -> Result<Vec<String>, GitArgError> {
    validate_ref_name("name", name)?;
    if let Some(f) = from {
        validate_ref_name("from", f)?;
    }
    let mut argv = vec!["branch".to_string(), "--".to_string(), name.to_string()];
    if let Some(f) = from {
        argv.push(f.to_string());
    }
    Ok(argv)
}

/// Builds the argv for `git push`: `["push", "--", remote, branch]`. Never
/// carries a force flag -- see [`reject_force`], which handlers must call
/// before reaching this function.
pub fn build_push_argv(remote: &str, branch: &str) -> Result<Vec<String>, GitArgError> {
    validate_remote_name(remote)?;
    validate_ref_name("branch", branch)?;
    Ok(vec![
        "push".to_string(),
        "--".to_string(),
        remote.to_string(),
        branch.to_string(),
    ])
}

#[cfg(test)]
#[path = "write_argv_tests.rs"]
mod tests;
