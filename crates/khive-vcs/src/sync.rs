//! NDJSON-to-SQLite sync library boundary.
//!
//! Rebuilds the SQLite database from `.khive/kg/entities.ndjson` and `edges.ndjson`.
//! Builds atomically into a `.tmp` file then renames. Also supports remote archive
//! fetch with SHA-256 pin verification via [`run_sync_remote`].

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_storage::types::{Edge, TextDocument};
use khive_storage::{LinkId, SubstrateKind};
use khive_types::EdgeRelation;
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

/// Configuration for a remote KG archive (maps to one entry in `schema.yaml`
/// `remotes:` list).
#[derive(Debug, Clone)]
pub struct RemoteConfig {
    /// Human-readable name for this remote (used in error messages and cache
    /// directory paths).
    pub name: String,
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
            return Err(anyhow!(
                "git clone failed for remote {:?}: {}",
                remote.name,
                stderr.trim()
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

    // ── 5. Atomically publish to cache ────────────────────────────────────────
    let cache_dir = repo_root.join(".khive/kg/remotes").join(&remote.name);
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    // Write files into staging first, then rename into place atomically.
    let tmp_entities = cache_dir.with_extension("entities.tmp");
    let tmp_edges = cache_dir.with_extension("edges.tmp");

    write_sorted_entities(&tmp_entities, &entities_ndjson)
        .context("writing staged entities.ndjson")?;
    write_sorted_edges(&tmp_edges, &edges_ndjson).context("writing staged edges.ndjson")?;

    std::fs::rename(&tmp_entities, cache_dir.join("entities.ndjson"))
        .context("renaming entities.ndjson into cache")?;
    std::fs::rename(&tmp_edges, cache_dir.join("edges.ndjson"))
        .context("renaming edges.ndjson into cache")?;

    // ── 6. Write meta.json ────────────────────────────────────────────────────
    let meta = MetaJson {
        fetched_at: Utc::now().to_rfc3339(),
        git_ref: remote.git_ref.clone(),
        commit_sha,
        content_hash: actual_hash.as_str().to_string(),
    };
    let meta_path = cache_dir.join("meta.json");
    let meta_json = serde_json::to_string_pretty(&meta).context("serializing meta.json")?;
    std::fs::write(&meta_path, meta_json.as_bytes()).context("writing meta.json")?;

    // staging tempdir is dropped here, cleaning up the clone.
    drop(staging);

    Ok(RemoteSyncReport {
        entities: entities_ndjson.len(),
        edges: edges_ndjson.len(),
        cache_dir: cache_dir.to_string_lossy().into_owned(),
        meta_path: meta_path.to_string_lossy().into_owned(),
        content_hash: actual_hash.as_str().to_string(),
        repinned: repin,
    })
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
        return Err(anyhow!("git {} failed: {}", args.join(" "), stderr.trim()));
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
            entity_type: None,
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

    // ── Validate-first gate ──────────────────────────────────────────────────────
    // Parse every edge relation before creating the temp DB so that an invalid
    // relation causes a clean error that leaves the existing DB intact.
    for (i, r) in edge_records.iter().enumerate() {
        r.relation.parse::<EdgeRelation>().with_context(|| {
            format!(
                "invalid edge relation {:?} at record {} — sync aborted before any DB write",
                r.relation,
                i + 1
            )
        })?;
    }

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
    writer
        .execute_script("PRAGMA wal_checkpoint(TRUNCATE);".to_string())
        .await?;
    Ok(())
}

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
    let mut count = 0;
    for r in records {
        let created_at = parse_ts_micros(r.created_at.as_deref());
        let updated_at = parse_ts_micros(r.updated_at.as_deref());
        // Build the FTS body from name + description (same as create_entity in operations.rs).
        let body = match &r.description {
            Some(d) if !d.is_empty() => format!("{} {}", r.name, d),
            _ => r.name.clone(),
        };
        let entity = khive_storage::entity::Entity {
            id: r.id,
            namespace: namespace.to_string(),
            kind: r.kind.clone(),
            entity_type: None,
            name: r.name.clone(),
            description: r.description.clone(),
            properties: r.properties.clone(),
            tags: r.tags.clone(),
            created_at,
            updated_at,
            deleted_at: None,
            merge_event_id: None,
            merged_into: None,
        };
        store
            .upsert_entity(entity)
            .await
            .with_context(|| format!("upsert entity {}", r.id))?;
        // Populate FTS5 index so text search works after sync.
        // Vectors are intentionally skipped: they are local-only derived state
        // and will be computed by `kkernel kg embed` when needed.
        text.upsert_document(TextDocument {
            subject_id: r.id,
            kind: SubstrateKind::Entity,
            title: Some(r.name.clone()),
            body,
            tags: r.tags.clone(),
            namespace: namespace.to_string(),
            metadata: r.properties.clone(),
            updated_at: chrono::DateTime::from_timestamp_micros(updated_at)
                .unwrap_or_else(chrono::Utc::now),
        })
        .await
        .with_context(|| format!("fts index entity {}", r.id))?;
        count += 1;
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
    let mut count = 0;
    for r in records {
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
        graph
            .upsert_edge(edge)
            .await
            .with_context(|| format!("upsert edge {}", r.edge_id))?;
        count += 1;
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
        let status = Command::new("git")
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
            name: "upstream".to_string(),
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
            name: "upstream".to_string(),
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
            name: "no-pin-remote".to_string(),
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
            name: "repinned".to_string(),
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
}
