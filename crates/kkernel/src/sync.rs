//! `kkernel sync` — build a SQLite working DB from NDJSON sources.
//!
//! Reads `<repo>/.khive/kg/entities.ndjson` and `<repo>/.khive/kg/edges.ndjson`,
//! parses each record per ADR-048 §2 canonical schema, and writes them into
//! a fresh SQLite database using the runtime's upsert APIs. The resulting DB
//! has the full khive schema (entities + graph_edges + FTS5 indexes + vector
//! tables) — same as the MCP server uses.
//!
//! This is the Rust half of issue #174. The Deno CLI's `khive kg sync` shells
//! out here so the working DB is a real SQLite file, not a misleading JSON
//! marker pretending to be SQLite.
//!
//! ## Atomicity
//!
//! Builds into `<target>.tmp` then renames over `<target>`. A crash mid-build
//! leaves the previous DB intact.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_storage::entity::Entity as StorageEntity;
use khive_storage::types::Edge;
use khive_storage::LinkId;
use khive_types::EdgeRelation;
use serde::Deserialize;
use uuid::Uuid;

/// Per-record entity shape produced by the Deno exporter (ADR-048 §2).
#[derive(Debug, Deserialize)]
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

/// Per-record edge shape produced by the Deno exporter (ADR-048 §2).
#[derive(Debug, Deserialize)]
struct NdjsonEdge {
    edge_id: Uuid,
    source: Uuid,
    target: Uuid,
    relation: String,
    #[serde(default = "default_weight")]
    weight: f64,
    // properties: not yet persisted to the storage-layer Edge struct.
    // Accepted but ignored so existing NDJSON files parse without warning.
    #[serde(default)]
    #[allow(dead_code)]
    properties: Option<serde_json::Value>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    updated_at: Option<String>,
}

fn default_weight() -> f64 {
    1.0
}

/// Parse an ISO-8601 timestamp string into microseconds since epoch.
/// Returns `now` if the string is None or unparseable.
fn parse_ts_micros(s: Option<&str>) -> i64 {
    s.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_micros())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_micros())
}

/// Summary of a sync run.
#[derive(Debug, serde::Serialize)]
pub struct SyncReport {
    pub entities: usize,
    pub edges: usize,
    pub db_path: String,
}

/// Run the sync: NDJSON -> SQLite via the runtime's upsert APIs.
///
/// `repo_root` is the directory containing `.khive/kg/{entities,edges}.ndjson`.
/// `db_path` is the target SQLite file (atomically replaced via tmp+rename).
/// `namespace` is the namespace for all imported records.
///
/// Returns a `SyncReport` describing the build, or an error if NDJSON parsing
/// or the SQLite upserts failed. On error, the tmp file is left behind for
/// post-mortem; the original `db_path` is untouched.
pub async fn run_sync(repo_root: &Path, db_path: &Path, namespace: &str) -> Result<SyncReport> {
    let entities_path = repo_root.join(".khive/kg/entities.ndjson");
    let edges_path = repo_root.join(".khive/kg/edges.ndjson");

    let entity_records = read_entities(&entities_path)
        .with_context(|| format!("reading {}", entities_path.display()))?;
    let edge_records =
        read_edges(&edges_path).with_context(|| format!("reading {}", edges_path.display()))?;

    let tmp_path = with_extension_suffix(db_path, ".tmp");
    let _ = std::fs::remove_file(&tmp_path);

    // Build the runtime against the tmp file. Vector embedding is disabled
    // because sync runs without an embedding model loaded — vectors are
    // computed lazily on access via the MCP server if needed.
    let config = RuntimeConfig {
        db_path: Some(tmp_path.clone()),
        default_namespace: namespace.to_string(),
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
    // through the last auto-checkpoint (every 4000 pages — see khive-db
    // pool::WAL_AUTOCHECKPOINT_PAGES). For small graphs no auto-checkpoint
    // fires, so the test data would silently disappear.
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
    let store = runtime
        .entities(Some(namespace))
        .context("opening entity store")?;
    let mut count = 0;
    for r in records {
        let created_at = parse_ts_micros(r.created_at.as_deref());
        let updated_at = parse_ts_micros(r.updated_at.as_deref());
        let entity = StorageEntity {
            id: r.id,
            namespace: namespace.to_string(),
            kind: r.kind,
            name: r.name,
            description: r.description,
            properties: r.properties,
            tags: r.tags,
            created_at,
            updated_at,
            deleted_at: None,
        };
        store
            .upsert_entity(entity)
            .await
            .with_context(|| format!("upsert entity {}", r.id))?;
        count += 1;
    }
    Ok(count)
}

async fn upsert_edges(
    runtime: &KhiveRuntime,
    namespace: &str,
    records: Vec<NdjsonEdge>,
) -> Result<usize> {
    let graph = runtime
        .graph(Some(namespace))
        .context("opening graph store")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

        // Verify the file exists, is non-empty, and starts with the SQLite
        // magic header — this is the contract that #174 fixed.
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

        // Re-open the DB via the runtime and verify the records persisted.
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: "test-ns".into(),
            embedding_model: None,
            ..RuntimeConfig::default()
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let alpha = rt
            .entities(Some("test-ns"))
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
        // Pre-create a sentinel DB at db_path. After a failed sync the
        // sentinel should remain (or after a successful one, be replaced).
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        std::fs::write(&db_path, b"SENTINEL").unwrap();

        // Write malformed entities ndjson — sync should fail.
        write_repo(repo, "not json\n", "");
        let err = run_sync(repo, &db_path, "test-ns").await.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("parsing entity")
                || err.chain().any(|e| e.to_string().contains("expected")),
            "expected parse error, got: {err}"
        );

        // Sentinel still present — sync did not clobber it.
        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(
            after, b"SENTINEL",
            "atomic guarantee: failed sync must not replace existing DB"
        );
    }

    #[tokio::test]
    async fn sync_missing_ndjson_files_succeeds_with_zero_counts() {
        // Issue an honest sync against an empty repo (no .khive/kg/ at all).
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let db_path = repo.join(".khive/state/working.db");

        let report = run_sync(repo, &db_path, "test-ns").await.unwrap();
        assert_eq!(report.entities, 0);
        assert_eq!(report.edges, 0);
    }
}
