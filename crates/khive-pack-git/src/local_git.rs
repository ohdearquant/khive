//! Local object plumbing. Callers own authorization, receipts, and canonical repo identity.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use khive_pack_exec::tree::{self, TreeEntry};
use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::{ContentRef, MAX_BLOB_WHOLE_BYTES};
use serde::Serialize;

use crate::write_argv::{validate_message, validate_ref_name};

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const HARDENING: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "commit.gpgsign=false",
    "credential.helper=",
    "core.sshCommand=/usr/bin/false",
    "protocol.allow=never",
];

#[derive(Debug)]
pub(crate) struct LocalGitError {
    code: &'static str,
    message: String,
    ambiguous: bool,
}

impl LocalGitError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            ambiguous: false,
        }
    }

    fn after_start(operation: &str, ref_effect: bool) -> Self {
        Self {
            code: if ref_effect { "unknown" } else { "git_failed" },
            message: format!("git {operation} completion could not be established"),
            ambiguous: ref_effect,
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    pub(crate) fn is_ambiguous(&self) -> bool {
        self.ambiguous
    }
}

impl fmt::Display for LocalGitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for LocalGitError {}

impl From<RuntimeError> for LocalGitError {
    fn from(error: RuntimeError) -> Self {
        let code = match &error {
            RuntimeError::InvalidInput(_) => "invalid_params",
            RuntimeError::NotFound(_) => "not_found",
            _ => "storage_error",
        };
        Self::new(code, error.to_string())
    }
}

impl From<khive_storage::StorageError> for LocalGitError {
    fn from(error: khive_storage::StorageError) -> Self {
        RuntimeError::from(error).into()
    }
}

type Result<T> = std::result::Result<T, LocalGitError>;

#[derive(Debug, Serialize)]
pub(crate) struct Checkout {
    pub commit: String,
    pub tree: String,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct DiffSummary {
    pub files: u64,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct DiffResult {
    pub base: String,
    pub head: String,
    pub diff: String,
    pub summary: DiffSummary,
}

fn git_command(repo: &Path, argv: &[&str], identity: Option<(&str, &str)>) -> Command {
    let mut command = Command::new("git");
    // Inherited GIT_DIR, index/object paths, config injection, and identities
    // must not redirect an operation away from the caller's authorized repo.
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0");
    if let Some((name, email)) = identity {
        command
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email);
    }
    for setting in HARDENING {
        command.arg("-c").arg(setting);
    }
    command.arg("-C").arg(repo).args(argv);
    command
}

fn capture(mut pipe: impl Read, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    let mut exceeded = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = pipe.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let keep = count.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        exceeded |= keep != count;
    }
    Ok((bytes, exceeded))
}

struct GitOutput {
    stdout: Vec<u8>,
    exit_code: i32,
}

fn cas_refused(argv: &[&str], stderr: &[u8]) -> bool {
    let ["update-ref", "--no-deref", "--create-reflog", "-m", _, reference, _, expected] = argv
    else {
        return false;
    };
    let Ok(message) = std::str::from_utf8(stderr) else {
        return false;
    };
    let prefix =
        format!("fatal: update_ref failed for ref '{reference}': cannot lock ref '{reference}': ");
    let Some(reason) = message
        .strip_prefix(&prefix)
        .and_then(|value| value.strip_suffix('\n'))
    else {
        return false;
    };
    if reason == "reference already exists" || reason == "dangling symref already exists" {
        return *expected == ZERO_OID;
    }
    let expected = expected.to_ascii_lowercase();
    if reason == format!("reference is missing but expected {expected}") {
        return true;
    }
    let Some(observed) = reason
        .strip_prefix("is at ")
        .and_then(|value| value.strip_suffix(&format!(" but expected {expected}")))
    else {
        return false;
    };
    observed.len() == 40 && observed.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn run_git(
    repo: &Path,
    argv: &[&str],
    input: Option<&[u8]>,
    identity: Option<(&str, &str)>,
    ref_effect: bool,
) -> Result<Vec<u8>> {
    run_git_output(repo, argv, input, identity, ref_effect, None).map(|output| output.stdout)
}

fn run_git_output(
    repo: &Path,
    argv: &[&str],
    input: Option<&[u8]>,
    identity: Option<(&str, &str)>,
    ref_effect: bool,
    allowed_exit: Option<i32>,
) -> Result<GitOutput> {
    let operation = argv.first().copied().unwrap_or("operation");
    let mut command = git_command(repo, argv, identity);
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| LocalGitError::new("git_spawn", format!("could not start git {operation}")))?;
    let stdout = child.stdout.take().expect("stdout was configured as piped");
    let stderr = child.stderr.take().expect("stderr was configured as piped");
    let stdin = child.stdin.take();
    let completed = std::thread::scope(|scope| {
        let out = scope.spawn(move || capture(stdout, MAX_BLOB_WHOLE_BYTES as usize));
        // Retain bounded stderr only for native precondition classification; never expose it.
        let err = scope.spawn(move || capture(stderr, 64 * 1024));
        let writer = scope.spawn(move || match (stdin, input) {
            (Some(mut pipe), Some(bytes)) => pipe.write_all(bytes),
            _ => Ok(()),
        });
        let status = child.wait();
        if status.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        (status, out.join(), err.join(), writer.join())
    });
    let (status, out, err, input_result) = completed;
    let status = status.map_err(|_| LocalGitError::after_start(operation, ref_effect))?;
    let exit_code = status
        .code()
        .ok_or_else(|| LocalGitError::after_start(operation, ref_effect))?;
    let (stderr, stderr_exceeded) = err
        .map_err(|_| LocalGitError::after_start(operation, ref_effect))?
        .map_err(|_| LocalGitError::after_start(operation, ref_effect))?;
    if !status.success() && allowed_exit != Some(exit_code) {
        if ref_effect && (stderr_exceeded || !cas_refused(argv, &stderr)) {
            // An update-ref error can follow a successful ref move, for example
            // when a later HEAD reflog write fails. Only known CAS refusals settle.
            return Err(LocalGitError::after_start(operation, true));
        }
        return Err(LocalGitError::new(
            if ref_effect {
                "not_committed"
            } else {
                "git_failed"
            },
            format!("git {operation} refused the operation"),
        ));
    }
    let (bytes, exceeded) = out
        .map_err(|_| LocalGitError::after_start(operation, ref_effect))?
        .map_err(|_| LocalGitError::after_start(operation, ref_effect))?;
    input_result
        .map_err(|_| LocalGitError::after_start(operation, ref_effect))?
        .map_err(|_| LocalGitError::after_start(operation, ref_effect))?;
    if exceeded {
        return Err(if ref_effect {
            LocalGitError::after_start(operation, true)
        } else {
            LocalGitError::new("output_limit", "git output exceeds the whole-blob limit")
        });
    }
    Ok(GitOutput {
        stdout: bytes,
        exit_code,
    })
}

async fn blocking<T, F>(operation: &'static str, ref_effect: bool, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| LocalGitError::after_start(operation, ref_effect))?
}

async fn run_async(repo: &Path, argv: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let repo = repo.to_path_buf();
    let argv: Vec<String> = argv.iter().map(|value| (*value).to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        run_git(&repo, &args, input.as_deref(), None, false)
    })
    .await
    .map_err(|_| LocalGitError::new("git_failed", "git object worker did not complete"))?
}

fn validate_oid(value: &str, field: &str) -> Result<()> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LocalGitError::new(
            "invalid_params",
            format!("{field} must be a 40-hex SHA"),
        ));
    }
    Ok(())
}

fn oid_output(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| LocalGitError::new("git_output", "git returned a non-UTF-8 object id"))?
        .trim();
    if text.len() != 40 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LocalGitError::new(
            "git_output",
            "git did not return one SHA-1 object id",
        ));
    }
    Ok(text.to_ascii_lowercase())
}

fn checked_ref(reference: &str) -> Result<String> {
    validate_ref_name("ref", reference)
        .map_err(|error| LocalGitError::new("invalid_params", error.to_string()))?;
    Ok(format!("{reference}^{{commit}}"))
}

fn branch_ref(branch: &str) -> Result<String> {
    validate_ref_name("branch", branch)
        .map_err(|error| LocalGitError::new("invalid_params", error.to_string()))?;
    Ok(format!("refs/heads/{branch}"))
}

fn resolve_commit_sync(repo: &Path, reference: &str) -> Result<String> {
    let reference = checked_ref(reference)?;
    oid_output(&run_git(
        repo,
        &["rev-parse", "--verify", "--end-of-options", &reference],
        None,
        None,
        false,
    )?)
}

fn require_direct_ref(repo: &Path, reference: &str) -> Result<()> {
    let output = run_git_output(
        repo,
        &["symbolic-ref", "--quiet", "--no-recurse", reference],
        None,
        None,
        false,
        Some(1),
    )?;
    if output.exit_code == 0 {
        return Err(LocalGitError::new(
            "ref_symbolic",
            "symbolic branch targets are refused",
        ));
    }
    Ok(())
}

pub(crate) async fn resolve_commit(repo: &Path, reference: &str) -> Result<String> {
    let repo = repo.to_path_buf();
    let reference = reference.to_string();
    blocking("rev-parse", false, move || {
        resolve_commit_sync(&repo, &reference)
    })
    .await
}

fn branch_head_sync(repo: &Path, branch: &str) -> Result<String> {
    let reference = branch_ref(branch)?;
    require_direct_ref(repo, &reference)?;
    resolve_commit_sync(repo, &reference).map_err(|error| {
        if error.code() == "git_failed" {
            LocalGitError::new(
                "expected_head_mismatch",
                "branch has no resolvable commit head",
            )
        } else {
            error
        }
    })
}

pub(crate) async fn branch_head(repo: &Path, branch: &str) -> Result<String> {
    let repo = repo.to_path_buf();
    let branch = branch.to_string();
    blocking("rev-parse", false, move || branch_head_sync(&repo, &branch)).await
}

#[derive(Debug)]
struct ListedBlob {
    path: String,
    oid: String,
    mode: u32,
}

fn parse_listing(bytes: &[u8]) -> Result<Vec<ListedBlob>> {
    if !bytes.is_empty() && bytes.last() != Some(&0) {
        return Err(LocalGitError::new(
            "git_output",
            "unterminated ls-tree record",
        ));
    }
    let mut entries = Vec::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| LocalGitError::new("git_output", "malformed ls-tree record"))?;
        let header = std::str::from_utf8(&record[..tab])
            .map_err(|_| LocalGitError::new("git_output", "invalid ls-tree metadata"))?;
        let fields: Vec<&str> = header.split(' ').collect();
        if fields.len() != 3 {
            return Err(LocalGitError::new(
                "git_output",
                "malformed ls-tree metadata",
            ));
        }
        let mode = match (fields[0], fields[1]) {
            ("100644", "blob") => 644,
            ("100755", "blob") => 755,
            _ => {
                return Err(LocalGitError::new(
                    "unsupported_entry",
                    "checkout refuses symlinks, submodules, and unsupported tree entries",
                ))
            }
        };
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| LocalGitError::new("unsupported_entry", "tree paths must be UTF-8"))?;
        let path = tree::validate_relative_path(path, "entry")?;
        validate_oid(fields[2], "tree entry object id")?;
        entries.push(ListedBlob {
            path,
            oid: fields[2].to_string(),
            mode,
        });
    }
    Ok(entries)
}

pub(crate) async fn checkout(rt: &KhiveRuntime, repo: &Path, reference: &str) -> Result<Checkout> {
    let reference = checked_ref(reference)?;
    let commit = oid_output(
        &run_async(
            repo,
            &["rev-parse", "--verify", "--end-of-options", &reference],
            None,
        )
        .await?,
    )?;
    let listing = run_async(repo, &["ls-tree", "-r", "-z", &commit], None).await?;
    // Validate every mode and path before producing any manifest entries.
    let listed = parse_listing(&listing)?;
    let store = tree::blob_store(rt)?;
    let mut entries = Vec::with_capacity(listed.len());
    for entry in listed {
        let bytes = run_async(repo, &["cat-file", "blob", &entry.oid], None).await?;
        let content_ref = store.put(bytes).await?;
        entries.push(TreeEntry {
            path: entry.path,
            content_ref: content_ref.as_str().to_string(),
            mode: entry.mode,
        });
    }
    let value = tree::entries_json(&entries);
    let manifest = serde_json::json!({"schema": "khive-tree/v1", "entries": &value});
    if manifest.to_string().len() as u64 > tree::MAX_MANIFEST_BYTES {
        return Err(LocalGitError::new(
            "output_limit",
            "checkout manifest exceeds the tree limit",
        ));
    }
    let tree = tree::store_from_value(rt, &value).await?;
    Ok(Checkout { commit, tree })
}

struct GitEntry {
    name: String,
    mode: &'static str,
    kind: &'static str,
    oid: String,
}

fn parent_and_name(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn mktree_input(entries: &[GitEntry]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in entries {
        bytes
            .extend_from_slice(format!("{} {} {}\t", entry.mode, entry.kind, entry.oid).as_bytes());
        bytes.extend_from_slice(entry.name.as_bytes());
        bytes.push(0);
    }
    bytes
}

pub(crate) async fn write_manifest_tree(
    rt: &KhiveRuntime,
    repo: &Path,
    manifest_ref: &str,
) -> Result<String> {
    let entries = tree::load(rt, manifest_ref).await?;
    tree::verify_blobs(rt, &entries).await?;
    let store = tree::blob_store(rt)?;
    let mut directories: BTreeMap<String, Vec<GitEntry>> = BTreeMap::new();
    directories.insert(String::new(), Vec::new());
    for entry in entries {
        let content_ref = ContentRef::from_hex(&entry.content_ref)
            .map_err(|error| LocalGitError::new("invalid_params", error))?;
        let bytes = store
            .get_bounded_verified(&content_ref, MAX_BLOB_WHOLE_BYTES)
            .await?;
        let oid = oid_output(
            &run_async(
                repo,
                &["hash-object", "-w", "--no-filters", "--stdin"],
                Some(bytes),
            )
            .await?,
        )?;
        let (parent, name) = parent_and_name(&entry.path);
        directories
            .entry(parent.to_string())
            .or_default()
            .push(GitEntry {
                name: name.to_string(),
                mode: if entry.mode == 755 {
                    "100755"
                } else {
                    "100644"
                },
                kind: "blob",
                oid,
            });
        let mut ancestor = parent;
        while !ancestor.is_empty() {
            ancestor = parent_and_name(ancestor).0;
            directories.entry(ancestor.to_string()).or_default();
        }
    }
    let mut paths: Vec<String> = directories.keys().cloned().collect();
    // Descendants are longer than their parents; avoid recursive stack growth.
    paths.sort_by_key(|path| std::cmp::Reverse(path.len()));
    for path in paths {
        let entries = directories
            .remove(&path)
            .expect("directory came from the same map");
        let oid =
            oid_output(&run_async(repo, &["mktree", "-z"], Some(mktree_input(&entries))).await?)?;
        if path.is_empty() {
            return Ok(oid);
        }
        let (parent, name) = parent_and_name(&path);
        directories
            .entry(parent.to_string())
            .or_default()
            .push(GitEntry {
                name: name.to_string(),
                mode: "040000",
                kind: "tree",
                oid,
            });
    }
    Err(LocalGitError::new(
        "git_output",
        "tree construction did not produce a root",
    ))
}

fn create_commit_sync(
    repo: &Path,
    git_tree: &str,
    parent: &str,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> Result<String> {
    validate_oid(git_tree, "git tree")?;
    validate_oid(parent, "expected_head")?;
    validate_message(message)
        .map_err(|error| LocalGitError::new("invalid_params", error.to_string()))?;
    for (field, value) in [("author name", author_name), ("author email", author_email)] {
        if value.trim().is_empty()
            || value
                .chars()
                .any(|c| c.is_control() || matches!(c, '<' | '>'))
        {
            return Err(LocalGitError::new(
                "actor_unmapped",
                format!("mapped {field} is invalid"),
            ));
        }
    }
    oid_output(&run_git(
        repo,
        &["commit-tree", git_tree, "-p", parent, "-m", message],
        None,
        Some((author_name, author_email)),
        false,
    )?)
}

pub(crate) async fn create_commit(
    repo: &Path,
    git_tree: &str,
    parent: &str,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> Result<String> {
    let repo = repo.to_path_buf();
    let git_tree = git_tree.to_string();
    let parent = parent.to_string();
    let message = message.to_string();
    let author_name = author_name.to_string();
    let author_email = author_email.to_string();
    blocking("commit-tree", false, move || {
        create_commit_sync(
            &repo,
            &git_tree,
            &parent,
            &message,
            &author_name,
            &author_email,
        )
    })
    .await
}

fn receipt_marker(receipt_id: &str) -> Result<String> {
    let id = uuid::Uuid::parse_str(receipt_id)
        .map_err(|_| LocalGitError::new("invalid_params", "receipt_id must be a canonical UUID"))?;
    // Require the persisted UUID representation, avoiding alternate spellings of a marker.
    if id.hyphenated().to_string() != receipt_id {
        return Err(LocalGitError::new(
            "invalid_params",
            "receipt_id must be a canonical UUID",
        ));
    }
    Ok(format!("khive-receipt:{receipt_id}"))
}

fn update_branch_sync(
    repo: &Path,
    branch: &str,
    new: &str,
    expected: &str,
    receipt_id: &str,
) -> Result<()> {
    validate_oid(new, "new head")?;
    if new == ZERO_OID {
        return Err(LocalGitError::new(
            "invalid_params",
            "new head must not be the zero object id",
        ));
    }
    validate_oid(expected, "expected_head")?;
    let reference = branch_ref(branch)?;
    let marker = receipt_marker(receipt_id)?;
    require_direct_ref(repo, &reference)?;
    // A symref installed after inspection must never redirect the ref-store write.
    run_git(
        repo,
        &[
            "update-ref",
            "--no-deref",
            "--create-reflog",
            "-m",
            &marker,
            &reference,
            new,
            expected,
        ],
        None,
        None,
        true,
    )?;
    Ok(())
}

pub(crate) async fn update_branch(
    repo: &Path,
    branch: &str,
    new: &str,
    expected: &str,
    receipt_id: &str,
) -> Result<()> {
    let repo = repo.to_path_buf();
    let branch = branch.to_string();
    let new = new.to_string();
    let expected = expected.to_string();
    let receipt_id = receipt_id.to_string();
    blocking("update-ref", true, move || {
        update_branch_sync(&repo, &branch, &new, &expected, &receipt_id)
    })
    .await
}

pub(crate) async fn create_branch_ref(
    repo: &Path,
    branch: &str,
    new: &str,
    receipt_id: &str,
) -> Result<()> {
    update_branch(repo, branch, new, ZERO_OID, receipt_id).await
}

fn parse_operation_recorded(bytes: &[u8], new_sha: &str, marker: &str) -> Result<bool> {
    // tformat:%H%x00%gs%x00 emits SHA NUL message NUL LF for every entry.
    // Validate the entire bounded output before using even an earlier matching entry.
    let invalid = || LocalGitError::new("git_output", "malformed reflog output");
    let mut remaining = bytes;
    let mut found = false;
    while !remaining.is_empty() {
        let end = remaining
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(invalid)?;
        let oid = &remaining[..end];
        if oid.len() != 40 || !oid.iter().all(u8::is_ascii_hexdigit) {
            return Err(invalid());
        }
        remaining = &remaining[end + 1..];
        let end = remaining
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(invalid)?;
        let message = &remaining[..end];
        remaining = remaining[end + 1..]
            .strip_prefix(b"\n")
            .ok_or_else(invalid)?;
        found |= oid.eq_ignore_ascii_case(new_sha.as_bytes()) && message == marker.as_bytes();
    }
    Ok(found)
}

/// Inspect Amendment 5 evidence: an exact receipt marker and new SHA, with that
/// SHA equal to or an ancestor of the named ref's current head. This is not a
/// claim of crash-atomicity for the ref backend.
pub(crate) async fn operation_recorded(
    repo: &Path,
    branch: &str,
    new_sha: &str,
    receipt_id: &str,
) -> Result<bool> {
    validate_oid(new_sha, "new head")?;
    let reference = branch_ref(branch)?;
    let marker = receipt_marker(receipt_id)?;
    let repo = repo.to_path_buf();
    let new_sha = new_sha.to_string();
    blocking("reflog", false, move || {
        require_direct_ref(&repo, &reference)?;
        let exists = run_git_output(
            &repo,
            &["reflog", "exists", &reference],
            None,
            None,
            false,
            Some(1),
        )?;
        if exists.exit_code == 1 {
            return Ok(false);
        }
        let bytes = run_git(
            &repo,
            &[
                "reflog",
                "show",
                "--format=tformat:%H%x00%gs%x00",
                "--no-abbrev-commit",
                "--no-decorate",
                "--no-color",
                "--no-patch",
                "--no-show-signature",
                "--no-notes",
                "--no-ext-diff",
                "--no-textconv",
                &reference,
                "--",
            ],
            None,
            None,
            false,
        )?;
        if !parse_operation_recorded(&bytes, &new_sha, &marker)? {
            return Ok(false);
        }
        require_direct_ref(&repo, &reference)?;
        // Resolve the head once, then make the ancestry query against immutable IDs.
        let head = resolve_commit_sync(&repo, &reference)?;
        if head.eq_ignore_ascii_case(&new_sha) {
            return Ok(true);
        }
        let ancestry = run_git_output(
            &repo,
            &["merge-base", "--is-ancestor", &new_sha, &head],
            None,
            None,
            false,
            Some(1),
        )?;
        Ok(ancestry.exit_code == 0)
    })
    .await
}

fn parse_numstat(bytes: &[u8]) -> Result<DiffSummary> {
    let mut summary = DiffSummary::default();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mut fields = line.splitn(3, |byte| *byte == b'\t');
        let invalid = || LocalGitError::new("git_output", "malformed diff numstat");
        let added = fields.next().ok_or_else(invalid)?;
        let deleted = fields.next().ok_or_else(invalid)?;
        if fields.next().is_none_or(|path| path.is_empty()) {
            return Err(invalid());
        }
        let counts = if added == b"-" && deleted == b"-" {
            (0, 0)
        } else {
            let count = |field: &[u8]| -> Result<u64> {
                std::str::from_utf8(field)
                    .map_err(|_| invalid())?
                    .parse::<u64>()
                    .map_err(|_| invalid())
            };
            (count(added)?, count(deleted)?)
        };
        summary.files = summary.files.checked_add(1).ok_or_else(invalid)?;
        summary.additions = summary
            .additions
            .checked_add(counts.0)
            .ok_or_else(invalid)?;
        summary.deletions = summary
            .deletions
            .checked_add(counts.1)
            .ok_or_else(invalid)?;
    }
    Ok(summary)
}

pub(crate) async fn diff(
    rt: &KhiveRuntime,
    repo: &Path,
    input_kind: &str,
    base: &str,
    head: &str,
) -> Result<DiffResult> {
    let scratch;
    let (working_repo, left, right) = match input_kind {
        "commits" => {
            let base_ref = checked_ref(base)?;
            let head_ref = checked_ref(head)?;
            let left = oid_output(
                &run_async(
                    repo,
                    &["rev-parse", "--verify", "--end-of-options", &base_ref],
                    None,
                )
                .await?,
            )?;
            let right = oid_output(
                &run_async(
                    repo,
                    &["rev-parse", "--verify", "--end-of-options", &head_ref],
                    None,
                )
                .await?,
            )?;
            (repo, left, right)
        }
        "trees" => {
            scratch = tempfile::Builder::new()
                .prefix("khive-git-diff-")
                .tempdir()
                .map_err(|_| {
                    LocalGitError::new("scratch_error", "could not create scratch repository")
                })?;
            run_async(
                scratch.path(),
                &["init", "--bare", "--template=", "--object-format=sha1"],
                None,
            )
            .await?;
            let left = write_manifest_tree(rt, scratch.path(), base).await?;
            let right = write_manifest_tree(rt, scratch.path(), head).await?;
            (scratch.path(), left, right)
        }
        _ => {
            return Err(LocalGitError::new(
                "invalid_params",
                "input_kind must be commits or trees",
            ))
        }
    };
    let bytes = run_async(
        working_repo,
        &[
            "diff-tree",
            "-p",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            &left,
            &right,
        ],
        None,
    )
    .await?;
    let numstat = run_async(
        working_repo,
        &[
            "diff-tree",
            "--numstat",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            &left,
            &right,
        ],
        None,
    )
    .await?;
    let summary = parse_numstat(&numstat)?;
    let content_ref = tree::blob_store(rt)?.put(bytes).await?;
    Ok(DiffResult {
        base: base.to_string(),
        head: head.to_string(),
        diff: content_ref.as_str().to_string(),
        summary,
    })
}

pub(crate) async fn is_ancestor(repo: &Path, base: &str, head: &str) -> Result<bool> {
    validate_oid(base, "base")?;
    validate_oid(head, "head")?;
    let (repo, base, head) = (repo.to_path_buf(), base.to_string(), head.to_string());
    blocking("merge-base", false, move || {
        Ok(run_git_output(
            &repo,
            &["merge-base", "--is-ancestor", &base, &head],
            None,
            None,
            false,
            Some(1),
        )?
        .exit_code
            == 0)
    })
    .await
}

pub(crate) async fn object_directory(repo: &Path) -> Result<std::path::PathBuf> {
    let repo = repo.to_path_buf();
    blocking("rev-parse", false, move || {
        let bytes = run_git(
            &repo,
            &["rev-parse", "--git-path", "objects"],
            None,
            None,
            false,
        )?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| LocalGitError::new("git_output", "invalid object path"))?
            .trim();
        let path = repo.join(text);
        std::fs::canonicalize(path)
            .map_err(|_| LocalGitError::new("git_output", "object directory unavailable"))
    })
    .await
}

pub(crate) async fn record_push_marker(
    repo: &Path,
    branch: &str,
    sha: &str,
    receipt_id: &str,
) -> Result<()> {
    validate_oid(sha, "pushed head")?;
    let reference = branch_ref(branch)?;
    let marker = receipt_marker(receipt_id)?;
    let repo = repo.to_path_buf();
    let sha = sha.to_string();
    blocking("reflog", false, move || {
        require_direct_ref(&repo, &reference)?;
        // update-ref suppresses reflog writes for an unchanged SHA. An explicit
        // reflog write records acknowledgement without changing any source ref.
        run_git(
            &repo,
            &["reflog", "write", &reference, &sha, &sha, &marker],
            None,
            None,
            false,
        )?;
        Ok(())
    })
    .await
}

pub(crate) async fn push_marker_support(repo: &Path) -> Result<(String, bool)> {
    let repo = repo.to_path_buf();
    blocking("reflog", false, move || {
        let version = run_git(&repo, &["--version"], None, None, false)?;
        let version = String::from_utf8(version)
            .map_err(|_| LocalGitError::new("git_output", "invalid Git version"))?;
        let help = run_git_output(&repo, &["reflog", "-h"], None, None, false, Some(129))?;
        Ok((
            version.trim().to_owned(),
            String::from_utf8_lossy(&help.stdout).contains("git reflog write "),
        ))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_preserves_delimiters_and_refuses_unsupported_modes() {
        let oid = "1234567890123456789012345678901234567890";
        let entries = parse_listing(format!("100755 blob {oid}\ta\tb\nc\0").as_bytes()).unwrap();
        assert_eq!(entries[0].path, "a\tb\nc");
        assert_eq!(entries[0].mode, 755);
        for metadata in ["120000 blob", "160000 commit"] {
            assert_eq!(
                parse_listing(format!("{metadata} {oid}\tx\0").as_bytes())
                    .unwrap_err()
                    .code(),
                "unsupported_entry"
            );
        }
        assert!(parse_listing(format!("100644 blob {oid}\tx").as_bytes()).is_err());
    }

    #[test]
    fn numstat_counts_binary_files_without_inventing_line_counts() {
        assert_eq!(
            parse_numstat(b"3\t2\ta\n-\t-\timage\n0\t0\tmode\n").unwrap(),
            DiffSummary {
                files: 3,
                additions: 3,
                deletions: 2,
            }
        );
        assert!(parse_numstat(b"3\t-\ta\n").is_err());
        assert!(parse_numstat(b"18446744073709551615\t0\ta\n1\t0\tb\n").is_err());
    }

    #[test]
    fn object_input_is_nul_framed_without_path_reinterpretation() {
        let entry = GitEntry {
            name: "tab\tline\n".into(),
            mode: "100644",
            kind: "blob",
            oid: ZERO_OID.into(),
        };
        assert_eq!(
            mktree_input(&[entry]),
            format!("100644 blob {ZERO_OID}\ttab\tline\n\0").into_bytes()
        );
    }

    #[test]
    fn only_exact_cas_diagnostics_establish_no_effect() {
        let new = "1".repeat(40);
        let expected = "2".repeat(40);
        let actual = "3".repeat(40);
        let argv = [
            "update-ref",
            "--no-deref",
            "--create-reflog",
            "-m",
            "khive-receipt:12345678-1234-4234-8234-123456789012",
            "refs/heads/work",
            &new,
            &expected,
        ];
        let prefix = "fatal: update_ref failed for ref 'refs/heads/work': cannot lock ref 'refs/heads/work': ";
        assert!(cas_refused(
            &argv,
            format!("{prefix}is at {actual} but expected {expected}\n").as_bytes()
        ));
        assert!(!cas_refused(
            &argv,
            format!("{prefix}unable to append to HEAD log\n").as_bytes()
        ));
        assert!(!cas_refused(
            &argv,
            format!("warning: config error\n{prefix}is at {actual} but expected {expected}\n")
                .as_bytes()
        ));
        let create = [
            "update-ref",
            "--no-deref",
            "--create-reflog",
            "-m",
            "khive-receipt:12345678-1234-4234-8234-123456789012",
            "refs/heads/work",
            &new,
            ZERO_OID,
        ];
        assert!(cas_refused(
            &create,
            format!("{prefix}reference already exists\n").as_bytes()
        ));
        assert!(!cas_refused(
            &argv,
            format!("{prefix}reference already exists\n").as_bytes()
        ));
    }

    #[test]
    fn reflog_evidence_requires_exact_marker_and_new_sha() {
        let sha = "a".repeat(40);
        let other_sha = "b".repeat(40);
        let marker = receipt_marker("12345678-1234-4234-8234-123456789012").unwrap();
        let record = |oid: &str, message: &str| format!("{oid}\0{message}\0\n").into_bytes();
        assert!(parse_operation_recorded(&record(&sha, &marker), &sha, &marker).unwrap());
        assert!(!parse_operation_recorded(&record(&other_sha, &marker), &sha, &marker).unwrap());
        for message in [
            format!("{marker}-extra"),
            format!("prefix {marker}"),
            "rival".into(),
        ] {
            assert!(!parse_operation_recorded(&record(&sha, &message), &sha, &marker).unwrap());
        }
        let mut history = record(&other_sha, "rival");
        history.extend(record(&sha, &marker));
        assert!(parse_operation_recorded(&history, &sha, &marker).unwrap());
        history.extend_from_slice(b"truncated");
        assert!(parse_operation_recorded(&history, &sha, &marker).is_err());
        assert!(!parse_operation_recorded(b"", &sha, &marker).unwrap());
    }

    #[test]
    fn reflog_evidence_rejects_missing_framing_and_bad_object_ids() {
        let sha = "a".repeat(40);
        let marker = "khive-receipt:12345678-1234-4234-8234-123456789012";
        for invalid in [
            format!("{sha}\0{marker}\0"),
            format!("{sha}\0{marker}\n"),
            format!("short\0{marker}\0\n"),
            format!("{}\0{marker}\0\n", "z".repeat(40)),
        ] {
            assert!(parse_operation_recorded(invalid.as_bytes(), &sha, marker).is_err());
        }
    }

    #[test]
    fn receipt_markers_reject_noncanonical_or_injected_ids() {
        for invalid in [
            "",
            "12345678123442348234123456789012",
            "12345678-1234-4234-8234-12345678901A",
            "12345678-1234-4234-8234-123456789012\nforged",
        ] {
            assert!(receipt_marker(invalid).is_err());
        }
        assert!(receipt_marker("12345678-1234-4234-8234-123456789012").is_ok());
    }

    #[test]
    fn required_compare_never_accepts_empty_or_non_sha() {
        for invalid in ["", "HEAD", "null", "1234", "-1"] {
            assert!(validate_oid(invalid, "expected_head").is_err());
        }
        assert!(validate_oid(ZERO_OID, "expected_head").is_ok());
    }
}
