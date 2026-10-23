//! Local object plumbing. Callers own authorization, receipts, and canonical repo identity.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use khive_runtime::blob_tree::{self as tree, TreeEntry};
use khive_runtime::{KhiveRuntime, RuntimeError, VerifiedBlob};
use khive_storage::{ContentRef, MAX_BLOB_WHOLE_BYTES};
use serde::Serialize;

use crate::git_env;
use crate::write_argv::{validate_message, validate_ref_name};

#[path = "local_git_checkout_batch.rs"]
mod checkout_batch;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
/// Settings only local invocations carry, passed after `git_env::SHARED_SETTINGS`.
const HARDENING: &[&str] = &[
    // Signature display and verification run a program the REPOSITORY names. `log` reads
    // `log.showSignature`, and every verifier path resolves through one of the `gpg*.program`
    // keys, so a repository whose config points them at a script executes that script the moment
    // a read verb touches it. The read verbs never report a signature, so nothing here is a
    // feature being turned off: this closes a program-execution door the verbs never used.
    "log.showSignature=false",
    "merge.verifySignatures=false",
    "gpg.program=/usr/bin/false",
    "gpg.openpgp.program=/usr/bin/false",
    "gpg.x509.program=/usr/bin/false",
    "gpg.ssh.program=/usr/bin/false",
];

/// Config keys that neutralise one content-filter driver, in the form `git -c` takes.
///
/// A clean, smudge or process filter is a program named by repository config, and git runs it
/// whenever it has to convert worktree content: `status` hashes a file whose stat data changed,
/// so the driver executes on a plain read. There is no single switch that turns filtering off,
/// and driver names are arbitrary, so the drivers the repository DECLARES are enumerated and each
/// one is overridden. An empty command is git's own "no filter" (`convert.c` applies a driver
/// only `if (cmd && *cmd)`), and `required=false` keeps the empty driver from being fatal.
///
/// The cost of this is honest and worth naming: content that a filter would have converted is
/// read as the bytes on disk, so a repository using a required filter can report a file as
/// modified that its own `git status` calls clean. Reading a repository must not run its code.
const FILTER_NEUTRALIZED: &[&str] = &["clean", "smudge", "process"];

fn filter_overrides(program: &Path, repo: &Path) -> Result<Vec<String>> {
    let mut command = base_command(program);
    command
        .arg("-C")
        .arg(repo)
        .args(["config", "-z", "--get-regexp", "--name-only", "^filter\\."])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = khive_runtime::process_retry::spawn_retrying_executable_busy(
        &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
        || command.spawn(),
    )
    .and_then(|child| child.wait_with_output())
    .map_err(|_| LocalGitError::new("git_config", "could not enumerate repository filters"))?;
    match output.status.code() {
        Some(0) => {}
        Some(1) => return Ok(Vec::new()),
        _ => {
            return Err(LocalGitError::new(
                "git_config",
                "repository filter enumeration did not complete successfully",
            ))
        }
    }
    let mut drivers: Vec<String> = Vec::new();
    for name in String::from_utf8_lossy(&output.stdout).split('\0') {
        // `filter.<driver>.<key>`; a driver name is a config subsection and may itself hold dots,
        // so the KEY is split from the right and everything between is the driver.
        let Some(rest) = name.strip_prefix("filter.") else {
            continue;
        };
        let Some((driver, _)) = rest.rsplit_once('.') else {
            continue;
        };
        if !driver.is_empty() && !drivers.iter().any(|seen| seen == driver) {
            drivers.push(driver.to_owned());
        }
    }
    let mut overrides = Vec::with_capacity(drivers.len() * (FILTER_NEUTRALIZED.len() + 1));
    for driver in drivers {
        for key in FILTER_NEUTRALIZED {
            overrides.push(format!("filter.{driver}.{key}="));
        }
        overrides.push(format!("filter.{driver}.required=false"));
    }
    Ok(overrides)
}

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

/// The environment and config every git invocation runs under, with no repository selected yet.
///
/// Split out so the config READ that enumerates filter drivers runs under the same hardening as
/// the operation it is hardening, without recursing into the enumeration it exists to feed.
fn base_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    git_env::apply_shared_env(&mut command);
    // Empty is a portable graft-file override that does not emit Git's deprecation hint.
    command.env("GIT_GRAFT_FILE", "");
    command.env("GIT_OPTIONAL_LOCKS", "0");
    for setting in git_env::SHARED_SETTINGS.iter().chain(HARDENING) {
        command.arg("-c").arg(setting);
    }
    command
}

pub(crate) fn git_command(
    program: &Path,
    repo: &Path,
    argv: &[&str],
    identity: Option<(&str, &str)>,
) -> Result<Command> {
    let overrides = filter_overrides(program, repo)?;
    Ok(git_command_with_overrides(
        program, repo, argv, identity, &overrides,
    ))
}

fn git_command_with_overrides(
    program: &Path,
    repo: &Path,
    argv: &[&str],
    identity: Option<(&str, &str)>,
    overrides: &[String],
) -> Command {
    let mut command = base_command(program);
    if let Some((name, email)) = identity {
        command
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email);
    }
    // Every invocation, not only the ones known today to convert content: a verb added later that
    // reads or writes the worktree inherits this rather than having to remember it.
    for setting in overrides {
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
    stderr: Vec<u8>,
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
    crate::object_id::is_40_hex(observed.as_bytes())
}

fn run_git(
    program: &Path,
    repo: &Path,
    argv: &[&str],
    input: Option<&[u8]>,
    identity: Option<(&str, &str)>,
    ref_effect: bool,
) -> Result<Vec<u8>> {
    run_git_output(program, repo, argv, input, identity, ref_effect, None)
        .map(|output| output.stdout)
}

fn run_git_output(
    program: &Path,
    repo: &Path,
    argv: &[&str],
    input: Option<&[u8]>,
    identity: Option<(&str, &str)>,
    ref_effect: bool,
    allowed_exit: Option<i32>,
) -> Result<GitOutput> {
    let command = git_command(program, repo, argv, identity)?;
    run_git_command_output(command, argv, input, ref_effect, allowed_exit)
}

fn run_git_command_output(
    mut command: Command,
    argv: &[&str],
    input: Option<&[u8]>,
    ref_effect: bool,
    allowed_exit: Option<i32>,
) -> Result<GitOutput> {
    let operation = argv.first().copied().unwrap_or("operation");
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = khive_runtime::process_retry::spawn_retrying_executable_busy(
        &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
        || command.spawn(),
    )
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
        // #2539: the exit status is the only bounded, non-sensitive fact that
        // separates a lock contention from a usage error, and without it a
        // classified refusal teaches nothing on the run that produced it. stderr
        // stays where it is: retained for classification, never exposed.
        return Err(LocalGitError::new(
            if ref_effect {
                "not_committed"
            } else {
                "git_failed"
            },
            format!("git {operation} refused the operation with exit status {exit_code}"),
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
        stderr,
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

async fn run_async(
    program: &Path,
    repo: &Path,
    argv: &[&str],
    input: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let argv: Vec<String> = argv.iter().map(|value| (*value).to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        run_git(&program, &repo, &args, input.as_deref(), None, false)
    })
    .await
    .map_err(|_| LocalGitError::new("git_failed", "git object worker did not complete"))?
}

/// Keep hydration admission while the blocking git writer consumes stdin.
async fn run_async_hydrated(
    program: &Path,
    repo: &Path,
    argv: &[&str],
    input: VerifiedBlob,
    filters: &mut Option<std::sync::Arc<[String]>>,
) -> Result<Vec<u8>> {
    let overrides = operation_filter_snapshot(program, repo, filters).await?;
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let argv: Vec<String> = argv.iter().map(|value| (*value).to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let command = git_command_with_overrides(&program, &repo, &args, None, &overrides);
        run_git_command_output(command, &args, Some(input.bytes()), false, None)
            .map(|output| output.stdout)
    })
    .await
    .map_err(|_| LocalGitError::new("git_failed", "git object worker did not complete"))?
}

async fn operation_filter_snapshot(
    program: &Path,
    repo: &Path,
    filters: &mut Option<std::sync::Arc<[String]>>,
) -> Result<std::sync::Arc<[String]>> {
    if let Some(filters) = filters {
        return Ok(std::sync::Arc::clone(filters));
    }
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let settings = tokio::task::spawn_blocking(move || filter_overrides(&program, &repo))
        .await
        .map_err(|_| {
            LocalGitError::new("git_config", "repository filter worker did not complete")
        })??;
    let settings: std::sync::Arc<[String]> = settings.into();
    *filters = Some(std::sync::Arc::clone(&settings));
    Ok(settings)
}

async fn run_async_with_snapshot(
    program: &Path,
    repo: &Path,
    argv: &[&str],
    input: Option<Vec<u8>>,
    filters: &mut Option<std::sync::Arc<[String]>>,
) -> Result<Vec<u8>> {
    let overrides = operation_filter_snapshot(program, repo, filters).await?;
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let argv: Vec<String> = argv.iter().map(|value| (*value).to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let command = git_command_with_overrides(&program, &repo, &args, None, &overrides);
        run_git_command_output(command, &args, input.as_deref(), false, None)
            .map(|output| output.stdout)
    })
    .await
    .map_err(|_| LocalGitError::new("git_failed", "git object worker did not complete"))?
}

fn validate_oid(value: &str, field: &str) -> Result<()> {
    if !crate::object_id::is_40_hex(value.as_bytes()) {
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
    if !crate::object_id::is_40_hex(text.as_bytes()) {
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

fn resolve_commit_sync(program: &Path, repo: &Path, reference: &str) -> Result<String> {
    let reference = checked_ref(reference)?;
    oid_output(&run_git(
        program,
        repo,
        &["rev-parse", "--verify", "--end-of-options", &reference],
        None,
        None,
        false,
    )?)
}

fn require_direct_ref(program: &Path, repo: &Path, reference: &str) -> Result<()> {
    let output = run_git_output(
        program,
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

pub(crate) async fn resolve_commit(program: &Path, repo: &Path, reference: &str) -> Result<String> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let reference = reference.to_string();
    blocking("rev-parse", false, move || {
        resolve_commit_sync(&program, &repo, &reference)
    })
    .await
}

pub(crate) async fn resolve_commit_object_id(
    program: &Path,
    repo: &Path,
    object_id: &str,
) -> Result<String> {
    validate_oid(object_id, "to")?;
    if object_id == ZERO_OID {
        return Err(LocalGitError::new(
            "invalid_params",
            "to must not be the zero object id",
        ));
    }
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let object_id = object_id.to_string();
    blocking("cat-file", false, move || {
        let kind = run_git(
            &program,
            &repo,
            &["cat-file", "-t", &object_id],
            None,
            None,
            false,
        )
        .map_err(|error| {
            if error.code() == "git_failed" {
                LocalGitError::new("invalid_params", "to must identify an existing commit")
            } else {
                error
            }
        })?;
        if kind != b"commit\n" {
            return Err(LocalGitError::new(
                "invalid_params",
                "to must identify a commit object",
            ));
        }
        Ok(object_id.to_ascii_lowercase())
    })
    .await
}

fn branch_head_sync(program: &Path, repo: &Path, branch: &str) -> Result<String> {
    let reference = branch_ref(branch)?;
    require_direct_ref(program, repo, &reference)?;
    resolve_commit_sync(program, repo, &reference).map_err(|error| {
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

pub(crate) async fn branch_head(program: &Path, repo: &Path, branch: &str) -> Result<String> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let branch = branch.to_string();
    blocking("rev-parse", false, move || {
        branch_head_sync(&program, &repo, &branch)
    })
    .await
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
    let program = rt.config().git_write.git_program();
    let reference = checked_ref(reference)?;
    let mut filters = None;
    let commit = oid_output(
        &run_async_with_snapshot(
            program,
            repo,
            &["rev-parse", "--verify", "--end-of-options", &reference],
            None,
            &mut filters,
        )
        .await?,
    )?;
    let listing = run_async_with_snapshot(
        program,
        repo,
        &["ls-tree", "-r", "-z", &commit],
        None,
        &mut filters,
    )
    .await?;
    // Validate every mode and path before producing any manifest entries.
    let listed = parse_listing(&listing)?;
    let store = tree::blob_store(rt)?;
    checkout_batch::admit_manifest(&listed)?;
    let mut entries = Vec::with_capacity(listed.len());
    let mut reader = if listed.is_empty() {
        None
    } else {
        Some(checkout_batch::BlobReader::start(program, repo, &listed, &mut filters).await?)
    };
    for entry in listed {
        let frame = reader
            .as_mut()
            .expect("nonempty tree has a blob reader")
            .next()
            .await?;
        let content_ref = store.put(frame.bytes).await?;
        frame
            .consumed
            .send(())
            .map_err(|_| LocalGitError::after_start("cat-file", false))?;
        entries.push(TreeEntry {
            path: entry.path,
            content_ref: content_ref.as_str().to_string(),
            mode: entry.mode,
        });
    }
    if let Some(reader) = reader {
        reader.finish().await?;
    }
    let value = tree::entries_json(&entries);
    let manifest = serde_json::json!({"schema": tree::TREE_SCHEMA, "entries": &value});
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
    let program = rt.config().git_write.git_program();
    let entries = tree::load(rt, manifest_ref).await?;
    tree::verify_blobs(rt, &entries).await?;
    let hydrator = rt
        .blob_hydrator()
        .ok_or_else(|| RuntimeError::Unconfigured("git blob hydrator is not installed".into()))?;
    let mut directories: BTreeMap<String, Vec<GitEntry>> = BTreeMap::new();
    let mut object_ids = BTreeMap::<ContentRef, String>::new();
    let mut filters = None;
    directories.insert(String::new(), Vec::new());
    for entry in entries {
        let content_ref = ContentRef::from_hex(&entry.content_ref)
            .map_err(|error| LocalGitError::new("invalid_params", error))?;
        let bytes = hydrator
            .hydrate_verified(&content_ref, MAX_BLOB_WHOLE_BYTES)
            .await?;
        let oid = if let Some(oid) = object_ids.get(&content_ref) {
            drop(bytes);
            oid.clone()
        } else {
            let oid = oid_output(
                &run_async_hydrated(
                    program,
                    repo,
                    &["hash-object", "-w", "--no-filters", "--stdin"],
                    bytes,
                    &mut filters,
                )
                .await?,
            )?;
            object_ids.insert(content_ref, oid.clone());
            oid
        };
        let (parent, name) = parent_and_name(&entry.path);
        directories
            .entry(parent.to_string())
            .or_default()
            .push(GitEntry {
                name: name.to_string(),
                mode: match entry.mode {
                    644 => "100644",
                    755 => "100755",
                    120000 => "120000",
                    _ => {
                        return Err(LocalGitError::new(
                            "invalid_params",
                            "tree entry mode must be 644, 755, or 120000",
                        ))
                    }
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
        let oid = oid_output(
            &run_async_with_snapshot(
                program,
                repo,
                &["mktree", "-z"],
                Some(mktree_input(&entries)),
                &mut filters,
            )
            .await?,
        )?;
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
    program: &Path,
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
        program,
        repo,
        &["commit-tree", git_tree, "-p", parent, "-m", message],
        None,
        Some((author_name, author_email)),
        false,
    )?)
}

pub(crate) async fn create_commit(
    program: &Path,
    repo: &Path,
    git_tree: &str,
    parent: &str,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> Result<String> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let git_tree = git_tree.to_string();
    let parent = parent.to_string();
    let message = message.to_string();
    let author_name = author_name.to_string();
    let author_email = author_email.to_string();
    blocking("commit-tree", false, move || {
        create_commit_sync(
            &program,
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
    program: &Path,
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
    require_direct_ref(program, repo, &reference)?;
    if new == expected && !reflog_write_support_sync(program, repo)?.1 {
        // A no-op CAS writes no reflog entry. Refuse before even that CAS if
        // this Git cannot record the separate receipt marker afterward.
        return Err(LocalGitError::new(
            "unsupported_toolchain",
            "Git does not advertise reflog write",
        ));
    }
    // A symref installed after inspection must never redirect the ref-store write.
    run_git(
        program,
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
    if new == expected {
        // Git accepts a compare-and-swap to the current head without writing
        // a reflog entry. Keep the receipt observable for reconciliation even
        // when the successful operation leaves the ref unchanged.
        run_git(
            program,
            repo,
            &["reflog", "write", &reference, new, new, &marker],
            None,
            None,
            false,
        )?;
    }
    Ok(())
}

pub(crate) async fn update_branch(
    program: &Path,
    repo: &Path,
    branch: &str,
    new: &str,
    expected: &str,
    receipt_id: &str,
) -> Result<()> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let branch = branch.to_string();
    let new = new.to_string();
    let expected = expected.to_string();
    let receipt_id = receipt_id.to_string();
    blocking("update-ref", true, move || {
        update_branch_sync(&program, &repo, &branch, &new, &expected, &receipt_id)
    })
    .await
}

pub(crate) async fn create_branch_ref(
    program: &Path,
    repo: &Path,
    branch: &str,
    new: &str,
    receipt_id: &str,
) -> Result<()> {
    update_branch(program, repo, branch, new, ZERO_OID, receipt_id).await
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
        if !crate::object_id::is_40_hex(oid) {
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
    program: &Path,
    repo: &Path,
    branch: &str,
    new_sha: &str,
    receipt_id: &str,
) -> Result<bool> {
    validate_oid(new_sha, "new head")?;
    let reference = branch_ref(branch)?;
    let marker = receipt_marker(receipt_id)?;
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let new_sha = new_sha.to_string();
    blocking("reflog", false, move || {
        require_direct_ref(&program, &repo, &reference)?;
        let exists = run_git_output(
            &program,
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
            &program,
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
        require_direct_ref(&program, &repo, &reference)?;
        // Resolve the head once, then make the ancestry query against immutable IDs.
        let head = resolve_commit_sync(&program, &repo, &reference)?;
        if head.eq_ignore_ascii_case(&new_sha) {
            return Ok(true);
        }
        let ancestry = run_git_output(
            &program,
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
    let program = rt.config().git_write.git_program();
    let scratch;
    let (working_repo, left, right) = match input_kind {
        "commits" => {
            let base_ref = checked_ref(base)?;
            let head_ref = checked_ref(head)?;
            let left = oid_output(
                &run_async(
                    program,
                    repo,
                    &["rev-parse", "--verify", "--end-of-options", &base_ref],
                    None,
                )
                .await?,
            )?;
            let right = oid_output(
                &run_async(
                    program,
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
                program,
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
        program,
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
        program,
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

pub(crate) async fn is_ancestor(
    program: &Path,
    repo: &Path,
    base: &str,
    head: &str,
) -> Result<bool> {
    validate_oid(base, "base")?;
    validate_oid(head, "head")?;
    let program = program.to_path_buf();
    let (repo, base, head) = (repo.to_path_buf(), base.to_string(), head.to_string());
    blocking("merge-base", false, move || {
        Ok(run_git_output(
            &program,
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

pub(crate) async fn object_directory(program: &Path, repo: &Path) -> Result<std::path::PathBuf> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    blocking("rev-parse", false, move || {
        let bytes = run_git(
            &program,
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
    program: &Path,
    repo: &Path,
    branch: &str,
    sha: &str,
    receipt_id: &str,
) -> Result<()> {
    validate_oid(sha, "pushed head")?;
    let reference = branch_ref(branch)?;
    let marker = receipt_marker(receipt_id)?;
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    let sha = sha.to_string();
    blocking("reflog", false, move || {
        require_direct_ref(&program, &repo, &reference)?;
        // update-ref suppresses reflog writes for an unchanged SHA. An explicit
        // reflog write records acknowledgement without changing any source ref.
        run_git(
            &program,
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

fn reflog_write_support_sync(program: &Path, repo: &Path) -> Result<(String, bool)> {
    let version = run_git(program, repo, &["--version"], None, None, false)?;
    let version = String::from_utf8(version)
        .map_err(|_| LocalGitError::new("git_output", "invalid Git version"))?;
    let help = run_git_output(
        program,
        repo,
        &["reflog", "-h"],
        None,
        None,
        false,
        Some(129),
    )?;
    Ok((
        version.trim().to_owned(),
        String::from_utf8_lossy(&help.stdout).contains("git reflog write "),
    ))
}

pub(crate) async fn push_marker_support(program: &Path, repo: &Path) -> Result<(String, bool)> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    blocking("reflog", false, move || {
        reflog_write_support_sync(&program, &repo)
    })
    .await
}

/// One entry of `git status --porcelain=v2 -z`, classified by its leading token.
#[derive(Serialize)]
pub(crate) struct StatusEntry {
    pub kind: &'static str,
    pub xy: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct StatusBranch {
    pub oid: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: Option<i64>,
    pub behind: Option<i64>,
}

pub(crate) struct StatusResult {
    pub branch: StatusBranch,
    pub entries: Vec<StatusEntry>,
    pub total: usize,
}

/// `git status --porcelain=v2 -z` over an allow-listed repository. `GIT_OPTIONAL_LOCKS=0` is
/// already in the hardened environment, so this refreshes nothing and takes no index lock: the
/// working tree and the index are byte-identical before and after. `total` counts every entry git
/// reported, so `total == 0` is a whole-repository claim even when `entries` was capped by `limit`.
pub(crate) async fn status(
    program: &Path,
    repo: &Path,
    untracked: &str,
    limit: usize,
) -> Result<StatusResult> {
    let untracked_arg = match untracked {
        "no" | "normal" | "all" => format!("--untracked-files={untracked}"),
        _ => {
            return Err(LocalGitError::new(
                "invalid_params",
                "untracked must be no, normal or all",
            ))
        }
    };
    let bytes = run_async(
        program,
        repo,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--no-renames",
            &untracked_arg,
            "-z",
        ],
        None,
    )
    .await?;
    parse_status(&bytes, limit)
}

/// Porcelain v2 is NUL-terminated per record, so a path holding a newline, a quote or invalid
/// UTF-8 stays one record. `--no-renames` is passed above, which removes the `2` form whose
/// original path is a second NUL-terminated field; the arm below still refuses it rather than
/// silently attributing the following record's path to it, so a future caller that drops the flag
/// gets an error instead of a wrong answer.
fn parse_status(bytes: &[u8], limit: usize) -> Result<StatusResult> {
    let mut branch = StatusBranch {
        oid: None,
        head: None,
        upstream: None,
        ahead: None,
        behind: None,
    };
    let mut entries = Vec::new();
    let mut total = 0_usize;
    for record in bytes.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(record).map_err(|_| {
            LocalGitError::new("git_failed", "git status emitted a non-UTF-8 record")
        })?;
        if let Some(header) = text.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                // Porcelain v2 spells "no sha yet" and "no branch" as the literal sentinels
                // `(initial)` and `(detached)`. They are reported as null rather than passed
                // through, so a caller reading `branch.head` never has to know the sentinel and a
                // detached head is an absent name rather than a name that looks real.
                "branch.oid" => branch.oid = (value != "(initial)").then(|| value.to_string()),
                "branch.head" => branch.head = (value != "(detached)").then(|| value.to_string()),
                "branch.upstream" => branch.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split_whitespace() {
                        match part.as_bytes().first() {
                            Some(b'+') => branch.ahead = part[1..].parse().ok(),
                            Some(b'-') => branch.behind = part[1..].parse().ok(),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let (token, rest) = text.split_once(' ').unwrap_or((text, ""));
        let entry = match token {
            "1" => {
                let mut fields = rest.splitn(8, ' ');
                let xy = fields.next().unwrap_or_default().to_string();
                let path = fields.nth(6).unwrap_or_default().to_string();
                StatusEntry {
                    kind: "ordinary",
                    xy,
                    path,
                    original_path: None,
                    score: None,
                }
            }
            "u" => {
                // `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`: ten fields after
                // the token, so the path is the ninth item left once XY is consumed.
                let mut fields = rest.splitn(10, ' ');
                let xy = fields.next().unwrap_or_default().to_string();
                let path = fields.nth(8).unwrap_or_default().to_string();
                StatusEntry {
                    kind: "unmerged",
                    xy,
                    path,
                    original_path: None,
                    score: None,
                }
            }
            "?" => StatusEntry {
                kind: "untracked",
                xy: "??".into(),
                path: rest.to_string(),
                original_path: None,
                score: None,
            },
            "!" => StatusEntry {
                kind: "ignored",
                xy: "!!".into(),
                path: rest.to_string(),
                original_path: None,
                score: None,
            },
            "2" => {
                return Err(LocalGitError::new(
                    "git_failed",
                    "git status reported a rename entry although --no-renames was passed",
                ))
            }
            _ => {
                return Err(LocalGitError::new(
                    "git_failed",
                    "git status emitted an unrecognized porcelain v2 record",
                ))
            }
        };
        total += 1;
        if entries.len() < limit {
            entries.push(entry);
        }
    }
    Ok(StatusResult {
        branch,
        entries,
        total,
    })
}

/// Probe this directory itself, without discovering an enclosing repository.
async fn repository_at_target(program: &Path, repo: &Path) -> Result<bool> {
    let program = program.to_path_buf();
    let repo = repo.to_path_buf();
    blocking("rev-parse", false, move || {
        let output = run_git_output(
            &program,
            &repo,
            &["rev-parse", "--resolve-git-dir", "."],
            None,
            None,
            false,
            Some(128),
        )?;
        match output.exit_code {
            0 => Ok(true),
            128 if output.stdout.is_empty()
                && output.stderr.as_slice() == b"fatal: not a gitdir '.'\n" =>
            {
                Ok(false)
            }
            _ => Err(LocalGitError::new(
                "git_failed",
                "git repository probe did not establish an uninitialized target",
            )),
        }
    })
    .await
}

/// `git init` on a directory the operator already allow-listed. The path must exist and must not
/// already hold a repository: re-running `init` over a live repository is refused rather than
/// performed, because git would rewrite configuration in place and the caller would read success.
/// `--template=` is passed so the new repository inherits no sample hooks, which keeps ADR-182
/// Amendment 2 item 4 true of a repository this pack created.
pub(crate) async fn init(program: &Path, repo: &Path, branch: &str) -> Result<String> {
    validate_ref_name("branch", branch)
        .map_err(|error| LocalGitError::new("invalid_params", error.to_string()))?;
    if !repo.is_dir() {
        return Err(LocalGitError::new(
            "repo_not_a_directory",
            "init target must be an existing directory",
        ));
    }
    if repo.join(".git").exists() || repository_at_target(program, repo).await? {
        return Err(LocalGitError::new(
            "already_initialized",
            "init target already holds a repository",
        ));
    }
    // Everything past this point can leave a repository directory behind. `git init` creates
    // `.git` and then writes HEAD, and reading HEAD back is a second process: either can fail
    // after the directory exists. A `not_committed` receipt over a half-made repository is a
    // receipt that is wrong, and this call must not delete a `.git` it may not have created (the
    // check above is a moment earlier, and `git init` is idempotent over an existing repository),
    // so the outcome is reported as unestablished with the path that has to be looked at.
    let created = run_async(
        program,
        repo,
        &["init", "-q", "--template=", "-b", branch],
        None,
    )
    .await;
    let partial = |error: LocalGitError| {
        if !repo.join(".git").exists() {
            return error;
        }
        // The code is the part a caller reads: an ambiguous failure settles as `Internal`, and
        // that path carries no detail, so the condition has to be IN the code. The receipt names
        // the repository in its own `repo` column, which is where the path to inspect comes from.
        LocalGitError {
            code: "init_partial_repository",
            message: format!(
                "git init left a repository directory at {} and its completion could not be \
                 established ({error}); inspect or remove that directory before retrying",
                repo.join(".git").display()
            ),
            ambiguous: true,
        }
    };
    created.map_err(partial)?;
    resolve_head_branch(program, repo).await.map_err(partial)
}

async fn resolve_head_branch(program: &Path, repo: &Path) -> Result<String> {
    let bytes = run_async(program, repo, &["symbolic-ref", "--short", "HEAD"], None).await?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

#[derive(Serialize)]
pub(crate) struct LogEntry {
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: String,
    pub committed_at: String,
    pub subject: String,
}

/// A bounded page of `git log`. Fields are newline-separated inside a NUL-separated record: `%s`
/// is the subject line and can hold no newline, and neither can an author name or an ISO date, so
/// the split is unambiguous for every path and message git can store. `--literal-pathspecs` is a
/// main-command option and so precedes the subcommand; it stops a caller-supplied path from being
/// read as a glob or a magic pathspec.
pub(crate) async fn log(
    program: &Path,
    repo: &Path,
    reference: &str,
    limit: usize,
    path: Option<&str>,
) -> Result<Vec<LogEntry>> {
    let resolved = checked_ref(reference)?;
    let count = limit.to_string();
    let mut argv = vec![
        "--literal-pathspecs",
        "log",
        "-z",
        "--no-color",
        // Beside `log.showSignature=false` in HARDENING. The config key closes the door for any
        // caller of this builder; the flag closes it on the one command that reads that key, so
        // neither a config override nor a future change to the hardening list re-opens it alone.
        "--no-show-signature",
        "--format=%H%n%an%n%ae%n%aI%n%cI%n%s",
        "-n",
        &count,
        "--end-of-options",
        &resolved,
    ];
    if let Some(path) = path {
        argv.push("--");
        argv.push(path);
    }
    let bytes = run_async(program, repo, &argv, None).await?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| LocalGitError::new("git_failed", "git log emitted a non-UTF-8 record"))?;
    let mut entries = Vec::new();
    for record in text.split('\0') {
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(6, '\n');
        let sha = fields.next().unwrap_or_default().to_string();
        validate_oid(&sha, "sha")?;
        entries.push(LogEntry {
            sha,
            author_name: fields.next().unwrap_or_default().to_string(),
            author_email: fields.next().unwrap_or_default().to_string(),
            authored_at: fields.next().unwrap_or_default().to_string(),
            committed_at: fields.next().unwrap_or_default().to_string(),
            subject: fields.next().unwrap_or_default().to_string(),
        });
    }
    Ok(entries)
}

#[cfg(all(test, unix))]
#[path = "local_git_gp_tests.rs"]
mod gp_tests;

#[cfg(all(test, unix))]
#[path = "local_git_init_tests.rs"]
mod init_tests;

#[cfg(test)]
#[path = "local_git_tests.rs"]
mod tests;
