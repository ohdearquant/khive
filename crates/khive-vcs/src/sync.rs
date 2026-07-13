//! NDJSON-to-SQLite sync library boundary.
//!
//! Rebuilds the SQLite database from `.khive/kg/entities.ndjson` and `edges.ndjson`.
//! Builds atomically into a `.tmp` file then renames. Also supports remote archive
//! fetch with SHA-256 pin verification via [`run_sync_remote`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_runtime::{entity_fts_document, KhiveRuntime, RuntimeConfig};
use khive_storage::types::Edge;
use khive_storage::LinkId;
use khive_types::{EdgeRelation, EntityKind};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::VcsError;
use crate::hash::snapshot_id_for_archive;
use crate::types::SnapshotId;

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
    // properties: accepted but not yet persisted to the storage-layer Edge
    // struct. Parsed here so existing NDJSON files round-trip without warning.
    #[serde(default)]
    // REASON: Accepted for NDJSON round-trip compatibility; not yet persisted to the Edge struct.
    #[allow(dead_code)]
    properties: Option<serde_json::Value>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    // REASON: Accepted for NDJSON round-trip compatibility; edge updated_at is derived from created_at.
    #[allow(dead_code)]
    updated_at: Option<String>,
}

fn default_weight() -> f64 {
    1.0
}

/// Parse an ISO-8601 timestamp string into microseconds since epoch.
/// Returns `now` if the string is `None` or unparseable.
fn parse_ts_micros(s: Option<&str>) -> i64 {
    s.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_micros())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_micros())
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

        entities_ndjson = read_entities(&entities_path)
            .with_context(|| format!("reading staged {}", entities_path.display()))?;
        edges_ndjson = read_edges(&edges_path)
            .with_context(|| format!("reading staged {}", edges_path.display()))?;
    }
    // `staging` tempdir is still alive here — we drop it after moving files.

    // ── 3. Build KgArchive and compute canonical hash ─────────────────────────
    // build_kg_archive is fallible: an invalid relation causes it to return an
    // error here, before any cache file is written (fail-closed).
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
/// publish step so tests can assert readers never observe a mixed cache state.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishFailAt {
    AfterEntities,
    AfterEdges,
    AfterMeta,
    BeforeSwap,
}

/// Publish a complete `{entities.ndjson, edges.ndjson, meta.json}` triple to
/// `<remotes_root>/<name>/` as a single atomic unit.
///
/// Builds a complete staging directory (a sibling of the cache directory,
/// under `remotes_root`) containing all three files, then switches visibility
/// with one directory-rename swap ([`atomic_replace_dir`]). A crash or error
/// at any point before the swap leaves the existing cache untouched; a crash
/// or error during the swap either leaves the old cache in place or completes
/// to the new cache — a reader never observes a mix of old and new files.
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

    let cache_dir = remotes_root.join(name);
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
/// If the second rename fails, the backup is restored so the old cache is
/// never lost. Both renames are single filesystem rename(2) calls, each of
/// which is atomic — at every instant `target_dir` resolves to either the
/// complete old directory, is briefly absent, or resolves to the complete new
/// directory; it never contains a mix of old and new files.
fn atomic_replace_dir(new_dir: &Path, target_dir: &Path) -> Result<()> {
    if !target_dir.exists() {
        std::fs::rename(new_dir, target_dir).with_context(|| {
            format!("renaming {} -> {}", new_dir.display(), target_dir.display())
        })?;
        return Ok(());
    }

    let backup = target_dir.with_file_name(format!(
        "{}.replaced-{}",
        target_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("cache"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&backup);

    std::fs::rename(target_dir, &backup).with_context(|| {
        format!(
            "backing up existing cache {} -> {}",
            target_dir.display(),
            backup.display()
        )
    })?;

    match std::fs::rename(new_dir, target_dir) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(())
        }
        Err(e) => {
            // Restore the old cache so a failed swap never leaves the target missing.
            let _ = std::fs::rename(&backup, target_dir);
            Err(e).with_context(|| {
                format!(
                    "renaming {} -> {} (old cache restored)",
                    new_dir.display(),
                    target_dir.display()
                )
            })
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

/// Redact URLs and embedded credentials from git stderr before surfacing in errors.
///
/// git on auth failure can include the full remote URL in stderr, which may carry
/// a `user:token@host` credential form.  ADR-037 §157 prohibits leaking remote
/// URLs in errors.  This function replaces any `scheme://[…@]host/path` token or
/// scp-style `user@host:path` remote with `<url-redacted>` so the sanitised text
/// is still useful for diagnostics while credentials and remote addresses stay out
/// of logs.
///
/// Handled forms:
/// - `scheme://[user:pass@]host/path` (HTTPS, SSH scheme URLs)
/// - `user@host:path` (scp-style SSH remotes, e.g. `git@github.com:org/repo.git`)
fn redact_git_stderr(raw: &str) -> String {
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

/// Returns `true` when position `i` in `bytes` is the `@` of an scp-style remote.
///
/// An scp remote looks like `word@host:path` where the colon is followed by a
/// non-whitespace character (to distinguish `user@host:path` from
/// `user@host: message text`).  We require that the character after `:` is
/// neither a space, a tab, nor another `:` (which would indicate an IPv6 address
/// or a port in a scheme URL already handled by the `://` branch).
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
        .map(|e| ExportedEntity {
            id: e.id,
            kind: e.kind.clone(),
            entity_type: e.entity_type.clone(),
            name: e.name.clone(),
            description: e.description.clone(),
            properties: e.properties.clone(),
            tags: e.tags.clone(),
            created_at: e
                .created_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(now),
            updated_at: e
                .updated_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(now),
        })
        .collect();

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
    sorted.sort_by(|a, b| {
        a.id.to_string()
            .to_ascii_lowercase()
            .cmp(&b.id.to_string().to_ascii_lowercase())
    });
    let mut lines = Vec::with_capacity(sorted.len());
    for r in sorted {
        let line = serde_json::to_string(r).context("serializing entity")?;
        lines.push(line);
    }
    std::fs::write(path, lines.join("\n")).context("writing entities file")?;
    Ok(())
}

/// Write edges to a file as sorted NDJSON (one JSON object per line).
///
/// Edges are sorted by (source, target, relation) to match the canonical sort
/// order used by `snapshot_id_for_archive`.
fn write_sorted_edges(path: &Path, records: &[NdjsonEdge]) -> Result<()> {
    let mut sorted: Vec<&NdjsonEdge> = records.iter().collect();
    sorted.sort_by(|a, b| {
        let ak = (
            a.source.to_string(),
            a.target.to_string(),
            a.relation.clone(),
        );
        let bk = (
            b.source.to_string(),
            b.target.to_string(),
            b.relation.clone(),
        );
        ak.cmp(&bk)
    });
    let mut lines = Vec::with_capacity(sorted.len());
    for r in sorted {
        let line = serde_json::to_string(r).context("serializing edge")?;
        lines.push(line);
    }
    std::fs::write(path, lines.join("\n")).context("writing edges file")?;
    Ok(())
}

/// Full ADR-020 structural validation of parsed NDJSON records (#476).
///
/// Checks entity kind validity, entity/edge timestamp validity, entity/edge
/// sort order (matching `write_sorted_entities`/`write_sorted_edges`), duplicate
/// entity ids, duplicate edge ids, duplicate semantic edge triples
/// (source, target, relation), dangling edge endpoints, and edge relation/weight
/// validity. Called before any temp DB is created so a violation here leaves
/// the existing target DB completely untouched.
fn validate_ndjson_records(entities: &[NdjsonEntity], edges: &[NdjsonEdge]) -> Result<()> {
    let mut entity_ids: HashSet<Uuid> = HashSet::with_capacity(entities.len());
    let mut prev_entity_key: Option<String> = None;
    for (i, e) in entities.iter().enumerate() {
        EntityKind::from_str(&e.kind)
            .map_err(|_| anyhow!("entity {i} ({}): unknown kind {:?}", e.id, e.kind))?;

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

        if !triples.insert((r.source, r.target, relation)) {
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

/// Rebuild `db_path` from `.khive/kg/{entities,edges}.ndjson` under `repo_root`.
///
/// The operation is atomic: the database is built in a `.tmp` sibling file and
/// renamed over `db_path` only on success. A crash or error leaves the previous
/// `db_path` intact.
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

    let tmp_path = with_extension_suffix(db_path, ".tmp");
    let _ = std::fs::remove_file(&tmp_path);

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

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::rename(&tmp_path, db_path)
        .with_context(|| format!("renaming {} -> {}", tmp_path.display(), db_path.display()))?;

    Ok(SyncReport {
        entities: entity_count,
        edges: edge_count,
        db_path: db_path.to_string_lossy().into_owned(),
    })
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
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let e: NdjsonEntity = serde_json::from_str(trimmed)
            .with_context(|| format!("parsing entity at line {}", i + 1))?;
        out.push(e);
    }
    Ok(out)
}

fn read_edges(path: &Path) -> Result<Vec<NdjsonEdge>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let e: NdjsonEdge = serde_json::from_str(trimmed)
            .with_context(|| format!("parsing edge at line {}", i + 1))?;
        out.push(e);
    }
    Ok(out)
}

async fn checkpoint_wal(runtime: &KhiveRuntime) -> Result<()> {
    let mut writer = runtime.backend().sql().writer().await?;
    // Top-level statement — a checkpoint cannot complete while a transaction
    // is open on the same connection. `execute_script` wraps its statement in
    // the WriterTask's per-request BEGIN IMMEDIATE under KHIVE_WRITE_QUEUE=1,
    // which would make this call silently no-op the checkpoint (the subsequent
    // rename above would then lose data — see the comment at the call site).
    writer
        .execute_script_top_level("PRAGMA wal_checkpoint(TRUNCATE);".to_string())
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
            let created_at = parse_ts_micros(r.created_at.as_deref());
            let updated_at = parse_ts_micros(r.updated_at.as_deref());
            let entity = khive_storage::entity::Entity {
                id: r.id,
                namespace: namespace.to_string(),
                kind: r.kind.clone(),
                entity_type: r.entity_type.clone(),
                name: r.name.clone(),
                description: r.description.clone(),
                properties: r.properties.clone(),
                tags: r.tags.clone(),
                created_at,
                updated_at,
                deleted_at: None,
                merge_event_id: None,
                merged_into: None,
                content_ref: None,
            };
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
        // and will be computed by `kkernel kg embed` when needed.
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
            let created_at =
                chrono::DateTime::from_timestamp_micros(parse_ts_micros(r.created_at.as_deref()))
                    .unwrap_or_else(chrono::Utc::now);
            let edge = Edge {
                id: LinkId::from(r.edge_id),
                namespace: namespace.to_string(),
                source_id: r.source,
                target_id: r.target,
                relation,
                weight: r.weight,
                created_at,
                updated_at: created_at,
                deleted_at: None,
                metadata: None,
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

// INLINE TEST JUSTIFICATION: Tests access private helpers (build_kg_archive,
// read_entities, read_edges, compute_pin) that cannot be exposed in crate-level
// tests/ without promoting them to pub(crate), which would widen the internal API.
// Production code above this line is ~625 LOC (under the 700-line gate).
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── F201 test helpers ─────────────────────────────────────────────────────

    /// Create a minimal git repository under `dir` with the given NDJSON content
    /// inside `.khive/kg/`. Returns the URL-style path suitable for `git clone`.
    fn make_git_remote(dir: &Path, entities_ndjson: &str, edges_ndjson: &str) -> String {
        let kg_dir = dir.join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        std::fs::write(kg_dir.join("entities.ndjson"), entities_ndjson).unwrap();
        std::fs::write(kg_dir.join("edges.ndjson"), edges_ndjson).unwrap();

        // Initialise git repo with a single commit on `main`.
        run_git(dir, &["init", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-m", "init"]);

        dir.to_string_lossy().into_owned()
    }

    fn run_git(dir: &Path, args: &[&str]) {
        // Hermetic: user-level core.hooksPath (e.g. the machine-wide JSON/JSONL
        // data-leak guard) must not run against fixture commits in temp repos.
        let status = Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
        assert!(
            status.success(),
            "git {} exited with {}",
            args.join(" "),
            status
        );
    }

    /// Compute the canonical `SnapshotId` for entity/edge NDJSON strings without
    /// touching the filesystem, so we can build expected pins from in-memory data.
    fn compute_pin(entities_ndjson: &str, edges_ndjson: &str, namespace: &str) -> SnapshotId {
        let tmp = TempDir::new().unwrap();
        let kg = tmp.path().join(".khive/kg");
        std::fs::create_dir_all(&kg).unwrap();
        std::fs::write(kg.join("entities.ndjson"), entities_ndjson).unwrap();
        std::fs::write(kg.join("edges.ndjson"), edges_ndjson).unwrap();

        let entities = read_entities(&kg.join("entities.ndjson")).unwrap();
        let edges = read_edges(&kg.join("edges.ndjson")).unwrap();
        let archive = build_kg_archive(namespace, &entities, &edges).unwrap();
        snapshot_id_for_archive(&archive).unwrap()
    }

    // ── test_run_sync_local_path_unchanged_behavior ───────────────────────────

    fn write_repo(dir: &Path, entities_ndjson: &str, edges_ndjson: &str) {
        let kg_dir = dir.join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        std::fs::write(kg_dir.join("entities.ndjson"), entities_ndjson).unwrap();
        std::fs::write(kg_dir.join("edges.ndjson"), edges_ndjson).unwrap();
    }

    #[tokio::test]
    async fn sync_empty_ndjson_produces_real_sqlite_file() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        write_repo(repo, "", "");

        let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
        assert_eq!(report.entities, 0);
        assert_eq!(report.edges, 0);

        let bytes = std::fs::read(&db_path).unwrap();
        assert!(!bytes.is_empty(), "DB file must be non-empty after sync");
        assert!(
            bytes.starts_with(b"SQLite format 3\0"),
            "DB file must start with SQLite magic header, got {:?}",
            &bytes[..bytes.len().min(20)]
        );
    }

    #[tokio::test]
    async fn sync_imports_entities_and_edges_into_real_db() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");

        let id_a = "11111111-1111-1111-1111-111111111111";
        let id_b = "22222222-2222-2222-2222-222222222222";
        let edge_id = "33333333-3333-3333-3333-333333333333";

        let line_a = format!(
            r#"{{"id":"{id_a}","kind":"concept","name":"Alpha","properties":{{}},"tags":[]}}"#
        );
        let line_b = format!(
            r#"{{"id":"{id_b}","kind":"concept","name":"Beta","properties":{{}},"tags":[]}}"#
        );
        let entities = format!("{line_a}\n{line_b}\n");
        let edges = format!(
            r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":1.0,"properties":{{}}}}"#
        );
        write_repo(repo, &entities, &edges);

        let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
        assert_eq!(report.entities, 2);
        assert_eq!(report.edges, 1);

        let ns = khive_types::Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..RuntimeConfig::default()
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let token = rt.authorize(ns).unwrap();
        let alpha = rt
            .entities(&token)
            .unwrap()
            .get_entity(id_a.parse().unwrap())
            .await
            .unwrap()
            .expect("entity Alpha must be retrievable after sync");
        assert_eq!(alpha.name, "Alpha");
        assert_eq!(alpha.kind, "concept");
    }

    /// Chunk-boundary round-trip: write N > SYNC_CHUNK_SIZE entities and edges
    /// so the batching path exercises at least two transaction boundaries, then
    /// verify the full count and spot-check the last record of the final chunk.
    ///
    /// In test builds SYNC_CHUNK_SIZE = 5, so N = 11 produces three chunks
    /// (5 + 5 + 1) for both entities and edges.
    #[tokio::test]
    async fn sync_chunk_boundary_round_trip() {
        const N: usize = 11; // > SYNC_CHUNK_SIZE (5 in test mode)

        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");

        // Generate N synthetic entity lines with predictable UUIDs.
        let ids: Vec<uuid::Uuid> = (0..N).map(|_| uuid::Uuid::new_v4()).collect();
        // #476 requires entities.ndjson to be in canonical ascending-by-id
        // order (matching `write_sorted_entities`), so sort the lines below
        // even though `ids` itself stays in generation order (used to build
        // the wrap-around edges and to spot-check the last-generated entity).
        let mut entity_lines: Vec<(uuid::Uuid, String)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                (
                    *id,
                    format!(
                        r#"{{"id":"{id}","kind":"concept","name":"SyntheticEntity{i}","description":"Synthetic test entity {i} for chunk-boundary coverage","properties":{{}},"tags":["bench","synthetic"]}}"#
                    ),
                )
            })
            .collect();
        entity_lines.sort_by(|a, b| {
            a.0.to_string()
                .to_ascii_lowercase()
                .cmp(&b.0.to_string().to_ascii_lowercase())
        });
        let entities_ndjson = entity_lines
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n");

        // Generate N synthetic edges: each entity points at the next one via
        // "extends". The last entity wraps back to the first. #476 requires
        // edges.ndjson sorted by (source, target, relation), matching
        // `write_sorted_edges`.
        let mut edge_lines: Vec<((String, String, String), String)> = ids
            .iter()
            .enumerate()
            .map(|(i, &src)| {
                let tgt = ids[(i + 1) % N];
                let eid = uuid::Uuid::new_v4();
                let key = (src.to_string(), tgt.to_string(), "extends".to_string());
                let line = format!(
                    r#"{{"edge_id":"{eid}","source":"{src}","target":"{tgt}","relation":"extends","weight":0.9,"properties":{{}}}}"#
                );
                (key, line)
            })
            .collect();
        edge_lines.sort_by(|a, b| a.0.cmp(&b.0));
        let edges_ndjson = edge_lines
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n");

        write_repo(repo, &entities_ndjson, &edges_ndjson);

        let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
        assert_eq!(report.entities, N, "all {N} entities must be written");
        assert_eq!(report.edges, N, "all {N} edges must be written");

        // Spot-check: the last entity (in the final partial chunk) must be readable.
        let last_id = *ids.last().unwrap();
        let ns = khive_types::Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..RuntimeConfig::default()
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let token = rt.authorize(ns).unwrap();
        let last_entity = rt
            .entities(&token)
            .unwrap()
            .get_entity(last_id)
            .await
            .unwrap()
            .expect("last entity (final chunk) must be readable after sync");
        assert_eq!(
            last_entity.name,
            format!("SyntheticEntity{}", N - 1),
            "last entity name must match"
        );
    }

    /// Error-abort: a parse failure before any DB write leaves the existing DB intact.
    /// This covers the failure-semantics contract: sync aborts on the first error
    /// and the target DB is never replaced by a partial result.
    #[tokio::test]
    async fn sync_aborts_on_invalid_ndjson_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        std::fs::write(&db_path, b"ORIGINAL").unwrap();

        // Mix valid entities with one invalid line to trigger a parse error
        // before any DB write.
        let id_a = uuid::Uuid::new_v4();
        let good_line = format!(
            r#"{{"id":"{id_a}","kind":"concept","name":"Good","properties":{{}},"tags":[]}}"#
        );
        let bad_ndjson = format!("{good_line}\nnot-valid-json\n");
        write_repo(repo, &bad_ndjson, "");

        let err = run_sync(repo, &db_path, "test-ns")
            .await
            .expect_err("sync must fail on invalid NDJSON");
        assert!(
            err.to_string().contains("parsing entity")
                || err.chain().any(|e| e.to_string().contains("expected")),
            "error must describe the parse failure, got: {err}"
        );

        // DB must be untouched (atomic rename guarantee).
        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(
            after, b"ORIGINAL",
            "failed sync must not replace existing DB"
        );
    }

    #[tokio::test]
    async fn sync_is_atomic_via_tmp_rename() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        std::fs::write(&db_path, b"SENTINEL").unwrap();

        write_repo(repo, "not json\n", "");
        let err = run_sync(repo, &db_path, "test-ns").await.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("parsing entity")
                || err.chain().any(|e| e.to_string().contains("expected")),
            "expected parse error, got: {err}"
        );

        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(
            after, b"SENTINEL",
            "atomic guarantee: failed sync must not replace existing DB"
        );
    }

    #[tokio::test]
    async fn sync_missing_ndjson_files_succeeds_with_zero_counts() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");

        let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
        assert_eq!(report.entities, 0);
        assert_eq!(report.edges, 0);
    }

    // ── #476: validate-first gate — one test per violation type ─────────────────
    //
    // Each test writes a sentinel DB file, runs `run_sync` against NDJSON that
    // violates exactly one structural rule, asserts `run_sync` returns `Err`,
    // and asserts the sentinel DB file is byte-unchanged: the violation must be
    // caught before the temp DB is even created, let alone renamed over the
    // target.

    async fn assert_sync_rejected_before_db_write(
        repo: &Path,
        db_path: &Path,
        entities_ndjson: &str,
        edges_ndjson: &str,
        expected_substr: &str,
    ) {
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        std::fs::write(db_path, b"SENTINEL").unwrap();
        write_repo(repo, entities_ndjson, edges_ndjson);

        let err = run_sync(repo, db_path, "test-ns")
            .await
            .expect_err("run_sync must reject invalid NDJSON");
        assert!(
            err.chain().any(|e| e.to_string().contains(expected_substr)),
            "error must mention {expected_substr:?}, got: {err:#}"
        );

        let after = std::fs::read(db_path).unwrap();
        assert_eq!(
            after, b"SENTINEL",
            "rejected sync must leave the target DB completely untouched"
        );
    }

    #[tokio::test]
    async fn sync_rejects_unknown_entity_kind_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id = "11111111-1111-1111-1111-111111111111";
        let entities = format!(
            r#"{{"id":"{id}","kind":"not-a-real-kind","name":"Bad","properties":{{}},"tags":[]}}"#
        );

        assert_sync_rejected_before_db_write(repo, &db_path, &entities, "", "unknown kind").await;
    }

    #[tokio::test]
    async fn sync_rejects_out_of_range_edge_weight_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id_a = "11111111-1111-1111-1111-111111111111";
        let id_b = "22222222-2222-2222-2222-222222222222";
        let entities = [
            format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
            format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
        ]
        .join("\n");
        let edge_id = "33333333-3333-3333-3333-333333333333";
        let edges = format!(
            r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":1.5,"properties":{{}}}}"#
        );

        assert_sync_rejected_before_db_write(repo, &db_path, &entities, &edges, "out of range")
            .await;
    }

    #[tokio::test]
    async fn sync_rejects_duplicate_entity_ids_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id = "11111111-1111-1111-1111-111111111111";
        let entities = [
            format!(r#"{{"id":"{id}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
            format!(r#"{{"id":"{id}","kind":"concept","name":"A2","properties":{{}},"tags":[]}}"#),
        ]
        .join("\n");

        assert_sync_rejected_before_db_write(repo, &db_path, &entities, "", "duplicate entity id")
            .await;
    }

    #[tokio::test]
    async fn sync_rejects_duplicate_edge_ids_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id_a = "11111111-1111-1111-1111-111111111111";
        let id_b = "22222222-2222-2222-2222-222222222222";
        let id_c = "33333333-3333-3333-3333-333333333333";
        let entities = [
            format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
            format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
            format!(r#"{{"id":"{id_c}","kind":"concept","name":"C","properties":{{}},"tags":[]}}"#),
        ]
        .join("\n");
        let edge_id = "44444444-4444-4444-4444-444444444444";
        let edges = [
            format!(r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
            format!(r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_c}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
        ]
        .join("\n");

        assert_sync_rejected_before_db_write(
            repo,
            &db_path,
            &entities,
            &edges,
            "duplicate edge id",
        )
        .await;
    }

    #[tokio::test]
    async fn sync_rejects_duplicate_edge_triples_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id_a = "11111111-1111-1111-1111-111111111111";
        let id_b = "22222222-2222-2222-2222-222222222222";
        let entities = [
            format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
            format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
        ]
        .join("\n");
        let edge_id_1 = "33333333-3333-3333-3333-333333333333";
        let edge_id_2 = "44444444-4444-4444-4444-444444444444";
        let edges = [
            format!(r#"{{"edge_id":"{edge_id_1}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
            format!(r#"{{"edge_id":"{edge_id_2}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.9,"properties":{{}}}}"#),
        ]
        .join("\n");

        assert_sync_rejected_before_db_write(
            repo,
            &db_path,
            &entities,
            &edges,
            "duplicate edge triple",
        )
        .await;
    }

    #[tokio::test]
    async fn sync_rejects_dangling_edge_source_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id_b = "22222222-2222-2222-2222-222222222222";
        let missing_source = "99999999-9999-9999-9999-999999999999";
        let entities =
            format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#);
        let edge_id = "33333333-3333-3333-3333-333333333333";
        let edges = format!(
            r#"{{"edge_id":"{edge_id}","source":"{missing_source}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}}}}"#
        );

        assert_sync_rejected_before_db_write(repo, &db_path, &entities, &edges, "dangling source")
            .await;
    }

    #[tokio::test]
    async fn sync_rejects_dangling_edge_target_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id_a = "11111111-1111-1111-1111-111111111111";
        let missing_target = "99999999-9999-9999-9999-999999999999";
        let entities =
            format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#);
        let edge_id = "33333333-3333-3333-3333-333333333333";
        let edges = format!(
            r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{missing_target}","relation":"extends","weight":0.5,"properties":{{}}}}"#
        );

        assert_sync_rejected_before_db_write(repo, &db_path, &entities, &edges, "dangling target")
            .await;
    }

    #[tokio::test]
    async fn sync_rejects_invalid_entity_timestamp_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id = "11111111-1111-1111-1111-111111111111";
        let entities = format!(
            r#"{{"id":"{id}","kind":"concept","name":"A","properties":{{}},"tags":[],"created_at":"not-a-timestamp"}}"#
        );

        assert_sync_rejected_before_db_write(repo, &db_path, &entities, "", "invalid created_at")
            .await;
    }

    #[tokio::test]
    async fn sync_rejects_invalid_edge_timestamp_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id_a = "11111111-1111-1111-1111-111111111111";
        let id_b = "22222222-2222-2222-2222-222222222222";
        let entities = [
            format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
            format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
        ]
        .join("\n");
        let edge_id = "33333333-3333-3333-3333-333333333333";
        let edges = format!(
            r#"{{"edge_id":"{edge_id}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}},"updated_at":"not-a-timestamp"}}"#
        );

        assert_sync_rejected_before_db_write(
            repo,
            &db_path,
            &entities,
            &edges,
            "invalid updated_at",
        )
        .await;
    }

    #[tokio::test]
    async fn sync_rejects_unsorted_entities_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        // "b..." sorts after "a...", so writing it first violates the
        // ascending-by-lowercase-id order that `write_sorted_entities` enforces.
        let id_hi = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let id_lo = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let entities = [
            format!(
                r#"{{"id":"{id_hi}","kind":"concept","name":"Hi","properties":{{}},"tags":[]}}"#
            ),
            format!(
                r#"{{"id":"{id_lo}","kind":"concept","name":"Lo","properties":{{}},"tags":[]}}"#
            ),
        ]
        .join("\n");

        assert_sync_rejected_before_db_write(
            repo,
            &db_path,
            &entities,
            "",
            "entities.ndjson is not sorted",
        )
        .await;
    }

    #[tokio::test]
    async fn sync_rejects_unsorted_edges_before_db_write() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        let id_a = "11111111-1111-1111-1111-111111111111";
        let id_b = "22222222-2222-2222-2222-222222222222";
        let id_c = "33333333-3333-3333-3333-333333333333";
        let entities = [
            format!(r#"{{"id":"{id_a}","kind":"concept","name":"A","properties":{{}},"tags":[]}}"#),
            format!(r#"{{"id":"{id_b}","kind":"concept","name":"B","properties":{{}},"tags":[]}}"#),
            format!(r#"{{"id":"{id_c}","kind":"concept","name":"C","properties":{{}},"tags":[]}}"#),
        ]
        .join("\n");
        let edge_id_1 = "44444444-4444-4444-4444-444444444444";
        let edge_id_2 = "55555555-5555-5555-5555-555555555555";
        // (source=c, target=a) sorts after (source=a, target=b) lexicographically
        // by UUID string, so writing it first violates edge sort order.
        let edges = [
            format!(r#"{{"edge_id":"{edge_id_1}","source":"{id_c}","target":"{id_a}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
            format!(r#"{{"edge_id":"{edge_id_2}","source":"{id_a}","target":"{id_b}","relation":"extends","weight":0.5,"properties":{{}}}}"#),
        ]
        .join("\n");

        assert_sync_rejected_before_db_write(
            repo,
            &db_path,
            &entities,
            &edges,
            "edges.ndjson is not sorted",
        )
        .await;
    }

    /// #473: `run_sync` must preserve `entity_type` from NDJSON into the
    /// SQLite-backed entity store so subtype-filtered reads see it.
    #[tokio::test]
    async fn sync_preserves_entity_type() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");

        let id_a = "44444444-4444-4444-4444-444444444444";
        let line_a = format!(
            r#"{{"id":"{id_a}","kind":"document","entity_type":"paper","name":"Attention Is All You Need","properties":{{}},"tags":[]}}"#
        );
        write_repo(repo, &line_a, "");

        let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
        assert_eq!(report.entities, 1);

        let ns = khive_types::Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..RuntimeConfig::default()
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let token = rt.authorize(ns).unwrap();
        let entity = rt
            .entities(&token)
            .unwrap()
            .get_entity(id_a.parse().unwrap())
            .await
            .unwrap()
            .expect("entity must be retrievable after sync");
        assert_eq!(
            entity.entity_type.as_deref(),
            Some("paper"),
            "entity_type must survive NDJSON sync, not be stored as NULL"
        );
    }

    /// #473: two archives differing ONLY by `entity_type` must hash to
    /// different `SnapshotId`s — the pin must be injective over entity_type,
    /// not collide with the untyped archive.
    #[test]
    fn remote_hash_includes_ndjson_entity_type() {
        let id_a = "55555555-5555-5555-5555-555555555555";
        let untyped = format!(
            r#"{{"id":"{id_a}","kind":"document","name":"Some Doc","properties":{{}},"tags":[]}}"#
        );
        let typed = format!(
            r#"{{"id":"{id_a}","kind":"document","entity_type":"paper","name":"Some Doc","properties":{{}},"tags":[]}}"#
        );

        let untyped_pin = compute_pin(&untyped, "", "test-ns");
        let typed_pin = compute_pin(&typed, "", "test-ns");

        assert_ne!(
            untyped_pin.as_str(),
            typed_pin.as_str(),
            "entity_type must be part of the canonical hash input; a pin for the \
             untyped archive must not validate typed content"
        );
    }

    /// F195: verify that FTS5 is populated during sync so text search works
    /// after sync without a separate `kkernel kg embed` pass.
    #[tokio::test]
    async fn sync_populates_fts_for_text_search() {
        use khive_runtime::RuntimeConfig;
        use khive_storage::types::{TextFilter, TextQueryMode, TextSearchRequest};

        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");

        let id_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let line_a = format!(
            r#"{{"id":"{id_a}","kind":"concept","name":"FlashAttention","description":"Fast attention algorithm","properties":{{}},"tags":[]}}"#
        );
        write_repo(repo, &line_a, "");

        run_sync(repo, &db_path, "test-ns").await.unwrap();

        let ns = khive_types::Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..RuntimeConfig::default()
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let token = rt.authorize(ns).unwrap();

        let hits = rt
            .text(&token)
            .expect("text store must be available")
            .search(TextSearchRequest {
                query: "FlashAttention".to_string(),
                filter: Some(TextFilter {
                    namespaces: vec!["test-ns".to_string()],
                    ..Default::default()
                }),
                mode: TextQueryMode::Phrase,
                top_k: 10,
                snippet_chars: 128,
            })
            .await
            .expect("text search must succeed after sync");

        assert!(
            !hits.is_empty(),
            "FTS search for 'FlashAttention' must return results after sync (F195)"
        );
        assert_eq!(
            hits[0].subject_id.to_string(),
            id_a,
            "FTS hit must reference the synced entity UUID"
        );
    }

    // ── #474 RemoteName tests (VCS-AUD-002) ─────────────────────────────────────

    /// #474: `RemoteName::parse` must reject every path-traversal / absolute
    /// / separator-containing shape before it can reach `run_sync_remote`.
    #[test]
    fn remote_name_rejects_path_traversal_cases() {
        for bad in [
            "",
            ".",
            "..",
            "../evil",
            "../../outside",
            "/tmp/evil",
            "safe/name",
            "safe\\name",
            "safe/../evil",
        ] {
            assert!(
                RemoteName::parse(bad).is_err(),
                "expected RemoteName::parse to reject {bad:?}"
            );
        }
    }

    /// #474: safe single-segment names must be accepted and preserved verbatim.
    #[test]
    fn remote_name_accepts_single_safe_segment() {
        for good in ["upstream", "team.data-1", "remote_2"] {
            let parsed = RemoteName::parse(good).expect("expected safe name to be accepted");
            assert_eq!(parsed.as_str(), good);
        }
    }

    /// #474: `run_sync_remote("../evil" | "/tmp/evil" | "safe/name")` must be
    /// impossible to even construct — `RemoteConfig::name` is a `RemoteName`,
    /// and `RemoteName::parse` is its only constructor — so these names never
    /// reach `run_sync_remote`'s cache-directory join. Confirm both the
    /// construction-time rejection AND (via filesystem check) that nothing
    /// was created at the traversal targets or under `.khive/kg/remotes/`.
    #[tokio::test]
    async fn run_sync_remote_cannot_be_constructed_with_invalid_name() {
        let repo_dir = TempDir::new().unwrap();
        let outside_target = repo_dir.path().parent().unwrap().join("evil-outside-probe");
        let _ = std::fs::remove_file(&outside_target);

        for bad in ["../evil", "/tmp/evil", "safe/name"] {
            assert!(
                RemoteName::parse(bad).is_err(),
                "RemoteName::parse must reject {bad:?} before any RemoteConfig can be built"
            );
        }

        // No RemoteConfig could be built from these names, so run_sync_remote was
        // never called: the cache tree and any traversal target are both absent.
        assert!(
            !repo_dir.path().join(".khive/kg/remotes").exists(),
            "cache tree must not exist — no sync ever ran"
        );
        assert!(
            !std::path::Path::new("/tmp/evil").exists(),
            "/tmp/evil must not have been created by this test run"
        );
        assert!(
            !outside_target.exists(),
            "nothing must be written outside the repo root"
        );
    }

    // ── #475 atomic publish failure-injection tests (VCS-AUD-003) ───────────────

    fn sample_entity(id: &str, name: &str) -> NdjsonEntity {
        NdjsonEntity {
            id: id.parse().unwrap(),
            kind: "concept".to_string(),
            entity_type: None,
            name: name.to_string(),
            description: None,
            properties: Some(serde_json::json!({})),
            tags: vec![],
            created_at: None,
            updated_at: None,
        }
    }

    fn sample_meta(tag: &str) -> MetaJson {
        MetaJson {
            fetched_at: "2026-01-01T00:00:00Z".to_string(),
            git_ref: "main".to_string(),
            commit_sha: "deadbeef".to_string(),
            content_hash: format!("sha256:{tag}"),
        }
    }

    /// Publish an "old" cache generation successfully, used as the baseline
    /// state that a subsequent failed publish must leave untouched.
    fn publish_old_generation(remotes_root: &Path, name: &str) -> PathBuf {
        let entities = vec![sample_entity(
            "11111111-1111-1111-1111-111111111111",
            "OldEntity",
        )];
        publish_remote_cache(
            remotes_root,
            name,
            &entities,
            &[],
            &sample_meta("old"),
            None,
        )
        .expect("baseline publish must succeed")
    }

    fn assert_cache_is_old_generation(cache_dir: &Path) {
        let entities = std::fs::read_to_string(cache_dir.join("entities.ndjson")).unwrap();
        assert!(
            entities.contains("OldEntity"),
            "cache entities must still be the old generation, got: {entities}"
        );
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(cache_dir.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["content_hash"], "sha256:old");
    }

    #[test]
    fn remote_cache_publish_failure_after_entities_keeps_old_cache() {
        let tmp = TempDir::new().unwrap();
        let remotes_root = tmp.path().join(".khive/kg/remotes");
        let cache_dir = publish_old_generation(&remotes_root, "upstream");

        let new_entities = vec![sample_entity(
            "22222222-2222-2222-2222-222222222222",
            "NewEntity",
        )];
        let err = publish_remote_cache(
            &remotes_root,
            "upstream",
            &new_entities,
            &[],
            &sample_meta("new"),
            Some(PublishFailAt::AfterEntities),
        )
        .expect_err("injected failure must surface as an error");
        assert!(err.to_string().contains("injected failure"));

        assert_cache_is_old_generation(&cache_dir);
    }

    #[test]
    fn remote_cache_publish_failure_after_edges_keeps_old_cache() {
        let tmp = TempDir::new().unwrap();
        let remotes_root = tmp.path().join(".khive/kg/remotes");
        let cache_dir = publish_old_generation(&remotes_root, "upstream");

        let new_entities = vec![sample_entity(
            "22222222-2222-2222-2222-222222222222",
            "NewEntity",
        )];
        let err = publish_remote_cache(
            &remotes_root,
            "upstream",
            &new_entities,
            &[],
            &sample_meta("new"),
            Some(PublishFailAt::AfterEdges),
        )
        .expect_err("injected failure must surface as an error");
        assert!(err.to_string().contains("injected failure"));

        assert_cache_is_old_generation(&cache_dir);
    }

    #[test]
    fn remote_cache_publish_failure_after_meta_keeps_old_cache() {
        let tmp = TempDir::new().unwrap();
        let remotes_root = tmp.path().join(".khive/kg/remotes");
        let cache_dir = publish_old_generation(&remotes_root, "upstream");

        let new_entities = vec![sample_entity(
            "22222222-2222-2222-2222-222222222222",
            "NewEntity",
        )];
        let err = publish_remote_cache(
            &remotes_root,
            "upstream",
            &new_entities,
            &[],
            &sample_meta("new"),
            Some(PublishFailAt::AfterMeta),
        )
        .expect_err("injected failure must surface as an error");
        assert!(err.to_string().contains("injected failure"));

        assert_cache_is_old_generation(&cache_dir);
    }

    /// Failure injected immediately before the directory swap: the staged
    /// directory is fully built but never made visible, so the reader-visible
    /// cache must still be exactly the old generation.
    #[test]
    fn remote_cache_publish_failure_before_swap_keeps_old_cache() {
        let tmp = TempDir::new().unwrap();
        let remotes_root = tmp.path().join(".khive/kg/remotes");
        let cache_dir = publish_old_generation(&remotes_root, "upstream");

        let new_entities = vec![sample_entity(
            "22222222-2222-2222-2222-222222222222",
            "NewEntity",
        )];
        let err = publish_remote_cache(
            &remotes_root,
            "upstream",
            &new_entities,
            &[],
            &sample_meta("new"),
            Some(PublishFailAt::BeforeSwap),
        )
        .expect_err("injected failure must surface as an error");
        assert!(err.to_string().contains("injected failure"));

        assert_cache_is_old_generation(&cache_dir);
    }

    /// A successful publish exposes the complete new entities+edges+meta
    /// together — never a partial mix with the previous generation.
    #[test]
    fn remote_cache_publish_success_exposes_complete_new_cache() {
        let tmp = TempDir::new().unwrap();
        let remotes_root = tmp.path().join(".khive/kg/remotes");
        let cache_dir = publish_old_generation(&remotes_root, "upstream");
        assert_cache_is_old_generation(&cache_dir);

        let new_entities = vec![sample_entity(
            "22222222-2222-2222-2222-222222222222",
            "NewEntity",
        )];
        let published = publish_remote_cache(
            &remotes_root,
            "upstream",
            &new_entities,
            &[],
            &sample_meta("new"),
            None,
        )
        .expect("publish must succeed");
        assert_eq!(published, cache_dir);

        let entities = std::fs::read_to_string(cache_dir.join("entities.ndjson")).unwrap();
        assert!(entities.contains("NewEntity"));
        assert!(
            !entities.contains("OldEntity"),
            "old generation entity must not linger after a successful publish"
        );
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(cache_dir.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["content_hash"], "sha256:new");
    }

    // ── F201 tests ────────────────────────────────────────────────────────────

    /// F201-1: `run_sync_remote` with a correct pin succeeds and writes the
    /// expected cache files and `meta.json`.
    #[tokio::test]
    async fn run_sync_remote_fetches_and_verifies_hash_match() {
        let remote_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();

        let id_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let entities = format!(
            r#"{{"id":"{id_a}","kind":"concept","name":"RemoteEntity","properties":{{}},"tags":[]}}"#
        );
        let edges = "";

        let remote_url = make_git_remote(remote_dir.path(), &entities, edges);
        let expected_pin = compute_pin(&entities, edges, "remote-ns");

        let remote = RemoteConfig {
            name: RemoteName::parse("upstream").unwrap(),
            url: remote_url,
            git_ref: "main".to_string(),
            namespace: "remote-ns".to_string(),
            pin: Some(expected_pin.clone()),
        };

        let report = run_sync_remote(repo_dir.path(), &remote, false)
            .await
            .expect("run_sync_remote must succeed with correct pin");

        assert_eq!(report.entities, 1, "must report 1 entity");
        assert_eq!(report.edges, 0, "must report 0 edges");
        assert_eq!(
            report.content_hash,
            expected_pin.as_str(),
            "content_hash must match the pin"
        );
        assert!(!report.repinned, "repin was not requested");

        // Cache files must exist.
        let cache = repo_dir.path().join(".khive/kg/remotes/upstream");
        assert!(
            cache.join("entities.ndjson").exists(),
            "entities.ndjson must exist in cache"
        );
        assert!(
            cache.join("edges.ndjson").exists(),
            "edges.ndjson must exist in cache"
        );
        assert!(
            cache.join("meta.json").exists(),
            "meta.json must exist in cache"
        );

        // meta.json must be valid JSON with the expected fields.
        let meta_bytes = std::fs::read(cache.join("meta.json")).unwrap();
        let meta: serde_json::Value = serde_json::from_slice(&meta_bytes).unwrap();
        assert_eq!(
            meta["content_hash"].as_str().unwrap(),
            expected_pin.as_str(),
            "meta.json content_hash must match"
        );
        assert!(
            meta["fetched_at"].as_str().is_some(),
            "meta.json must have fetched_at"
        );
        assert!(
            meta["commit_sha"].as_str().is_some(),
            "meta.json must have commit_sha"
        );
    }

    /// F201-2: `run_sync_remote` with a wrong pin fails before touching the
    /// cache (fail-closed guarantee).
    #[tokio::test]
    async fn run_sync_remote_rejects_hash_mismatch() {
        let remote_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();

        let id_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let entities = format!(
            r#"{{"id":"{id_b}","kind":"concept","name":"AnotherEntity","properties":{{}},"tags":[]}}"#
        );
        let edges = "";

        let remote_url = make_git_remote(remote_dir.path(), &entities, edges);

        // Deliberate wrong pin: 64 zero hex chars.
        let wrong_pin = SnapshotId::from_hash(&"0".repeat(64)).unwrap();

        let remote = RemoteConfig {
            name: RemoteName::parse("upstream").unwrap(),
            url: remote_url,
            git_ref: "main".to_string(),
            namespace: "remote-ns".to_string(),
            pin: Some(wrong_pin.clone()),
        };

        let err = run_sync_remote(repo_dir.path(), &remote, false)
            .await
            .expect_err("run_sync_remote must fail on hash mismatch");

        let err_msg = err.to_string();
        assert!(
            err_msg.contains("hash mismatch") || err_msg.contains("sha256:"),
            "error must mention hash mismatch, got: {err_msg}"
        );

        // Cache must NOT have been written (fail-closed).
        let cache = repo_dir.path().join(".khive/kg/remotes/upstream");
        assert!(
            !cache.join("entities.ndjson").exists(),
            "entities.ndjson must NOT exist after mismatch"
        );
        assert!(
            !cache.join("meta.json").exists(),
            "meta.json must NOT exist after mismatch"
        );
    }

    /// F201-3: `run_sync_remote` with no pin still proceeds and writes `meta.json`
    /// (hash is still computed and written for auditability).
    #[tokio::test]
    async fn run_sync_remote_no_pin_proceeds_and_writes_meta() {
        let remote_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();

        let id_c = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let entities = format!(
            r#"{{"id":"{id_c}","kind":"concept","name":"Pinless","properties":{{}},"tags":[]}}"#
        );

        let remote_url = make_git_remote(remote_dir.path(), &entities, "");

        let remote = RemoteConfig {
            name: RemoteName::parse("no-pin-remote").unwrap(),
            url: remote_url,
            git_ref: "main".to_string(),
            namespace: "remote-ns".to_string(),
            pin: None,
        };

        let report = run_sync_remote(repo_dir.path(), &remote, false)
            .await
            .expect("run_sync_remote must succeed with no pin");

        assert_eq!(report.entities, 1);
        assert!(
            report.content_hash.starts_with("sha256:"),
            "content_hash must have sha256: prefix even without pin"
        );

        let cache = repo_dir.path().join(".khive/kg/remotes/no-pin-remote");
        assert!(
            cache.join("meta.json").exists(),
            "meta.json must be written even when pin is absent"
        );
    }

    /// F201-4: `--repin` skips pin comparison and returns the actual hash,
    /// allowing the caller to update `schema.yaml`.
    #[tokio::test]
    async fn run_sync_remote_repin_updates_hash_ignoring_old_pin() {
        let remote_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();

        let id_d = "dddddddd-dddd-dddd-dddd-dddddddddddd";
        let entities = format!(
            r#"{{"id":"{id_d}","kind":"concept","name":"RepinTarget","properties":{{}},"tags":[]}}"#
        );

        let remote_url = make_git_remote(remote_dir.path(), &entities, "");
        let actual_hash = compute_pin(&entities, "", "repin-ns");

        // Deliberately stale/wrong pin — repin must ignore it.
        let stale_pin = SnapshotId::from_hash(&"f".repeat(64)).unwrap();

        let remote = RemoteConfig {
            name: RemoteName::parse("repinned").unwrap(),
            url: remote_url,
            git_ref: "main".to_string(),
            namespace: "repin-ns".to_string(),
            pin: Some(stale_pin),
        };

        let report = run_sync_remote(repo_dir.path(), &remote, true)
            .await
            .expect("repin must succeed even with wrong existing pin");

        assert!(report.repinned, "repinned flag must be true");
        assert_eq!(
            report.content_hash,
            actual_hash.as_str(),
            "repinned hash must be the actual fetched archive hash"
        );

        // Cache must be populated.
        let cache = repo_dir.path().join(".khive/kg/remotes/repinned");
        assert!(cache.join("entities.ndjson").exists());
        assert!(cache.join("meta.json").exists());
    }

    // ── URL redaction tests ───────────────────────────────────────────────────

    /// Credentials embedded in a URL must not survive redact_git_stderr.
    #[test]
    fn redact_strips_credential_url() {
        let raw = "fatal: Authentication failed for 'https://user:token@host/repo.git'";
        let out = redact_git_stderr(raw);
        assert!(
            !out.contains("user:token"),
            "credential must be redacted, got: {out}"
        );
        assert!(
            !out.contains("host/repo.git"),
            "host/path must be redacted, got: {out}"
        );
        assert!(
            out.contains("<url-redacted>"),
            "placeholder must be present, got: {out}"
        );
    }

    /// Plain text without a URL must pass through unchanged.
    #[test]
    fn redact_passes_plain_text() {
        let raw = "error: unable to read refs from remote";
        assert_eq!(redact_git_stderr(raw), raw);
    }

    /// Multiple URLs in the same stderr string must all be redacted.
    #[test]
    fn redact_handles_multiple_urls() {
        let raw = "fetch https://a:b@host1/r1.git and push https://c:d@host2/r2.git failed";
        let out = redact_git_stderr(raw);
        assert!(!out.contains("a:b"), "first credential must be redacted");
        assert!(!out.contains("c:d"), "second credential must be redacted");
        assert_eq!(
            out.matches("<url-redacted>").count(),
            2,
            "both URLs must be replaced"
        );
    }

    /// A bare URL without credentials is also redacted (the host is still sensitive).
    #[test]
    fn redact_handles_url_without_credentials() {
        let raw = "fatal: repository 'https://github.com/org/private-repo.git/' not found";
        let out = redact_git_stderr(raw);
        assert!(
            !out.contains("github.com/org/private-repo"),
            "URL path must be redacted"
        );
        assert!(
            out.contains("<url-redacted>"),
            "placeholder must be present"
        );
    }

    /// scp-style `git@host:org/repo.git` must be fully redacted.
    #[test]
    fn redact_strips_scp_style_remote() {
        let raw = "ERROR: Repository not found.\nfatal: Could not read from remote repository git@github.com:org/private-repo.git";
        let out = redact_git_stderr(raw);
        assert!(
            !out.contains("git@"),
            "scp userinfo must be redacted, got: {out}"
        );
        assert!(
            !out.contains("github.com"),
            "scp host must be redacted, got: {out}"
        );
        assert!(
            !out.contains("private-repo"),
            "scp path must be redacted, got: {out}"
        );
        assert!(
            out.contains("<url-redacted>"),
            "placeholder must be present, got: {out}"
        );
    }

    /// `user@host:path` (non-git@ prefix) must also be redacted.
    #[test]
    fn redact_strips_user_at_host_colon_path() {
        let raw = "fatal: repository user@bitbucket.org:myteam/myrepo.git not found";
        let out = redact_git_stderr(raw);
        assert!(
            !out.contains("user@"),
            "userinfo must be redacted, got: {out}"
        );
        assert!(
            !out.contains("bitbucket.org"),
            "host must be redacted, got: {out}"
        );
        assert!(
            out.contains("<url-redacted>"),
            "placeholder must be present, got: {out}"
        );
    }

    /// Plain `host:path` without a `user@` prefix must NOT be over-redacted
    /// (it is not a recognised remote form).
    #[test]
    fn redact_does_not_over_redact_plain_colon() {
        let raw = "error: src refspec main does not match any";
        let out = redact_git_stderr(raw);
        assert_eq!(out, raw, "plain text with colon must not be altered");
    }

    // ── Public error boundary tests ───────────────────────────────────────────
    //
    // These tests verify that the sanitiser is wired into the actual public error
    // path (the `anyhow` error returned by `run_sync_remote`).  They use realistic
    // git-stderr fragments — the kind git emits when a clone fails for auth or
    // network reasons — and assert that the rendered error string contains no raw
    // credentials or remote-URL tokens.
    //
    // The FAIL-before / PASS-after property is demonstrated by the
    // `redact_git_stderr` unit tests above (which call the function directly)
    // combined with these wiring tests that confirm the sanitised output is what
    // the caller actually sees in `err.to_string()`.

    /// HTTPS URL with embedded `user:pass` credentials must not appear in the
    /// public error string produced by a clone failure.
    ///
    /// git echoes credential-bearing HTTPS URLs in its stderr on auth failure,
    /// e.g.: `fatal: Authentication failed for 'https://user:token@host/repo.git'`
    /// The sanitiser must strip that before it reaches the caller.
    #[tokio::test]
    async fn public_error_redacts_https_credential_url() {
        let repo_dir = tempfile::TempDir::new().unwrap();
        // Use a credential-bearing HTTPS URL that will fail immediately.
        let remote = RemoteConfig {
            name: RemoteName::parse("cred-test").unwrap(),
            url: "https://user:secret_token@nonexistent.example.invalid/org/repo.git".to_string(),
            git_ref: "main".to_string(),
            namespace: "test-ns".to_string(),
            pin: None,
        };
        let err = run_sync_remote(repo_dir.path(), &remote, false)
            .await
            .expect_err("clone of nonexistent URL must fail");

        let err_str = err.to_string();
        let err_chain: String = err
            .chain()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" | ");

        assert!(
            !err_str.contains("secret_token") && !err_chain.contains("secret_token"),
            "credential must not appear in public error string or chain: {err_str} | {err_chain}"
        );
        assert!(
            !err_str.contains("user:") && !err_chain.contains("user:"),
            "userinfo must not appear in public error: {err_str} | {err_chain}"
        );
        // The remote name is used in the error, but the URL must not be.
        assert!(
            err_str.contains("cred-test") || err_chain.contains("cred-test"),
            "remote name must be present for diagnostics: {err_str} | {err_chain}"
        );
    }

    /// scp-style `git@host:org/repo.git` must not leak through the sanitiser
    /// into the public error string.
    ///
    /// This test exercises the FAIL-before/PASS-after property of the scp fix:
    /// the sanitiser is called on the raw git stderr, which git may populate with
    /// lines like `fatal: Could not read from remote repository.` that do NOT
    /// contain the URL — so for scp remotes that fail at DNS/SSH level the URL
    /// is not re-echoed by git.  What we assert here is that the sanitiser IS
    /// wired into the error path and that the rendered error does not include the
    /// scp token from any source.
    ///
    /// The companion unit tests `redact_strips_scp_style_remote` and
    /// `redact_strips_user_at_host_colon_path` directly verify the sanitiser
    /// strips scp tokens from git stderr strings; this test confirms the wiring.
    #[tokio::test]
    async fn public_error_redacts_scp_style_remote() {
        let repo_dir = tempfile::TempDir::new().unwrap();
        let remote = RemoteConfig {
            name: RemoteName::parse("scp-test").unwrap(),
            url: "git@nonexistent.example.invalid:org/private-repo.git".to_string(),
            git_ref: "main".to_string(),
            namespace: "test-ns".to_string(),
            pin: None,
        };
        let err = run_sync_remote(repo_dir.path(), &remote, false)
            .await
            .expect_err("clone of nonexistent scp remote must fail");

        let err_str = err.to_string();
        let err_chain: String = err
            .chain()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" | ");

        // The remote URL must not appear verbatim in the public error.
        // git does not echo scp URLs into stderr on SSH-level failures —
        // the host appears in SSH's own message, not from URL echoing.
        // We assert that our scp token (user@host:path form) is absent.
        assert!(
            !err_str.contains("git@nonexistent.example.invalid")
                && !err_chain.contains("git@nonexistent.example.invalid"),
            "scp remote URL must not appear in public error: {err_str} | {err_chain}"
        );
        assert!(
            !err_str.contains("private-repo") && !err_chain.contains("private-repo"),
            "scp repo path must not appear in public error: {err_str} | {err_chain}"
        );
        // Remote name must still be present for diagnostics.
        assert!(
            err_str.contains("scp-test") || err_chain.contains("scp-test"),
            "remote name must be present for diagnostics: {err_str} | {err_chain}"
        );
    }

    /// Regression: VCS sync FTS document must be field-identical to
    /// `entity_fts_document` output for the same entity.  Before this fix,
    /// `upsert_entities` built a `TextDocument` inline with slightly different
    /// field mapping than the canonical helper, which could produce divergent
    /// FTS shapes when the helper is updated.
    #[test]
    fn sync_fts_document_matches_entity_fts_document() {
        use khive_runtime::entity_fts_document;
        use khive_storage::SubstrateKind;

        let id = uuid::Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let props: Option<serde_json::Value> =
            Some(serde_json::json!({"domain": "attention", "status": "researched"}));

        let entity = khive_storage::entity::Entity {
            id,
            namespace: "test-ns".to_string(),
            kind: "concept".to_string(),
            entity_type: None,
            name: "FlashAttention".to_string(),
            description: Some("Fast attention algorithm".to_string()),
            properties: props.clone(),
            tags: vec!["attention".to_string(), "inference".to_string()],
            created_at: 1_000_000,
            updated_at: 2_000_000,
            deleted_at: None,
            merge_event_id: None,
            merged_into: None,
            content_ref: None,
        };

        let doc = entity_fts_document(&entity);

        assert_eq!(doc.subject_id, id);
        assert_eq!(doc.kind, SubstrateKind::Entity);
        assert_eq!(doc.namespace, "test-ns");
        assert_eq!(doc.title.as_deref(), Some("FlashAttention"));
        assert_eq!(doc.body, "FlashAttention Fast attention algorithm");
        assert_eq!(
            doc.tags,
            vec!["attention".to_string(), "inference".to_string()]
        );
        assert_eq!(doc.metadata, props);
        assert_eq!(
            doc.updated_at,
            chrono::DateTime::from_timestamp_micros(2_000_000).unwrap()
        );
    }

    /// `user@host:path` scp form must not appear in the public error string.
    #[tokio::test]
    async fn public_error_redacts_user_pass_at_host_scp() {
        let repo_dir = tempfile::TempDir::new().unwrap();
        let remote = RemoteConfig {
            name: RemoteName::parse("userpass-scp").unwrap(),
            url: "deploy@nonexistent.example.invalid:infra/secret-repo.git".to_string(),
            git_ref: "main".to_string(),
            namespace: "test-ns".to_string(),
            pin: None,
        };
        let err = run_sync_remote(repo_dir.path(), &remote, false)
            .await
            .expect_err("clone of nonexistent scp remote must fail");

        let err_str = err.to_string();
        let err_chain: String = err
            .chain()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" | ");

        assert!(
            !err_str.contains("deploy@nonexistent.example.invalid")
                && !err_chain.contains("deploy@nonexistent.example.invalid"),
            "scp userinfo+host must not appear in public error: {err_str} | {err_chain}"
        );
        assert!(
            !err_str.contains("secret-repo") && !err_chain.contains("secret-repo"),
            "scp repo path must not appear in public error: {err_str} | {err_chain}"
        );
    }
}

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
// `execute_script_top_level("PRAGMA wal_checkpoint(TRUNCATE);")` call that
// `checkpoint_wal` makes, over a `PoolConfig { write_queue_enabled: true, .. }`
// literal (no env var mutation, no cross-test race).
#[cfg(test)]
mod checkpoint_wal_write_queue_tests {
    #[tokio::test]
    async fn wal_checkpoint_truncate_succeeds_with_write_queue_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("vcs-checkpoint-write-queue.db");
        let pool_cfg = khive_db::PoolConfig {
            path: Some(db_path),
            write_queue_enabled: true,
            ..khive_db::PoolConfig::default()
        };
        let pool = std::sync::Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
        {
            let mut writer = pool.writer().expect("writer");
            khive_db::run_migrations(writer.conn_mut()).expect("migrations");
        }
        assert!(
            pool.writer_task_handle().unwrap().is_some(),
            "writer task must be spawned with the flag on for a file-backed pool"
        );

        let sql: std::sync::Arc<dyn khive_storage::SqlAccess> =
            std::sync::Arc::new(khive_db::SqlBridge::new(std::sync::Arc::clone(&pool), true));

        let mut writer = sql.writer().await.expect("writer handle");
        let result = writer
            .execute_script_top_level("PRAGMA wal_checkpoint(TRUNCATE);".to_string())
            .await;

        assert!(
            result.is_ok(),
            "PRAGMA wal_checkpoint(TRUNCATE) via execute_script_top_level must succeed under \
             KHIVE_WRITE_QUEUE (no BEGIN IMMEDIATE wrap); got {result:?}"
        );
    }

    /// Revert-and-confirm-fails companion: the OLD (broken) call shape —
    /// plain `execute_script`, which wraps the pragma in the WriterTask's
    /// `BEGIN IMMEDIATE` — must fail under the write queue. This proves the
    /// test above is actually exercising the regression, not passing
    /// vacuously.
    #[tokio::test]
    async fn wal_checkpoint_truncate_via_plain_execute_script_fails_with_write_queue_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("vcs-checkpoint-write-queue-regression.db");
        let pool_cfg = khive_db::PoolConfig {
            path: Some(db_path),
            write_queue_enabled: true,
            ..khive_db::PoolConfig::default()
        };
        let pool = std::sync::Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
        {
            let mut writer = pool.writer().expect("writer");
            khive_db::run_migrations(writer.conn_mut()).expect("migrations");
        }

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
