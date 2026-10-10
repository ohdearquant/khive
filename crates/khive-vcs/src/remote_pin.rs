//! Update the Git commit of one remote in an existing KG schema (ADR-020 §8).
//!
//! A Git `commit` pin is separate from ADR-037's SHA-256 archive `pin`. This module
//! changes only `commit`; it never checks out an archive or opens a database.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Permissions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_yaml::{Mapping, Value};

use crate::sync::{redact_git_stderr, RemoteName};

#[cfg(test)]
mod tests;

/// Receipt for a schema commit update. It deliberately contains no remote URL.
#[derive(Debug, Serialize)]
pub struct RemotePinUpdate {
    pub remote: String,
    pub requested_ref: String,
    pub previous_commit: String,
    pub commit: String,
    pub updated: bool,
}

/// Resolve a remote ref and atomically replace its schema `commit` pin.
///
/// Ref precedence is the explicit argument, the remote's `ref`, then `HEAD`.
/// Only format-major 2 schemas are supported. A remote uses either the `repo`
/// GitHub shorthand or an explicit `url`; other YAML values are preserved, but
/// successful rewrites normalize YAML formatting and do not retain comments.
/// An unchanged commit leaves the original file bytes untouched.
///
/// Concurrent updates refuse while another update holds the project lock. An
/// intervening schema edit detected before publication also refuses. External
/// editors that ignore the lock are not covered by a filesystem compare-and-swap.
/// Errors after the atomic rename explicitly say the new commit was published.
pub fn update_remote_pin(
    repo_root: &Path,
    remote: &RemoteName,
    requested_ref: Option<&str>,
) -> Result<RemotePinUpdate> {
    update_remote_pin_with(repo_root, remote, requested_ref, resolve_commit, |_| Ok(()))
}

struct Selection {
    document: Value,
    index: usize,
    source: OsString,
    reference: String,
    previous_commit: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Publication {
    BeforeRename,
    AfterRename,
}

fn update_remote_pin_with(
    repo_root: &Path,
    remote: &RemoteName,
    requested_ref: Option<&str>,
    resolve: impl FnOnce(&OsStr, &str, &RemoteName) -> Result<String>,
    mut checkpoint: impl FnMut(Publication) -> Result<()>,
) -> Result<RemotePinUpdate> {
    let root = repo_root.canonicalize().context("opening KG repository")?;
    let schema_path = root.join(".khive/kg/schema.yaml");
    let (original, permissions) = read_schema(&schema_path)?;
    let mut selected = select_remote(&original, &root, remote, requested_ref)?;
    let _lock = lock_schema(&root)?;
    ensure_unchanged(&schema_path, &original)?;
    let commit = resolve(&selected.source, &selected.reference, remote)?;
    if !is_commit(&commit) {
        bail!("remote {remote}: Git did not resolve one full 40-character commit SHA");
    }
    let commit = commit.to_ascii_lowercase();
    ensure_unchanged(&schema_path, &original)?;
    let updated = !selected.previous_commit.eq_ignore_ascii_case(&commit);
    if updated {
        selected.document["remotes"][selected.index]["commit"] = Value::String(commit.clone());
        let bytes = serde_yaml::to_string(&selected.document).context("serializing KG schema")?;
        let parent = schema_path
            .parent()
            .context("KG schema has no parent directory")?;
        let mut pending = tempfile::Builder::new()
            .prefix(".schema-update-")
            .tempfile_in(parent)
            .context("creating pending KG schema")?;
        pending
            .write_all(bytes.as_bytes())
            .context("writing pending KG schema")?;
        pending.flush().context("flushing pending KG schema")?;
        pending
            .as_file()
            .set_permissions(permissions)
            .context("preserving KG schema permissions")?;
        pending
            .as_file()
            .sync_all()
            .context("syncing pending KG schema")?;
        checkpoint(Publication::BeforeRename)?;
        ensure_unchanged(&schema_path, &original)?;
        let published = pending
            .persist(&schema_path)
            .context("publishing KG schema")?;
        checkpoint(Publication::AfterRename)
            .and_then(|()| published.sync_all().map_err(Into::into))
            .and_then(|()| sync_parent(parent))
            .context("new schema commit was published, but durability confirmation failed; inspect schema.yaml before retrying")?;
    }
    Ok(RemotePinUpdate {
        remote: remote.as_str().to_owned(),
        requested_ref: selected.reference,
        previous_commit: selected.previous_commit,
        commit,
        updated,
    })
}

fn field<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a Value> {
    mapping.get(key)
}

fn required_string<'a>(mapping: &'a Mapping, key: &str) -> Result<&'a str> {
    field(mapping, key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("KG schema requires a nonempty string {key}"))
}

fn select_remote(
    bytes: &[u8],
    root: &Path,
    remote: &RemoteName,
    requested_ref: Option<&str>,
) -> Result<Selection> {
    // Do not echo malformed YAML (which may include a credential-bearing URL).
    let document: Value =
        serde_yaml::from_slice(bytes).map_err(|error| match error.location() {
            Some(location) => anyhow!(
                "invalid KG schema YAML at line {}, column {}",
                location.line(),
                location.column()
            ),
            None => anyhow!("invalid KG schema YAML"),
        })?;
    let mapping = document
        .as_mapping()
        .context("KG schema must be one mapping document")?;
    let version = semver::Version::parse(required_string(mapping, "format_version")?)
        .map_err(|_| anyhow!("invalid KG schema format_version; expected semver major 2"))?;
    if version.major != 2 {
        bail!("unsupported KG schema format_version; expected major 2");
    }
    let remotes = field(mapping, "remotes")
        .and_then(Value::as_sequence)
        .context("KG schema remotes must be a sequence")?;
    let mut names = std::collections::HashSet::new();
    let mut selected = None;
    for (index, entry) in remotes.iter().enumerate() {
        let mapping = entry
            .as_mapping()
            .context("each KG schema remote must be a mapping")?;
        let name = required_string(mapping, "name")?;
        RemoteName::parse(name).map_err(|_| anyhow!("invalid KG schema remote name"))?;
        if !names.insert(name) {
            bail!("duplicate KG schema remote name");
        }
        if name == remote.as_str() {
            selected = Some((index, mapping));
        }
    }
    let (index, mapping) =
        selected.with_context(|| format!("remote {remote} is absent from KG schema"))?;
    let previous_commit = required_string(mapping, "commit")?.to_owned();
    if !is_commit(&previous_commit) {
        bail!("remote {remote}: schema commit must be a full 40-character Git SHA");
    }
    let source = match (field(mapping, "repo"), field(mapping, "url")) {
        (Some(_), None) => github_source(required_string(mapping, "repo")?)?,
        (None, Some(_)) => explicit_source(root, required_string(mapping, "url")?)?,
        _ => bail!("remote {remote}: supply exactly one of repo or url in KG schema"),
    };
    let configured_ref = field(mapping, "ref")
        .map(|_| required_string(mapping, "ref"))
        .transpose()?;
    let reference = requested_ref
        .or(configured_ref)
        .unwrap_or("HEAD")
        .to_owned();
    validate_reference(&reference)?;
    Ok(Selection {
        document,
        index,
        source,
        reference,
        previous_commit,
    })
}

fn is_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn github_source(repository: &str) -> Result<OsString> {
    let parts: Vec<_> = repository.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || part.len() > 255
                || *part == "."
                || *part == ".."
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
    {
        bail!("KG schema repo must be an owner/name GitHub shorthand; use url for a Git URL or local path");
    }
    Ok(format!("https://github.com/{repository}.git").into())
}

fn explicit_source(root: &Path, url: &str) -> Result<OsString> {
    if url.starts_with('-')
        || url
            .bytes()
            .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
    {
        bail!("invalid KG schema remote url");
    }
    if Path::new(url).is_absolute() {
        return Ok(url.into());
    }
    if let Some((scheme, _)) = url.split_once("://") {
        if !matches!(
            scheme,
            "https" | "http" | "ssh" | "git" | "file" | "ftp" | "ftps"
        ) {
            bail!("unsupported KG schema remote URL scheme");
        }
        return Ok(url.into());
    }
    // scp-style Git URLs have a colon before any path separator. Reject Git's
    // helper::address syntax, which could select an arbitrary transport helper.
    if let Some((host, path)) = url.split_once(':') {
        if !host.contains('/') && !host.contains('\\') {
            if host.is_empty() || path.is_empty() || path.starts_with(':') {
                bail!("invalid KG schema remote URL");
            }
            return Ok(url.into());
        }
    }
    Ok(root.join(url).into_os_string())
}

fn validate_reference(reference: &str) -> Result<()> {
    // These are Git's ref-name restrictions, with one-level names allowed.
    // No wildcard/refspec/revision-expression is accepted as a single source.
    if reference.is_empty()
        || reference.starts_with('-')
        || reference == "@"
        || reference.contains("..")
        || reference.contains("@{")
        || reference.ends_with('.')
        || reference
            .bytes()
            .any(|byte| byte <= b' ' || byte == 127 || b"~^:?*[\\".contains(&byte))
        || reference
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        bail!("invalid remote ref; expected one Git ref name or a full commit SHA");
    }
    Ok(())
}

fn git_command(directory: &Path) -> Command {
    let mut command = Command::new("git");
    // Routing inherited from a caller must not escape the disposable repository.
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| {
            name.starts_with("GIT_CONFIG_KEY_")
                || name.starts_with("GIT_CONFIG_VALUE_")
                || matches!(
                    name,
                    "GIT_DIR"
                        | "GIT_WORK_TREE"
                        | "GIT_COMMON_DIR"
                        | "GIT_INDEX_FILE"
                        | "GIT_OBJECT_DIRECTORY"
                        | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                        | "GIT_NAMESPACE"
                        | "GIT_SHALLOW_FILE"
                        | "GIT_PREFIX"
                        | "GIT_REPLACE_REF_BASE"
                        | "GIT_QUARANTINE_PATH"
                        | "GIT_CONFIG"
                        | "GIT_CONFIG_COUNT"
                        | "GIT_CONFIG_PARAMETERS"
                        | "GIT_DEFAULT_HASH"
                        | "GIT_DEFAULT_REF_FORMAT"
                )
        }) {
            command.env_remove(name);
        }
    }
    // Keep explicit Git config-file selection and transport credentials; these
    // include test/user opt-outs from machine-wide configuration.
    let mut hooks = OsString::from("core.hooksPath=");
    hooks.push(directory.join("no-hooks"));
    command
        .current_dir(directory)
        .arg("-c")
        .arg(hooks)
        .arg("-c")
        .arg("core.fsmonitor=false")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn run_git(
    command: &mut Command,
    stage: &str,
    remote: &RemoteName,
    private_source: Option<&OsStr>,
) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("remote {remote}: starting Git {stage}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Also cover exact local paths and scp URLs without a user@ prefix,
        // which are outside the shared redactor's URL-shaped token grammar.
        let stderr = match private_source {
            Some(source) => stderr.replace(source.to_string_lossy().as_ref(), "<url-redacted>"),
            None => stderr.into_owned(),
        };
        let safe = redact_git_stderr(stderr.trim());
        bail!("remote {remote}: Git {stage} failed: {safe}");
    }
    Ok(output)
}

fn resolve_commit(source: &OsStr, reference: &str, remote: &RemoteName) -> Result<String> {
    let temporary = tempfile::Builder::new()
        .prefix("khive-remote-pin-")
        .tempdir()
        .context("creating temporary remote-pin repository")?;
    let directory = temporary.path();
    run_git(
        git_command(directory).args([
            "init",
            "--bare",
            "--quiet",
            "--object-format=sha1",
            "--template=",
            ".",
        ]),
        "init",
        remote,
        None,
    )?;
    run_git(
        git_command(directory).args(["check-ref-format", "--allow-onelevel", reference]),
        "ref validation",
        remote,
        None,
    )?;
    run_git(
        git_command(directory)
            .args([
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-auto-maintenance",
                "--depth=1",
                "--filter=blob:none",
                "--",
            ])
            .arg(source)
            .arg(reference),
        "fetch",
        remote,
        Some(source),
    )?;
    let output = run_git(
        git_command(directory).args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            "FETCH_HEAD^{commit}",
        ]),
        "commit verification",
        remote,
        Some(source),
    )?;
    let commit = std::str::from_utf8(&output.stdout)
        .context("Git returned a non-UTF-8 commit SHA")?
        .trim();
    if !is_commit(commit) {
        bail!("remote {remote}: Git did not resolve one full 40-character commit SHA");
    }
    Ok(commit.to_ascii_lowercase())
}

fn read_schema(path: &Path) -> Result<(Vec<u8>, Permissions)> {
    let mut file = open_schema(path)?;
    let permissions = file
        .metadata()
        .context("reading KG schema permissions")?
        .permissions();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).context("reading KG schema")?;
    Ok((bytes, permissions))
}

#[cfg(unix)]
fn open_schema(path: &Path) -> Result<File> {
    use khive_fs::opened_file::{open_regular_file_nofollow, ContainedOpenError};
    open_regular_file_nofollow(path).map_err(|error| match error {
        ContainedOpenError::Open(error)
        | ContainedOpenError::Metadata(error)
        | ContainedOpenError::Resolve(error) => {
            anyhow!(error).context("opening regular KG schema without following a symlink")
        }
        _ => anyhow!("KG schema must be a regular file, not a symlink"),
    })
}

#[cfg(windows)]
fn open_schema(path: &Path) -> Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    // FILE_FLAG_OPEN_REPARSE_POINT opens the final entry itself, not its target.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x00200000)
        .open(path)
        .context("opening regular KG schema without following a symlink")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
        bail!("KG schema must be a regular file, not a reparse point");
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_schema(_path: &Path) -> Result<File> {
    bail!("KG schema no-follow opens are unsupported on this platform")
}

fn ensure_unchanged(path: &Path, original: &[u8]) -> Result<()> {
    if read_schema(path)?.0 != original {
        bail!("KG schema changed while resolving the remote; retry the update");
    }
    Ok(())
}

fn lock_schema(root: &Path) -> Result<File> {
    let state = root.join(".khive/state");
    fs::create_dir_all(&state).context("creating KG update lock directory")?;
    #[cfg(unix)]
    let lock = {
        use khive_fs::fd_relative::{open_file_at, Create, OpenFileOptions};
        use std::os::fd::AsFd;
        let directory = khive_fs::directory_walk::open_dir_nofollow(&state)
            .context("opening KG update lock directory")?;
        open_file_at(
            directory.as_fd(),
            "kg-update.lock",
            OpenFileOptions {
                read_write: true,
                create: Create::IfMissing,
                nonblock: true,
                mode: 0o600,
            },
        )
        .context("opening KG update lock")?
    };
    #[cfg(not(unix))]
    let lock = {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000);
        }
        options
            .open(state.join("kg-update.lock"))
            .context("opening KG update lock")?
    };
    let metadata = lock.metadata().context("reading KG update lock metadata")?;
    if !metadata.is_file() {
        bail!("KG update lock must be a regular file");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("KG update lock must not be a reparse point");
        }
    }
    fs4::FileExt::try_lock(&lock)
        .context("another KG schema update holds the project lock; retry after it completes")?;
    // Retain the pathname after close; unlinking it could let another writer
    // acquire a different inode while a holder still owns this lock.
    Ok(lock)
}

fn sync_parent(parent: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .context("syncing KG schema directory")?;
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}
