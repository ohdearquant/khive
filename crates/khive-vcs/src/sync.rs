//! NDJSON-to-SQLite sync library boundary.
//!
//! Rebuilds the SQLite database from `.khive/kg/entities.ndjson` and `edges.ndjson`.
//! Builds into a unique sibling file then renames. Also supports remote archive
//! fetch with SHA-256 pin verification via [`run_sync_remote`].

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use khive_runtime::pack::{PackRegistry, VerbRegistryBuilder};
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_runtime::{entity_fts_document, KhiveRuntime, RuntimeConfig};
use khive_storage::types::Edge;
use khive_storage::LinkId;
use khive_types::{EdgeRelation, Pack};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use uuid::Uuid;

use crate::error::VcsError;
use crate::hash::snapshot_id_for_archive;
use crate::types::SnapshotId;

#[cfg(test)]
#[path = "remote_cache_perf_tests.rs"]
mod remote_cache_perf_tests;
#[cfg(test)]
#[path = "remote_cache_recovery_tests.rs"]
mod remote_cache_recovery_tests;

/// Per-record entity shape in NDJSON sources.
#[derive(Debug, Serialize, Deserialize)]
struct NdjsonEntity {
    id: Uuid,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entity_type: Option<String>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    properties: Option<serde_json::Value>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

/// Per-record edge shape in NDJSON sources.
#[derive(Debug, Serialize, Deserialize)]
struct NdjsonEdge {
    edge_id: Uuid,
    source: Uuid,
    target: Uuid,
    relation: String,
    #[serde(default = "default_weight")]
    weight: f64,
    #[serde(default)]
    properties: Option<serde_json::Value>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

fn default_weight() -> f64 {
    1.0
}

/// Parse a present RFC3339 timestamp exactly; only absence uses `fallback`.
fn parse_timestamp(
    s: Option<&str>,
    fallback: chrono::DateTime<Utc>,
) -> Result<chrono::DateTime<Utc>> {
    khive_runtime::portability::parse_archive_timestamp(s, fallback)
}

fn parse_ts_micros(s: Option<&str>, fallback: chrono::DateTime<Utc>) -> Result<i64> {
    Ok(parse_timestamp(s, fallback)?.timestamp_micros())
}

/// Summary of a completed sync run.
#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub entities: usize,
    pub edges: usize,
    pub db_path: String,
}

// ── F201: Remote archive fetch ────────────────────────────────────────────────

/// A validated remote cache name: a single path segment safe to join under
/// `.khive/kg/remotes/` without escaping that directory.
///
/// Construct via [`RemoteName::parse`]. There is no public way to build a
/// `RemoteName` that fails validation, so a `RemoteConfig` can never carry an
/// unsafe name into [`run_sync_remote`] (VCS-AUD-002).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RemoteName(String);

/// Renders exactly like a plain `String`'s `Debug` (quoted), so existing
/// `{:?}`-formatted error messages that embedded `remote.name` keep the same
/// shape after the `String` -> `RemoteName` migration.
impl std::fmt::Debug for RemoteName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl RemoteName {
    /// Validate `raw` as a safe single-path-segment remote name.
    ///
    /// Rejects: empty strings, `.`, `..`, any name containing `/` or `\`, and
    /// any character outside `[A-Za-z0-9._-]`. Because path separators are
    /// rejected outright, an absolute path (Unix `/root`, Windows `C:\root`)
    /// can never pass — `:` is also outside the allowed character set.
    pub fn parse(raw: impl Into<String>) -> Result<Self, VcsError> {
        let raw = raw.into();
        let valid = !raw.is_empty()
            && raw != "."
            && raw != ".."
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            && !raw.contains('/')
            && !raw.contains('\\');
        if !valid {
            return Err(VcsError::InvalidRemoteName(raw));
        }
        Ok(Self(raw))
    }

    /// The validated name as a `&str`, safe to join onto a cache directory path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RemoteName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Configuration for a remote KG archive (maps to one entry in `schema.yaml`
/// `remotes:` list).
#[derive(Debug, Clone)]
pub struct RemoteConfig {
    /// Validated name for this remote (used in error messages and cache
    /// directory paths). Constructed via [`RemoteName::parse`], so a
    /// `RemoteConfig` can never carry a path-traversal or absolute-path name.
    pub name: RemoteName,
    /// Git remote URL (e.g. `https://github.com/org/kg-data.git`).
    pub url: String,
    /// Git ref to check out (branch or tag, e.g. `main`).
    pub git_ref: String,
    /// Namespace to assign to imported records.
    pub namespace: String,
    /// Optional SHA-256 content-hash pin. When present, a mismatch between the
    /// fetched archive hash and this value aborts the sync (fail-closed).
    pub pin: Option<SnapshotId>,
}

/// Summary of a completed remote sync run (F201).
#[derive(Debug, Serialize)]
pub struct RemoteSyncReport {
    pub entities: usize,
    pub edges: usize,
    /// Path to the populated cache directory (`.khive/kg/remotes/<name>/`).
    pub cache_dir: String,
    /// Path to the written `meta.json` file.
    pub meta_path: String,
    /// Canonical SHA-256 content hash of the fetched archive (`sha256:<hex>`).
    pub content_hash: String,
    /// `true` when `repin` was requested — the caller should write
    /// `content_hash` back to `schema.yaml` as the new `pin` value.
    pub repinned: bool,
}

/// Metadata written to `.khive/kg/remotes/<name>/meta.json`.
#[derive(Debug, Serialize)]
struct MetaJson {
    /// ISO-8601 timestamp of when the fetch completed.
    fetched_at: String,
    /// Git ref that was resolved.
    git_ref: String,
    /// Git commit SHA resolved from `git_ref` at fetch time.
    commit_sha: String,
    /// Canonical content hash of the fetched archive.
    content_hash: String,
}

/// Fetch a remote KG archive, verify SHA-256, populate `.khive/kg/remotes/`, write `meta.json`.
/// Fail-closed on hash mismatch; use `repin=true` to update the pin.
pub async fn run_sync_remote(
    repo_root: &Path,
    remote: &RemoteConfig,
    repin: bool,
) -> Result<RemoteSyncReport> {
    // ── 1. Create staging directory ──────────────────────────────────────────
    let state_dir = repo_root.join(".khive/state/remote-staging");
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating staging dir {}", state_dir.display()))?;
    let staging = tempfile::TempDir::new_in(&state_dir).context("creating staging temp dir")?;
    let staging_path = staging.path().to_path_buf();

    // ── 2. Git clone (sparse, depth=1) ───────────────────────────────────────
    let entities_ndjson: Vec<NdjsonEntity>;
    let edges_ndjson: Vec<NdjsonEdge>;
    let commit_sha: String;

    {
        // Clone only the objects needed — no blobs, just tree metadata, then
        // sparse-checkout the two NDJSON files we need.
        let clone_out = Command::new("git")
            .args([
                "clone",
                "--depth=1",
                "--filter=blob:none",
                "--no-checkout",
                "--branch",
                &remote.git_ref,
            ])
            .arg(&remote.url)
            .arg(&staging_path)
            .output()
            .context("running git clone")?;

        if !clone_out.status.success() {
            let stderr = String::from_utf8_lossy(&clone_out.stderr);
            let safe = redact_git_stderr(stderr.trim());
            return Err(anyhow!(
                "git clone failed for remote {:?}: {}",
                remote.name,
                safe
            ));
        }

        // Resolve commit SHA from HEAD.
        let rev_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&staging_path)
            .output()
            .context("running git rev-parse HEAD")?;
        commit_sha = String::from_utf8_lossy(&rev_out.stdout).trim().to_string();

        // Sparse checkout: enable and limit to the two NDJSON files.
        run_git_in(&staging_path, &["sparse-checkout", "init", "--cone"])
            .context("git sparse-checkout init")?;
        run_git_in(
            &staging_path,
            &[
                "sparse-checkout",
                "set",
                ".khive/kg/entities.ndjson",
                ".khive/kg/edges.ndjson",
            ],
        )
        .context("git sparse-checkout set")?;
        run_git_in(&staging_path, &["checkout"]).context("git checkout")?;

        // Parse the staged NDJSON files.
        let entities_path = staging_path.join(".khive/kg/entities.ndjson");
        let edges_path = staging_path.join(".khive/kg/edges.ndjson");

        entities_ndjson = read_remote_entities(&entities_path)
            .with_context(|| format!("reading staged {}", entities_path.display()))?;
        edges_ndjson = read_remote_edges(&edges_path)
            .with_context(|| format!("reading staged {}", edges_path.display()))?;
    }
    // `staging` tempdir is still alive here — we drop it after moving files.

    // ── 3. Build KgArchive and compute canonical hash ─────────────────────────
    // Apply the same deterministic gate used by local DB rebuilds before hash
    // computation or reader-visible cache publication.
    validate_ndjson_records(&entities_ndjson, &edges_ndjson)
        .with_context(|| format!("validating remote {:?} NDJSON", remote.name))?;
    let archive = build_kg_archive(&remote.namespace, &entities_ndjson, &edges_ndjson)
        .with_context(|| format!("validating archive for remote {:?}", remote.name))?;
    let actual_hash = snapshot_id_for_archive(&archive)
        .map_err(|e| anyhow!("hashing archive for remote {:?}: {}", remote.name, e))?;

    // ── 4. Pin verification (fail-closed) ────────────────────────────────────
    if let Some(expected) = &remote.pin {
        if !repin && actual_hash != *expected {
            return Err(anyhow!(VcsError::HashMismatch {
                expected: expected.clone(),
                actual: actual_hash.clone(),
            })
            .context(format!(
                "remote {:?}: hash mismatch — use `--repin` to accept the new content \
                 after independently verifying it (actual hash: {})",
                remote.name,
                actual_hash.as_str()
            )));
        }
    }

    // ── 5. Build meta and atomically publish to cache ─────────────────────────
    let meta = MetaJson {
        fetched_at: Utc::now().to_rfc3339(),
        git_ref: remote.git_ref.clone(),
        commit_sha,
        content_hash: actual_hash.as_str().to_string(),
    };

    let remotes_root = repo_root.join(".khive/kg/remotes");
    let cache_dir = remotes_root.join(remote.name.as_str());
    let published = publish_remote_cache(
        &remotes_root,
        remote.name.as_str(),
        &entities_ndjson,
        &edges_ndjson,
        &meta,
        #[cfg(test)]
        None,
    )
    .with_context(|| format!("publishing cache for remote {:?}", remote.name))?;
    debug_assert_eq!(published, cache_dir);

    // staging tempdir (the git clone) is dropped here, cleaning up the clone.
    drop(staging);

    Ok(RemoteSyncReport {
        entities: entities_ndjson.len(),
        edges: edges_ndjson.len(),
        cache_dir: cache_dir.to_string_lossy().into_owned(),
        meta_path: cache_dir.join("meta.json").to_string_lossy().into_owned(),
        content_hash: actual_hash.as_str().to_string(),
        repinned: repin,
    })
}

/// Injection points for #475 failure-injection tests: simulate a crash at each
/// staging step so tests can assert readers never observe a mixed cache state.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishFailAt {
    AfterGitignoreTempCreate,
    AfterEntities,
    AfterEdges,
    AfterMeta,
    BeforeSwap,
}

/// Publish a complete `{entities.ndjson, edges.ndjson, meta.json}` triple to
/// `<remotes_root>/<name>/` as one complete cache generation.
///
/// Builds a complete staging directory (a sibling of the cache directory,
/// under `remotes_root`) containing all three files, then switches visibility
/// with [`atomic_replace_dir`]. A crash between its two renames can leave the
/// target briefly absent and the old generation in a `.replaced~*` sibling;
/// the next publish recovers that sibling before replacing it. A reader never
/// observes a mix of old and new files within the target directory.
const REMOTE_BACKUP_MARKER: &str = ".replaced~";
const REMOTE_BACKUP_OWNER_FILE: &str = ".khive-backup-owner";
const REMOTE_BACKUP_OWNER_HEADER: &str = "khive-vcs remote cache backup v1\n";
const REMOTES_GITIGNORE: &[u8] = b"*\n";
const REMOTES_GITIGNORE_PENDING_PREFIX: &str = ".khive-gitignore-pending-";

fn read_bounded_marker(path: &Path, expected_len: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(expected_len as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn tool_owned_remote_cache_gitignore_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file()
                || read_bounded_marker(path, REMOTES_GITIGNORE.len())?.as_slice()
                    != REMOTES_GITIGNORE
            {
                bail!(
                    "remote cache ignore file {} is not the tool-owned `*` rule",
                    path.display()
                );
            }
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

/// A completed marker ignores these names. Sweep only after validating or
/// publishing that marker: a concurrent creator can then safely revalidate it
/// if its own pending file was removed before `persist_noclobber`.
fn sweep_remote_cache_gitignore_pending(remotes_root: &Path) {
    let Ok(entries) = fs::read_dir(remotes_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .to_str()
            .is_some_and(|name| name.starts_with(REMOTES_GITIGNORE_PENDING_PREFIX))
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        // Only remove an empty or partially written tool marker. The bounded
        // read avoids consuming a contributor file with a similar name.
        let Ok(file) = File::open(entry.path()) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take(REMOTES_GITIGNORE.len() as u64 + 1)
            .read_to_end(&mut bytes)
            .is_ok()
            && REMOTES_GITIGNORE.starts_with(&bytes)
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn ensure_remote_cache_gitignore(
    remotes_root: &Path,
    #[cfg(test)] interrupt_after_temp_create: bool,
) -> Result<()> {
    let path = remotes_root.join(".gitignore");
    if tool_owned_remote_cache_gitignore_exists(&path)? {
        sweep_remote_cache_gitignore_pending(remotes_root);
        return Ok(());
    }

    let mut pending = tempfile::Builder::new()
        .prefix(REMOTES_GITIGNORE_PENDING_PREFIX)
        .tempfile_in(remotes_root)
        .with_context(|| format!("creating pending ignore file in {}", remotes_root.display()))?;
    #[cfg(test)]
    if interrupt_after_temp_create {
        // Keep the file to model a process death, which skips TempFile's Drop.
        let (_file, _path) = pending.keep().context("keeping interrupted ignore file")?;
        bail!("injected failure after cache ignore temp creation");
    }
    pending
        .write_all(REMOTES_GITIGNORE)
        .with_context(|| format!("writing pending ignore file in {}", remotes_root.display()))?;
    pending
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing pending ignore file in {}", remotes_root.display()))?;
    match pending.persist_noclobber(&path) {
        Ok(_file) => {
            fsync_dir_best_effort(remotes_root);
            sweep_remote_cache_gitignore_pending(remotes_root);
            Ok(())
        }
        Err(error) => {
            let tempfile::PersistError {
                error: persist_error,
                file,
            } = error;
            drop(file);
            // Another creator may have won. Even if its successful sweep
            // removed our pending file, accept only its complete marker.
            if tool_owned_remote_cache_gitignore_exists(&path)? {
                sweep_remote_cache_gitignore_pending(remotes_root);
                Ok(())
            } else {
                Err(persist_error).with_context(|| format!("publishing {}", path.display()))
            }
        }
    }
}

fn publish_remote_cache(
    remotes_root: &Path,
    name: &str,
    entities: &[NdjsonEntity],
    edges: &[NdjsonEdge],
    meta: &MetaJson,
    #[cfg(test)] fail_at: Option<PublishFailAt>,
) -> Result<PathBuf> {
    std::fs::create_dir_all(remotes_root)
        .with_context(|| format!("creating {}", remotes_root.display()))?;
    #[cfg(test)]
    ensure_remote_cache_gitignore(
        remotes_root,
        fail_at == Some(PublishFailAt::AfterGitignoreTempCreate),
    )?;
    #[cfg(not(test))]
    ensure_remote_cache_gitignore(remotes_root)?;
    let cache_dir = remotes_root.join(name);
    // Recovery precedes staging I/O too: an error while constructing the next
    // generation must not leave a prior crash's backup as the only copy.
    recover_stale_backups(&cache_dir, |from: &Path, to: &Path| {
        std::fs::rename(from, to)
    })?;
    let staging = tempfile::TempDir::new_in(remotes_root).context("creating staging dir")?;

    write_sorted_entities(&staging.path().join("entities.ndjson"), entities)
        .context("writing staged entities.ndjson")?;
    #[cfg(test)]
    if fail_at == Some(PublishFailAt::AfterEntities) {
        anyhow::bail!("injected failure after staged entities write");
    }

    write_sorted_edges(&staging.path().join("edges.ndjson"), edges)
        .context("writing staged edges.ndjson")?;
    #[cfg(test)]
    if fail_at == Some(PublishFailAt::AfterEdges) {
        anyhow::bail!("injected failure after staged edges write");
    }

    let meta_json = serde_json::to_string_pretty(meta).context("serializing meta.json")?;
    std::fs::write(staging.path().join("meta.json"), meta_json.as_bytes())
        .context("writing staged meta.json")?;
    fsync_dir_best_effort(staging.path());
    #[cfg(test)]
    if fail_at == Some(PublishFailAt::AfterMeta) {
        anyhow::bail!("injected failure after staged meta write, before swap");
    }

    #[cfg(test)]
    if fail_at == Some(PublishFailAt::BeforeSwap) {
        anyhow::bail!("injected failure before swap");
    }
    atomic_replace_dir(staging.path(), &cache_dir)?;
    fsync_dir_best_effort(remotes_root);

    Ok(cache_dir)
}

/// Atomically replace `target_dir` with `new_dir` (a complete, ready-to-serve
/// directory). If `target_dir` does not exist yet, `new_dir` is simply renamed
/// into place. If `target_dir` already exists, the existing directory is first
/// renamed to a sibling backup path, then `new_dir` is renamed into
/// `target_dir`'s place; the backup is removed only after the swap succeeds.
/// If the second rename fails, restoration is attempted and any restoration
/// failure names the retained backup. Both renames are filesystem rename(2) calls, each of
/// which is atomic — at every instant `target_dir` resolves to either the
/// complete old directory, is briefly absent, or resolves to the complete new
/// directory; it never contains a mix of old and new files.
fn atomic_replace_dir(new_dir: &Path, target_dir: &Path) -> Result<()> {
    atomic_replace_dir_with(new_dir, target_dir, |from: &Path, to: &Path| {
        std::fs::rename(from, to)
    })
}

fn backup_owner_contents(backup: &Path) -> Result<String> {
    let name = backup
        .file_name()
        .and_then(|part| part.to_str())
        .context("cache backup has no UTF-8 name")?;
    Ok(format!("{REMOTE_BACKUP_OWNER_HEADER}{name}\n"))
}

fn is_owned_backup(backup: &Path) -> Result<bool> {
    let marker = backup.join(REMOTE_BACKUP_OWNER_FILE);
    let metadata = match std::fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };
    if !metadata.file_type().is_file() {
        return Ok(false);
    }
    let Ok(expected) = backup_owner_contents(backup) else {
        return Ok(false);
    };
    let Ok(contents) = read_bounded_marker(&marker, expected.len()) else {
        return Ok(false);
    };
    Ok(contents == expected.as_bytes())
}

fn mark_cache_for_backup(target_dir: &Path, backup: &Path) -> Result<()> {
    let marker = target_dir.join(REMOTE_BACKUP_OWNER_FILE);
    match std::fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                bail!("cache backup marker {} is not a file", marker.display());
            }
            let name = target_dir
                .file_name()
                .and_then(|part| part.to_str())
                .context("cache target has no UTF-8 name")?;
            let prefix = format!("{REMOTE_BACKUP_OWNER_HEADER}{name}{REMOTE_BACKUP_MARKER}");
            // Every tool-generated suffix is a u32 process id (at most ten
            // digits). A contributor-owned marker need not be read in full.
            let previous_bytes = read_bounded_marker(&marker, prefix.len() + 10 + 1)
                .with_context(|| format!("reading {}", marker.display()))?;
            let previous = std::str::from_utf8(&previous_bytes)
                .with_context(|| format!("decoding {}", marker.display()))?;
            let suffix = previous
                .strip_prefix(&prefix)
                .and_then(|value| value.strip_suffix('\n'));
            if !suffix.is_some_and(|value| {
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                bail!("cache backup marker {} is unrecognized", marker.display());
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", marker.display())),
    }
    std::fs::write(&marker, backup_owner_contents(backup)?)
        .with_context(|| format!("writing {}", marker.display()))?;
    std::fs::File::open(&marker)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("syncing {}", marker.display()))?;
    fsync_dir_best_effort(target_dir);
    Ok(())
}

fn recover_stale_backups(
    target_dir: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let parent = target_dir.parent().context("cache target has no parent")?;
    let name = target_dir
        .file_name()
        .and_then(|part| part.to_str())
        .context("cache target has no UTF-8 name")?;
    // '~' cannot occur in a RemoteName; older ".replaced-" siblings may be
    // live remotes and must be left for manual recovery.
    let backup_prefix = format!("{name}{REMOTE_BACKUP_MARKER}");
    let mut stale = Vec::new();
    for entry in
        std::fs::read_dir(parent).with_context(|| format!("reading {}", parent.display()))?
    {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(suffix) = file_name
            .to_str()
            .and_then(|part| part.strip_prefix(&backup_prefix))
        else {
            continue;
        };
        if !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            // The name alone is not proof: repository content may include a
            // backup-shaped directory. Never follow a symlink while checking.
            if !entry.file_type()?.is_dir() || !is_owned_backup(&entry.path())? {
                eprintln!(
                    "warning: leaving unverified cache backup {} untouched",
                    entry.path().display()
                );
                continue;
            }
            stale.push(entry.path());
        }
    }
    if !target_dir.exists() && !stale.is_empty() {
        if stale.len() != 1 {
            bail!(
                "cache {} is missing with multiple backups; manual recovery required",
                target_dir.display()
            );
        }
        rename(&stale[0], target_dir).with_context(|| {
            format!(
                "restoring cache backup {} -> {}",
                stale[0].display(),
                target_dir.display()
            )
        })?;
        std::fs::remove_file(target_dir.join(REMOTE_BACKUP_OWNER_FILE)).with_context(|| {
            format!("clearing restored cache marker in {}", target_dir.display())
        })?;
    } else {
        // A present target is the published generation; these are leftovers of
        // earlier completed or interrupted swaps.
        for path in stale {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing stale cache backup {}", path.display()))?;
        }
    }
    Ok(())
}

fn atomic_replace_dir_with(
    new_dir: &Path,
    target_dir: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    recover_stale_backups(target_dir, &mut rename)?;
    if !target_dir.exists() {
        rename(new_dir, target_dir).with_context(|| {
            format!("renaming {} -> {}", new_dir.display(), target_dir.display())
        })?;
        return Ok(());
    }

    let name = target_dir
        .file_name()
        .and_then(|part| part.to_str())
        .context("cache target has no UTF-8 name")?;
    let backup = target_dir.with_file_name(format!(
        "{name}{REMOTE_BACKUP_MARKER}{}",
        std::process::id()
    ));

    match std::fs::symlink_metadata(&backup) {
        Ok(_) => bail!(
            "cache backup path {} already exists; manual recovery required",
            backup.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("checking {}", backup.display())),
    }
    mark_cache_for_backup(target_dir, &backup)?;

    if let Err(error) = rename(target_dir, &backup) {
        let _ = std::fs::remove_file(target_dir.join(REMOTE_BACKUP_OWNER_FILE));
        return Err(error).with_context(|| {
            format!(
                "backing up existing cache {} -> {}",
                target_dir.display(),
                backup.display()
            )
        });
    }

    match rename(new_dir, target_dir) {
        Ok(()) => {
            // The new generation is already published. A cleanup failure is
            // recoverable on the next publish and must not report this commit
            // as failed.
            match is_owned_backup(&backup) {
                Ok(true) => {
                    let _ = std::fs::remove_dir_all(&backup);
                }
                Ok(false) => eprintln!(
                    "warning: leaving unverified cache backup {} untouched",
                    backup.display()
                ),
                Err(error) => eprintln!(
                    "warning: leaving cache backup {} untouched: {error:#}",
                    backup.display()
                ),
            }
            Ok(())
        }
        Err(e) => {
            match rename(&backup, target_dir) {
                Ok(()) => {
                    let _ = std::fs::remove_file(target_dir.join(REMOTE_BACKUP_OWNER_FILE));
                    Err(e).with_context(|| {
                        format!("renaming {} -> {} (old cache restored)", new_dir.display(), target_dir.display())
                    })
                }
                Err(restore) => Err(anyhow!(
                    "renaming {} -> {} failed: {e}; restoring old cache failed: {restore}; old cache remains at {}",
                    new_dir.display(), target_dir.display(), backup.display()
                )),
            }
        }
    }
}

/// Best-effort `fsync` of a directory's entries, where the platform supports
/// opening a directory as a file handle (Unix). Errors are ignored: this is a
/// durability improvement, not a correctness requirement for the atomicity
/// guarantee above (which relies on rename(2) semantics, not fsync).
fn fsync_dir_best_effort(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

/// Redact URLs and embedded credentials (`user:token@host` HTTPS forms, and
/// scp-style `user@host:path` remotes) from git stderr before it reaches a
/// caller-visible error — ADR-037 §157 prohibits leaking remote URLs. See
/// `docs/api/sync.md` for the exact matched forms.
pub(crate) fn redact_git_stderr(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"://") {
            // Walk back over the scheme characters already written.
            let scheme_start = {
                let mut s = i;
                while s > 0 && {
                    let b = bytes[s - 1];
                    b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'
                } {
                    s -= 1;
                }
                s
            };
            let already_appended = i - scheme_start;
            out.truncate(out.len() - already_appended);
            // Advance past "://" and consume until the next whitespace or EOL.
            let rest_start = i + 3;
            let url_end = bytes[rest_start..]
                .iter()
                .position(|&b| b.is_ascii_whitespace())
                .map(|p| rest_start + p)
                .unwrap_or(bytes.len());
            out.push_str("<url-redacted>");
            i = url_end;
        } else if is_scp_remote_start(bytes, i) {
            // scp-style remote: `word@host:path`.  Walk back to the start of the
            // `word` part (already written into `out`), then consume forward to
            // the end of the token (next whitespace or end of input).
            let token_start = scan_back_word(bytes, i);
            let already_appended = i - token_start;
            out.truncate(out.len() - already_appended);
            // Consume `@host:path` (the token continues until whitespace/EOL).
            let token_end = bytes[i..]
                .iter()
                .position(|&b| b.is_ascii_whitespace())
                .map(|p| i + p)
                .unwrap_or(bytes.len());
            out.push_str("<url-redacted>");
            i = token_end;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// `true` when position `i` is the `@` of an scp-style remote (`word@host:path`,
/// distinguished from `word@host: message text` by requiring a non-whitespace,
/// non-`:` character right after the colon). See `docs/api/sync.md`.
fn is_scp_remote_start(bytes: &[u8], i: usize) -> bool {
    if bytes[i] != b'@' {
        return false;
    }
    // There must be at least one non-whitespace, non-@ character before `@`.
    if i == 0 || bytes[i - 1].is_ascii_whitespace() {
        return false;
    }
    // After `@` there must be content and eventually a `:non-space` sequence.
    let after_at = &bytes[i + 1..];
    // Find `:` in the host portion (before any whitespace).
    let colon_pos = after_at
        .iter()
        .position(|&b| b == b':' || b.is_ascii_whitespace());
    match colon_pos {
        Some(p) if after_at[p] == b':' => {
            // Colon found; make sure the character after it is not whitespace
            // and not another colon (IPv6 / port disambiguation).
            let next = p + 1;
            if next >= after_at.len() {
                return false;
            }
            let ch = after_at[next];
            !ch.is_ascii_whitespace() && ch != b':'
        }
        _ => false,
    }
}

/// Walk backwards from `i` to find the start of the current word (sequence of
/// non-whitespace, non-quote characters).
fn scan_back_word(bytes: &[u8], i: usize) -> usize {
    let mut s = i;
    while s > 0 {
        let b = bytes[s - 1];
        if b.is_ascii_whitespace() || b == b'\'' || b == b'"' {
            break;
        }
        s -= 1;
    }
    s
}

/// Run a git command inside `dir`, returning an error if it fails.
fn run_git_in(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let safe = redact_git_stderr(stderr.trim());
        return Err(anyhow!("git {} failed: {}", args.join(" "), safe));
    }
    Ok(())
}

/// Convert the NDJSON record slices into a [`KgArchive`] for hashing.
///
/// Returns an error if any edge carries an unrecognised relation string, so
/// that invalid edges are rejected *before* the hash is computed and before
/// any cache or database write occurs (fail-closed).
fn build_kg_archive(
    namespace: &str,
    entities: &[NdjsonEntity],
    edges: &[NdjsonEdge],
) -> Result<KgArchive> {
    let now = Utc::now();
    let exported_entities: Vec<ExportedEntity> = entities
        .iter()
        .map(|e| {
            Ok(ExportedEntity {
                id: e.id,
                kind: e.kind.clone(),
                entity_type: e.entity_type.clone(),
                name: e.name.clone(),
                description: e.description.clone(),
                properties: e.properties.clone(),
                tags: e.tags.clone(),
                created_at: parse_timestamp(e.created_at.as_deref(), now)
                    .with_context(|| format!("entity {} invalid created_at", e.id))?,
                updated_at: parse_timestamp(e.updated_at.as_deref(), now)
                    .with_context(|| format!("entity {} invalid updated_at", e.id))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut exported_edges: Vec<ExportedEdge> = Vec::with_capacity(edges.len());
    for e in edges {
        let relation: EdgeRelation = e
            .relation
            .parse()
            .map_err(|err| anyhow!("invalid edge relation {:?}: {}", e.relation, err))?;
        exported_edges.push(ExportedEdge {
            edge_id: e.edge_id,
            source: e.source,
            target: e.target,
            relation,
            weight: e.weight,
            properties: e.properties.clone(),
            created_at: parse_timestamp(e.created_at.as_deref(), now)
                .with_context(|| format!("edge {} invalid created_at", e.edge_id))?,
            updated_at: parse_timestamp(e.updated_at.as_deref(), now)
                .with_context(|| format!("edge {} invalid updated_at", e.edge_id))?,
        });
    }

    Ok(KgArchive {
        format: "khive-kg".into(),
        version: "0.1".into(),
        namespace: namespace.to_string(),
        exported_at: now,
        entities: exported_entities,
        edges: exported_edges,
    })
}

/// Write entities to a file as sorted NDJSON (one JSON object per line).
///
/// Entities are sorted by UUID string (case-insensitive ascending) to match
/// the canonical sort order used by `snapshot_id_for_archive`.
fn write_sorted_entities(path: &Path, records: &[NdjsonEntity]) -> Result<()> {
    let mut sorted: Vec<&NdjsonEntity> = records.iter().collect();
    sorted.sort_by_key(|entity| entity.id);
    let file = File::create(path).context("creating entities file")?;
    let mut writer = BufWriter::new(file);
    for (index, record) in sorted.into_iter().enumerate() {
        if index != 0 {
            writer
                .write_all(b"\n")
                .context("writing entity separator")?;
        }
        serde_json::to_writer(&mut writer, record).context("serializing entity")?;
    }
    writer.flush().context("writing entities file")?;
    Ok(())
}

/// Write edges to a file as sorted NDJSON (one JSON object per line).
///
/// Edges are sorted by (source, target, relation) to match the canonical sort
/// order used by `snapshot_id_for_archive`.
fn write_sorted_edges(path: &Path, records: &[NdjsonEdge]) -> Result<()> {
    let mut sorted: Vec<&NdjsonEdge> = records.iter().collect();
    sorted.sort_by(|a, b| {
        (a.source, a.target, a.relation.as_str()).cmp(&(b.source, b.target, b.relation.as_str()))
    });
    let file = File::create(path).context("creating edges file")?;
    let mut writer = BufWriter::new(file);
    for (index, record) in sorted.into_iter().enumerate() {
        if index != 0 {
            writer.write_all(b"\n").context("writing edge separator")?;
        }
        serde_json::to_writer(&mut writer, record).context("serializing edge")?;
    }
    writer.flush().context("writing edges file")?;
    Ok(())
}

/// Full ADR-020 structural validation of parsed NDJSON records (#476).
///
/// Checks registered, canonical entity kinds and reserved property validity,
/// entity/edge timestamp validity, entity/edge
/// sort order (matching `write_sorted_entities`/`write_sorted_edges`), duplicate
/// entity ids, duplicate edge ids, duplicate canonical semantic edge triples
/// (source, target, relation), dangling edge endpoints, and edge relation/weight
/// validity. Called before any temp DB is created so a violation here leaves
/// the existing target DB completely untouched.
fn validate_ndjson_records(entities: &[NdjsonEntity], edges: &[NdjsonEdge]) -> Result<()> {
    let valid_entity_kinds = registered_entity_kinds()?;
    let mut entity_ids: HashSet<Uuid> = HashSet::with_capacity(entities.len());
    let mut prev_entity_key: Option<String> = None;
    for (i, e) in entities.iter().enumerate() {
        if !valid_entity_kinds.contains(e.kind.as_str()) {
            let kind = khive_types::EntityKind::from_str(&e.kind)
                .map_err(|_| anyhow!("entity {i} ({}): unknown kind {:?}", e.id, e.kind))?;
            // The parser accepts aliases for interactive callers, but NDJSON
            // is a canonical archive. Pack-registered kinds are already exact.
            if e.kind != kind.name() {
                bail!(
                    "entity {i} ({}): non-canonical kind {:?}; use {:?}",
                    e.id,
                    e.kind,
                    kind.name()
                );
            }
        }
        // ADR-115 Amendment 1 §3 reserves this runtime-owned key on every
        // properties-bearing write path, including pack-defined entity kinds.
        khive_runtime::secret_gate::reject_reserved_secret_gate_property(e.properties.as_ref())
            .map_err(|error| anyhow!("entity {i} ({}) properties rejected: {error}", e.id))?;
        if e.name.trim().is_empty() {
            bail!("entity {i} ({}): name must be a non-blank name", e.id);
        }

        if !entity_ids.insert(e.id) {
            bail!("entity {i}: duplicate entity id {}", e.id);
        }

        for (field, value) in [("created_at", &e.created_at), ("updated_at", &e.updated_at)] {
            if let Some(s) = value {
                chrono::DateTime::parse_from_rfc3339(s)
                    .with_context(|| format!("entity {i} ({}): invalid {field} {s:?}", e.id))?;
            }
        }

        let key = e.id.to_string().to_ascii_lowercase();
        if let Some(prev) = &prev_entity_key {
            if key < *prev {
                bail!(
                    "entities.ndjson is not sorted: entity {i} ({}) is out of order",
                    e.id
                );
            }
        }
        prev_entity_key = Some(key);
    }

    let mut edge_ids: HashSet<Uuid> = HashSet::with_capacity(edges.len());
    let mut triples: HashSet<(Uuid, Uuid, EdgeRelation)> = HashSet::with_capacity(edges.len());
    let mut prev_edge_key: Option<(String, String, String)> = None;
    for (i, r) in edges.iter().enumerate() {
        let relation = r.relation.parse::<EdgeRelation>().with_context(|| {
            format!(
                "invalid edge relation {:?} at record {} — sync aborted before any DB write",
                r.relation,
                i + 1
            )
        })?;

        if !r.weight.is_finite() || !(0.0..=1.0).contains(&r.weight) {
            bail!(
                "edge {i} ({}): weight {} out of range; must be finite and in [0.0, 1.0]",
                r.edge_id,
                r.weight
            );
        }

        if !edge_ids.insert(r.edge_id) {
            bail!("edge {i}: duplicate edge id {}", r.edge_id);
        }

        // ADR-115 Amendment 1 §3: edge metadata is in the unchanged
        // blocking-scanner class. This validator guards BOTH consumers —
        // the local DB rebuild and remote cache publication
        // (`publish_remote_cache` persists these records reader-visible),
        // so the reservation and the credential scanner both run here,
        // before any write or publish.
        khive_runtime::secret_gate::reject_reserved_secret_gate_property(r.properties.as_ref())
            .map_err(|e| anyhow!("edge {i} ({}) properties rejected: {e}", r.edge_id))?;
        if let Some(props) = r.properties.as_ref() {
            khive_runtime::secret_gate::check_json(props)
                .map_err(|e| anyhow!("edge {i} ({}) properties rejected: {e}", r.edge_id))?;
        }

        let (source, target) = relation.canonical_endpoints(r.source, r.target);
        if !triples.insert((source, target, relation)) {
            bail!(
                "edge {i} ({}): duplicate edge triple (source={}, target={}, relation={:?})",
                r.edge_id,
                r.source,
                r.target,
                relation
            );
        }

        if !entity_ids.contains(&r.source) {
            bail!(
                "edge {i} ({}): dangling source {} — no matching entity",
                r.edge_id,
                r.source
            );
        }
        if !entity_ids.contains(&r.target) {
            bail!(
                "edge {i} ({}): dangling target {} — no matching entity",
                r.edge_id,
                r.target
            );
        }

        for (field, value) in [("created_at", &r.created_at), ("updated_at", &r.updated_at)] {
            if let Some(s) = value {
                chrono::DateTime::parse_from_rfc3339(s)
                    .with_context(|| format!("edge {i} ({}): invalid {field} {s:?}", r.edge_id))?;
            }
        }

        let key = (
            r.source.to_string(),
            r.target.to_string(),
            r.relation.clone(),
        );
        if let Some(prev) = &prev_edge_key {
            if key < *prev {
                bail!(
                    "edges.ndjson is not sorted: edge {i} ({}) is out of order",
                    r.edge_id
                );
            }
        }
        prev_edge_key = Some(key);
    }

    Ok(())
}

/// Use the same merged pack vocabulary as the CLI's KG validator. The KG pack
/// is also named directly because `khive-vcs` is usable as a library without
/// the binary's inventory-linked packs.
fn registered_entity_kinds() -> Result<HashSet<&'static str>> {
    let runtime = KhiveRuntime::memory().context("building sync kind-validation runtime")?;
    let mut builder = VerbRegistryBuilder::new();
    let names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    PackRegistry::register_packs(&names, runtime, &mut builder)
        .map_err(|e| anyhow!("building sync pack vocabulary: {e}"))?;
    let registry = builder
        .build_metadata()
        .context("building sync pack metadata")?;
    let mut kinds: HashSet<&'static str> = registry.all_entity_kinds().into_iter().collect();
    kinds.extend(
        <khive_pack_kg::KgPack as Pack>::ENTITY_KINDS
            .iter()
            .copied(),
    );
    Ok(kinds)
}

/// Rebuild `db_path` from `.khive/kg/{entities,edges}.ndjson` under `repo_root`.
///
/// The target must be closed by all SQLite clients. Sync serializes other sync
/// calls with a sibling lock file and refuses any existing target `-wal` or
/// `-shm` sidecar rather than pairing old WAL frames with a new main file.
/// It builds in a unique sibling file and renames only after checkpointing;
/// errors before that rename leave the previous database intact. A later
/// verification error means the replacement may already be visible.
///
/// `namespace` is applied to all imported records.
///
/// Returns a [`SyncReport`] on success, or an error if NDJSON parsing or SQLite
/// upserts fail.
pub async fn run_sync(repo_root: &Path, db_path: &Path, namespace: &str) -> Result<SyncReport> {
    let entities_path = repo_root.join(".khive/kg/entities.ndjson");
    let edges_path = repo_root.join(".khive/kg/edges.ndjson");

    let entity_records = read_entities(&entities_path)
        .with_context(|| format!("reading {}", entities_path.display()))?;
    let edge_records =
        read_edges(&edges_path).with_context(|| format!("reading {}", edges_path.display()))?;

    // ── Validate-first gate (#476) ────────────────────────────────────────────
    // Run the full ADR-020 structural validation before creating the temp DB,
    // so any violation leaves the existing DB completely untouched.
    validate_ndjson_records(&entity_records, &edge_records).context(
        "validating ADR-020 KG NDJSON before DB rebuild — sync aborted before any DB write",
    )?;

    let parent = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let _target_lock = lock_sync_target(db_path)?;
    refuse_target_sidecars(db_path)?;
    let tmp_db = SyncTempDb::new(parent)?;
    let tmp_path = tmp_db.path().to_path_buf();

    // Build the runtime against the tmp file. Vector embedding is disabled
    // because sync runs without an embedding model loaded — vectors are
    // computed lazily on access via the MCP server if needed.
    let ns = khive_types::Namespace::parse(namespace)
        .map_err(|e| anyhow!("invalid namespace {namespace:?}: {e}"))?;
    let config = RuntimeConfig {
        db_path: Some(tmp_path.clone()),
        default_namespace: ns,
        embedding_model: None,
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config)
        .with_context(|| format!("building runtime for {}", tmp_path.display()))?;

    let entity_count = upsert_entities(&runtime, namespace, entity_records).await?;
    let edge_count = upsert_edges(&runtime, namespace, edge_records).await?;

    // Checkpoint the WAL so all committed writes land in the main DB file.
    // Without this, `rename(tmp, target)` moves only the main file and leaves
    // the -wal alongside it; opening `target` later would see only the data
    // through the last auto-checkpoint (every 4000 pages). For small graphs no
    // auto-checkpoint fires, so the data would silently disappear.
    checkpoint_wal(&runtime)
        .await
        .context("checkpoint WAL before rename")?;

    // Drop the runtime so SQLite releases its file handles before rename.
    drop(runtime);

    let temp_wal = with_extension_suffix(&tmp_path, "-wal");
    match fs::metadata(&temp_wal) {
        Ok(metadata) if metadata.len() > 0 => bail!(
            "sync temp database still has uncheckpointed WAL frames in {}",
            temp_wal.display()
        ),
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("checking {}", temp_wal.display())),
    }
    refuse_target_sidecars(db_path)?;
    fs::rename(&tmp_path, db_path)
        .with_context(|| format!("renaming {} -> {}", tmp_path.display(), db_path.display()))?;
    verify_replaced_db(db_path, entity_count, edge_count)?;

    Ok(SyncReport {
        entities: entity_count,
        edges: edge_count,
        db_path: db_path.to_string_lossy().into_owned(),
    })
}

/// The lock file is deliberately retained after release: unlinking it would
/// let a waiter acquire a different inode while the first sync still runs.
fn lock_sync_target(db_path: &Path) -> Result<File> {
    let lock_path = with_extension_suffix(db_path, ".sync.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening sync lock {}", lock_path.display()))?;
    fs4::FileExt::try_lock(&lock)
        .with_context(|| format!("sync already owns target {}", db_path.display()))?;
    Ok(lock)
}

fn refuse_target_sidecars(db_path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm"] {
        let sidecar = with_extension_suffix(db_path, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => bail!(
                "sync target {} has SQLite sidecar {}; close all clients and recover/checkpoint the target before retrying",
                db_path.display(),
                sidecar.display()
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("checking {}", sidecar.display())),
        }
    }
    Ok(())
}

/// A unique same-directory SQLite build. Its guard removes the main file and
/// both SQLite sidecars on every failure path, including an upsert error.
struct SyncTempDb(tempfile::TempPath);

impl SyncTempDb {
    fn new(parent: &Path) -> Result<Self> {
        let file = tempfile::Builder::new()
            .prefix(".khive-sync-")
            .tempfile_in(parent)
            .with_context(|| format!("creating sync temp database in {}", parent.display()))?;
        Ok(Self(file.into_temp_path()))
    }

    fn path(&self) -> &Path {
        self.0.as_ref()
    }
}

impl Drop for SyncTempDb {
    fn drop(&mut self) {
        for suffix in ["-wal", "-shm"] {
            let _ = fs::remove_file(with_extension_suffix(self.path(), suffix));
        }
        // TempPath's own Drop removes the main file if it was not renamed.
    }
}

fn verify_replaced_db(
    db_path: &Path,
    expected_entities: usize,
    expected_edges: usize,
) -> Result<()> {
    let db = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("reopening replaced database {}", db_path.display()))?;
    for (table, expected) in [
        ("entities", expected_entities),
        ("graph_edges", expected_edges),
    ] {
        let actual: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .with_context(|| format!("verifying {table} count in {}", db_path.display()))?;
        if actual != i64::try_from(expected).context("sync count exceeds i64")? {
            bail!(
                "replaced database {} has {actual} {table}, expected {expected}",
                db_path.display()
            );
        }
    }
    Ok(())
}

fn with_extension_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn read_entities(path: &Path) -> Result<Vec<NdjsonEntity>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    read_ndjson(&text, "entity")
}

fn read_ndjson<T: DeserializeOwned>(text: &str, label: &str) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let e: T = serde_json::from_str(trimmed)
            .with_context(|| format!("parsing {label} at line {}", i + 1))?;
        out.push(e);
    }
    Ok(out)
}

fn read_edges(path: &Path) -> Result<Vec<NdjsonEdge>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    read_ndjson(&text, "edge")
}

/// A checked-out remote member is untrusted. Keep the read bound to an opened
/// regular file, without traversing links in the clone-controlled path.
fn read_remote_regular_file(path: &Path) -> Result<Option<String>> {
    let Some(mut file) = open_remote_member(path)? else {
        return Ok(None);
    };
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("reading remote NDJSON member {}", path.display()))?;
    Ok(Some(text))
}

#[cfg(unix)]
fn open_remote_member(path: &Path) -> Result<Option<std::fs::File>> {
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;

    fn open_component(
        parent_fd: RawFd,
        name: &CStr,
        path: &Path,
        directory: bool,
    ) -> Result<Option<File>> {
        // Each clone-controlled component is opened relative to the already
        // opened parent, so a link swap cannot redirect a later component.
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if directory { libc::O_DIRECTORY } else { 0 };
        let fd = unsafe { libc::openat(parent_fd, name.as_ptr(), flags) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            if error.raw_os_error() == Some(libc::ELOOP)
                || error.raw_os_error() == Some(libc::ENOTDIR)
            {
                bail!(
                    "remote NDJSON {} {} is not a regular {}",
                    if directory { "directory" } else { "member" },
                    path.display(),
                    if directory { "directory" } else { "file" }
                );
            }
            return Err(error).with_context(|| format!("opening remote path {}", path.display()));
        }
        // SAFETY: openat returned a newly owned descriptor, transferred once to File.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file
            .metadata()
            .with_context(|| format!("stat opened remote path {}", path.display()))?;
        if directory && !metadata.is_dir() {
            bail!(
                "remote NDJSON directory {} is not a regular directory",
                path.display()
            );
        }
        if !directory && !metadata.is_file() {
            bail!(
                "remote NDJSON member {} is not a regular file",
                path.display()
            );
        }
        Ok(Some(file))
    }

    let kg = path
        .parent()
        .context("remote NDJSON member has no parent")?;
    let khive = kg.parent().context("remote NDJSON kg has no parent")?;
    let staging = khive
        .parent()
        .context("remote NDJSON .khive has no parent")?;
    if kg.file_name() != Some(std::ffi::OsStr::new("kg"))
        || khive.file_name() != Some(std::ffi::OsStr::new(".khive"))
    {
        bail!(
            "remote NDJSON member {} is outside .khive/kg",
            path.display()
        );
    }
    let staging_name = CString::new(staging.as_os_str().as_bytes())?;
    let khive_name = CString::new(".khive")?;
    let kg_name = CString::new("kg")?;
    let member_name = CString::new(
        path.file_name()
            .context("remote NDJSON member has no name")?
            .as_bytes(),
    )?;

    // The staging directory is a private TempDir; all clone-controlled path
    // components below it are opened by dirfd with O_NOFOLLOW.
    let Some(staging_dir) = open_component(libc::AT_FDCWD, &staging_name, staging, true)? else {
        return Ok(None);
    };
    let Some(khive_dir) = open_component(staging_dir.as_raw_fd(), &khive_name, khive, true)? else {
        return Ok(None);
    };
    let Some(kg_dir) = open_component(khive_dir.as_raw_fd(), &kg_name, kg, true)? else {
        return Ok(None);
    };
    open_component(kg_dir.as_raw_fd(), &member_name, path, false)
}

#[cfg(not(unix))]
fn open_remote_member(path: &Path) -> Result<Option<std::fs::File>> {
    // The portable fallback checks every clone-controlled component before
    // opening the member. Unix uses dirfds above to bind those checks to opens.
    let kg = path
        .parent()
        .context("remote NDJSON member has no parent")?;
    let khive = kg.parent().context("remote NDJSON kg has no parent")?;
    for directory in [khive, kg] {
        let metadata = match std::fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("stat {}", directory.display()))
            }
        };
        if !metadata.file_type().is_dir() {
            bail!(
                "remote NDJSON directory {} is not a regular directory",
                directory.display()
            );
        }
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
    };
    if !metadata.file_type().is_file() {
        bail!(
            "remote NDJSON member {} is not a regular file",
            path.display()
        );
    }
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening remote NDJSON member {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!(
            "remote NDJSON member {} is not a regular file",
            path.display()
        );
    }
    Ok(Some(file))
}

fn read_remote_entities(path: &Path) -> Result<Vec<NdjsonEntity>> {
    match read_remote_regular_file(path)? {
        Some(text) => read_ndjson(&text, "entity"),
        None => Ok(Vec::new()),
    }
}

fn read_remote_edges(path: &Path) -> Result<Vec<NdjsonEdge>> {
    match read_remote_regular_file(path)? {
        Some(text) => read_ndjson(&text, "edge"),
        None => Ok(Vec::new()),
    }
}

async fn checkpoint_wal(runtime: &KhiveRuntime) -> Result<()> {
    let mut writer = runtime.backend().sql().writer().await?;
    // Top-level statement — a checkpoint cannot complete while a transaction
    // is open on the same connection. `execute_script` wraps its statement in
    // the WriterTask's per-request BEGIN IMMEDIATE under KHIVE_WRITE_QUEUE=1,
    // which would make this call silently no-op the checkpoint (the subsequent
    // rename above would then lose data — see the comment at the call site).
    writer
        .execute_script_top_level(khive_storage::TopLevelMaintenance::WalCheckpointTruncate)
        .await?;
    Ok(())
}

// Number of rows committed per SQLite transaction during bulk sync.
// 10_000 keeps WAL growth per chunk below ~40 MiB at an average 4 KiB entity,
// while amortising transaction overhead across many rows.
// In test builds the value is reduced so chunk-boundary tests run without
// generating tens of thousands of synthetic rows.
#[cfg(not(test))]
const SYNC_CHUNK_SIZE: usize = 10_000;
#[cfg(test)]
const SYNC_CHUNK_SIZE: usize = 5;

async fn upsert_entities(
    runtime: &KhiveRuntime,
    namespace: &str,
    records: Vec<NdjsonEntity>,
) -> Result<usize> {
    let ns = khive_types::Namespace::parse(namespace)
        .map_err(|e| anyhow!("invalid namespace {namespace:?}: {e}"))?;
    let token = runtime.authorize(ns)?;
    let store = runtime.entities(&token).context("opening entity store")?;
    let text = runtime.text(&token).context("opening text store")?;

    // Convert and write SYNC_CHUNK_SIZE records at a time so that peak
    // converted-buffer memory is O(SYNC_CHUNK_SIZE), not O(records.len()).
    // Field mapping is identical to the previous per-row loop so that
    // sync, create, update, merge, and reindex produce identical shapes.
    let mut count = 0usize;
    for chunk in records.chunks(SYNC_CHUNK_SIZE) {
        let mut entities_chunk = Vec::with_capacity(chunk.len());
        let mut docs_chunk = Vec::with_capacity(chunk.len());
        for r in chunk {
            let fallback = Utc::now();
            let created_at = parse_ts_micros(r.created_at.as_deref(), fallback)
                .with_context(|| format!("entity {} invalid created_at", r.id))?;
            let updated_at = parse_ts_micros(r.updated_at.as_deref(), fallback)
                .with_context(|| format!("entity {} invalid updated_at", r.id))?;
            let entity = khive_storage::entity::Entity {
                entity_type: r.entity_type.clone(),
                description: r.description.clone(),
                properties: r.properties.clone(),
                tags: r.tags.clone(),
                ..khive_storage::entity::Entity::minimal(
                    r.id,
                    namespace,
                    r.kind.clone(),
                    r.name.clone(),
                    created_at,
                    updated_at,
                )
            };
            khive_runtime::secret_gate::reject_reserved_secret_gate_property(
                entity.properties.as_ref(),
            )
            .map_err(|error| anyhow!("entity {} properties rejected: {error}", entity.id))?;
            // Use the canonical FTS document constructor so sync, create, update,
            // merge, and reindex all produce identical document shapes.
            let fts_doc = entity_fts_document(&entity);
            entities_chunk.push(entity);
            docs_chunk.push(fts_doc);
        }

        // Entity rows — one BEGIN IMMEDIATE / COMMIT per chunk.
        let summary = store
            .upsert_entities(entities_chunk)
            .await
            .context("batch upsert entities")?;
        if summary.failed > 0 {
            return Err(anyhow!(
                "entity write: {}/{} rows failed (first: {})",
                summary.failed,
                summary.attempted,
                summary.first_error
            ));
        }
        count += summary.affected as usize;

        // FTS docs — one BEGIN IMMEDIATE / COMMIT per chunk.
        // Vectors are intentionally skipped: they are local-only derived state
        // and can be repaired explicitly by `kkernel reindex` when needed.
        let summary = text
            .upsert_documents(docs_chunk)
            .await
            .context("batch FTS upsert")?;
        if summary.failed > 0 {
            return Err(anyhow!(
                "FTS write: {}/{} docs failed in chunk",
                summary.failed,
                summary.attempted
            ));
        }
    }
    Ok(count)
}

async fn upsert_edges(
    runtime: &KhiveRuntime,
    namespace: &str,
    records: Vec<NdjsonEdge>,
) -> Result<usize> {
    let ns = khive_types::Namespace::parse(namespace)
        .map_err(|e| anyhow!("invalid namespace {namespace:?}: {e}"))?;
    let token = runtime.authorize(ns)?;
    let graph = runtime.graph(&token).context("opening graph store")?;

    // Convert and write SYNC_CHUNK_SIZE edges at a time so that peak
    // converted-buffer memory is O(SYNC_CHUNK_SIZE), not O(records.len()).
    // Edge relation validation already ran in run_sync before the tmp DB was
    // created, so parse() here should always succeed.
    // upsert_edges rolls back the entire chunk on the first storage error
    // and returns Err, which propagates via ? without advancing count.
    let mut count = 0usize;
    for chunk in records.chunks(SYNC_CHUNK_SIZE) {
        let mut edge_chunk = Vec::with_capacity(chunk.len());
        for r in chunk {
            let relation: EdgeRelation = r
                .relation
                .parse()
                .map_err(|e| anyhow!("invalid relation {:?}: {}", r.relation, e))?;
            // ADR-115 Amendment 1 §3: edge metadata is a properties-bearing
            // write path in the unchanged blocking-scanner class; the
            // runtime-owned `khive:secret_gate` key is reservation-only and
            // credential-shaped values are rejected, sync included.
            khive_runtime::secret_gate::reject_reserved_secret_gate_property(r.properties.as_ref())
                .map_err(|e| anyhow!("edge {} properties rejected: {e}", r.edge_id))?;
            if let Some(p) = r.properties.as_ref() {
                khive_runtime::secret_gate::check_json(p)
                    .map_err(|e| anyhow!("edge {} properties rejected: {e}", r.edge_id))?;
            }
            let fallback = Utc::now();
            let created_at = parse_timestamp(r.created_at.as_deref(), fallback)
                .with_context(|| format!("edge {} invalid created_at", r.edge_id))?;
            let updated_at = parse_timestamp(r.updated_at.as_deref(), fallback)
                .with_context(|| format!("edge {} invalid updated_at", r.edge_id))?;
            let edge = Edge {
                id: LinkId::from(r.edge_id),
                namespace: namespace.to_string(),
                source_id: r.source,
                target_id: r.target,
                relation,
                weight: r.weight,
                created_at,
                updated_at,
                deleted_at: None,
                metadata: r.properties.clone(),
                target_backend: None,
            };
            edge_chunk.push(edge);
        }
        let summary = graph
            .upsert_edges(edge_chunk)
            .await
            .context("batch upsert edges")?;
        count += summary.affected as usize;
    }
    Ok(count)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

// IN-CRATE TEST JUSTIFICATION: Tests access private helpers (build_kg_archive,
// read_entities, read_edges, compute_pin) that cannot be exposed in crate-level
// tests/ without promoting them to pub(crate), which would widen the internal API.
// The local and remote paths share private validation helpers above this line.
#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;

// ── ADR-067 Fork C slice 2 (sibling of memory.vacuum's Fork C slice 2
// fix): `checkpoint_wal` under the write queue ───────────────────────────
//
// `checkpoint_wal` used to send `"PRAGMA wal_checkpoint(TRUNCATE);"` via plain
// `execute_script`, which — once `execute_script` started routing through the
// WriterTask (ADR-067 Component A) under `KHIVE_WRITE_QUEUE=1` — landed inside
// the WriterTask's own per-request `BEGIN IMMEDIATE`. A checkpoint cannot
// complete while a transaction is open on the same connection, so the
// checkpoint would silently no-op and the subsequent `rename(tmp, target)`
// (see the comment at the `checkpoint_wal` call site above) would drop any
// writes still sitting in the `-wal` file. This proves the fixed
// `execute_script_top_level` path (no BEGIN/COMMIT/ROLLBACK wrap) succeeds
// with the write queue enabled.
//
// `KhiveRuntime` has no config-injection point for `PoolConfig` (production
// construction hardcodes `PoolConfig::default()`), so — mirroring the
// `memory.vacuum` regression test in khive-pack-memory's `prune.rs` — this
// drives the underlying mechanism directly at the `SqlBridge` level: the same
// `execute_script_top_level(TopLevelMaintenance::WalCheckpointTruncate)` call that
// `checkpoint_wal` makes, over a `PoolConfig { write_queue_enabled: Some(true), .. }`
// literal (no env var mutation, no cross-test race).
#[cfg(test)]
mod checkpoint_wal_write_queue_tests {
    #[tokio::test]
    async fn wal_checkpoint_truncate_succeeds_with_write_queue_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("vcs-checkpoint-write-queue.db");
        let pool_cfg = khive_db::PoolConfig {
            path: Some(db_path),
            write_queue_enabled: Some(true),
            ..khive_db::PoolConfig::for_test()
        };
        let pool = std::sync::Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
        pool.run_migrations().expect("migrations");
        assert!(
            pool.writer_task_handle().unwrap().is_some(),
            "writer task must be spawned with the flag on for a file-backed pool"
        );

        let sql: std::sync::Arc<dyn khive_storage::SqlAccess> =
            std::sync::Arc::new(khive_db::SqlBridge::new(std::sync::Arc::clone(&pool), true));

        let mut writer = sql.writer().await.expect("writer handle");
        let result = writer
            .execute_script_top_level(khive_storage::TopLevelMaintenance::WalCheckpointTruncate)
            .await;

        assert!(
            result.is_ok(),
            "PRAGMA wal_checkpoint(TRUNCATE) via execute_script_top_level must succeed under \
             KHIVE_WRITE_QUEUE (no BEGIN IMMEDIATE wrap); got {result:?}"
        );
    }

    /// Revert-and-confirm-fails companion to the test above. See
    /// `docs/api/sync.md#wal-checkpoint-under-the-write-queue`.
    #[tokio::test]
    async fn wal_checkpoint_truncate_via_plain_execute_script_fails_with_write_queue_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("vcs-checkpoint-write-queue-regression.db");
        let pool_cfg = khive_db::PoolConfig {
            path: Some(db_path),
            write_queue_enabled: Some(true),
            ..khive_db::PoolConfig::for_test()
        };
        let pool = std::sync::Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
        pool.run_migrations().expect("migrations");

        let sql: std::sync::Arc<dyn khive_storage::SqlAccess> =
            std::sync::Arc::new(khive_db::SqlBridge::new(std::sync::Arc::clone(&pool), true));

        let mut writer = sql.writer().await.expect("writer handle");
        let result = writer
            .execute_script("PRAGMA wal_checkpoint(TRUNCATE);".to_string())
            .await;

        assert!(
            result.is_err(),
            "PRAGMA wal_checkpoint(TRUNCATE) via plain execute_script must FAIL under \
             KHIVE_WRITE_QUEUE (it wraps in BEGIN IMMEDIATE, and SQLite rejects a WAL \
             checkpoint inside an open transaction); got {result:?} — if this now passes, \
             the WriterTask no longer wraps execute_script in a transaction and this whole \
             regression class needs re-auditing"
        );
    }
}

#[cfg(test)]
#[tokio::test]
async fn issue2673_sync_upserts_advance_destination_entity_version() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    for version in 1..=3 {
        let record = NdjsonEntity {
            id,
            kind: "concept".into(),
            entity_type: None,
            name: format!("sync revision {version}"),
            description: None,
            properties: None,
            tags: vec![],
            created_at: None,
            updated_at: None,
        };
        assert_eq!(
            upsert_entities(&runtime, "local", vec![record])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            runtime.get_entity(&token, id).await.unwrap().version,
            version
        );
    }
}

#[cfg(test)]
#[tokio::test]
async fn direct_entity_upsert_refuses_reserved_properties_before_storage() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    let record = NdjsonEntity {
        id,
        kind: "concept".into(),
        entity_type: None,
        name: "reserved sync row".into(),
        description: None,
        properties: Some(serde_json::json!({
            "khive:secret_gate": "exempted:content-sha256-manifest-v1"
        })),
        tags: vec![],
        created_at: None,
        updated_at: None,
    };
    let error = upsert_entities(&runtime, "local", vec![record])
        .await
        .expect_err("direct sync writer must refuse the reserved property");
    assert!(error.to_string().contains("khive:secret_gate"), "{error}");
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .is_none());
}
