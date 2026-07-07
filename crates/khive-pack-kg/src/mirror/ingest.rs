//! Idempotent per-file mirror: file content → `document` entity (ADR-087).
//!
//! Where `khive-pack-session/src/mirror/ingest.rs` tails a JSONL file
//! incrementally and writes into pack-private tables, this module reads one
//! whole file, masks secrets, and writes a real `document` entity through
//! the same internal path (`KhiveRuntime::create_entity`) an agent's
//! `create` call would use — the storage-target divergence ADR-087
//! documents explicitly (see its "critical divergence" section). Content is
//! re-read and re-checksummed whole on every pass; `.khive/` files under the
//! mirror's default scope are markdown/text and small by construction, so
//! the unbounded-read concern the session mirror's per-pass byte cap
//! addresses does not recur here (ADR-087 Decision item 1).

use std::path::Path;

use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use khive_runtime::{secret_gate, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::EdgeRelation;

/// Soft ceiling on inline `description` size (ADR-086's "prefer inline
/// under ~200KB" guidance — a soft operational guideline, not a hard
/// technical limit). Content over this is truncated at a UTF-8-safe
/// boundary, with a note pointing back at `properties.source_uri` for the
/// full text.
const INLINE_CONTENT_SOFT_LIMIT: usize = 200_000;

/// Outcome of one [`mirror_file`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorOutcome {
    /// A new `document` entity version was written (first sight of this
    /// path, or its content changed since the last synced version).
    Created {
        entity_id: Uuid,
        /// The prior version's entity id, when this write superseded one.
        superseded: Option<Uuid>,
    },
    /// The file's mtime advanced but its content hash did not — the cursor
    /// row is refreshed but no new entity or edge is written.
    Unchanged,
}

/// Mirror one file at `path` (absolute) into a `document` entity.
///
/// `khive_dir` is the `.khive/` directory `path` lives under — used to
/// compute the stable relative identity used as the entity's `name` and the
/// path-to-`entity_type` mapping. Never advances the persisted cursor on
/// any error (IO, hashing, or the entity/edge write itself) — a failed pass
/// leaves the file to be retried on the next poll, matching the session
/// mirror's failure posture (ADR-087 Decision item 1).
///
/// A changed file (hash differs from the cursor's `last_hash`) produces a
/// NEW `document` entity plus a `supersedes` edge to the prior version
/// (matched by `properties.source_uri`) — never an in-place content
/// overwrite (the data-vs-view principle; ADR-086 Decision item 4).
pub async fn mirror_file(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    khive_dir: &Path,
    path: &Path,
) -> Result<MirrorOutcome, RuntimeError> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| RuntimeError::Internal(format!("workspace mirror: stat {path:?}: {e}")))?;
    let mtime = mtime_micros(&metadata);

    let rel_path = path
        .strip_prefix(khive_dir)
        .map_err(|_| {
            RuntimeError::Internal(format!(
                "workspace mirror: {path:?} is not under {khive_dir:?}"
            ))
        })?
        .to_string_lossy()
        .replace('\\', "/");

    let raw_content = std::fs::read_to_string(path)
        .map_err(|e| RuntimeError::Internal(format!("workspace mirror: read {path:?}: {e}")))?;

    // Checksum of the raw file content: used both as the cursor's
    // change-detection key and as `properties.checksum` (an identity/
    // integrity marker for the source read, sha256(content)[:16] per the
    // fleet's standing content-hash-not-labels convention).
    let checksum = content_checksum(&raw_content);

    if let Some(cursor) = load_cursor(runtime, path).await? {
        if cursor.last_hash == checksum {
            // Content unchanged since the last synced version — refresh
            // only the mtime/synced-at bookkeeping; no new entity or edge.
            if cursor.last_mtime != mtime {
                upsert_cursor(runtime, path, mtime, &checksum).await?;
            }
            return Ok(MirrorOutcome::Unchanged);
        }
    }

    // Unconditional secret masking — never `check()` (ADR-087 Decision item
    // 1): this is passive ingestion of pre-existing content on disk, which
    // cannot reject a whole file over one matched line the way a rejectable
    // agent-authored write can.
    let masked = secret_gate::mask_secrets(&raw_content);
    let description = truncate_for_inline(&masked);

    let source_uri = path.to_string_lossy().into_owned();
    let source_type = source_type_for_path(path);
    let entity_type = entity_type_for_path(&rel_path);

    let properties = serde_json::json!({
        "source_uri": source_uri,
        "source_type": source_type,
        "checksum": checksum,
    });

    let prior = find_prior_version(runtime, token, &source_uri).await?;

    let entity = runtime
        .create_entity(
            token,
            "document",
            Some(entity_type),
            &rel_path,
            Some(&description),
            Some(properties),
            vec!["workspace-mirror".to_string()],
        )
        .await?;

    if let Some(prior_id) = prior {
        // Version history via `supersedes`, never in-place mutation (the
        // data-vs-view principle; ADR-086 Decision item 4). A failure here
        // must NOT advance the cursor: the next poll re-attempts the whole
        // pass (a fresh entity version plus the edge) rather than leaving an
        // un-superseded orphan silently treated as done.
        runtime
            .link(
                token,
                entity.id,
                prior_id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await?;
    }

    // Cursor advances ONLY after the entity (and, when applicable, the
    // supersedes edge) has been durably written — never on error, matching
    // the session mirror's failure posture (ADR-087 Decision item 1).
    upsert_cursor(runtime, path, mtime, &checksum).await?;

    Ok(MirrorOutcome::Created {
        entity_id: entity.id,
        superseded: prior,
    })
}

/// Map a `.khive/`-relative path to an ADR-086 `document` `entity_type`
/// token. `entity_type` validation is advisory-only in v1 (ADR-086 §3, and
/// the registry's `document` subtype set is being extended in a parallel
/// lane) — `KhiveRuntime::create_entity` does not enforce it, so this value
/// is written as-is and composes once both land.
fn entity_type_for_path(rel_path: &str) -> &'static str {
    if rel_path.starts_with("notes/handoffs/") {
        "handoff"
    } else if rel_path.starts_with("notes/summaries/") {
        "summary"
    } else if rel_path.starts_with("notes/") {
        "note"
    } else if rel_path.starts_with("reports/")
        || rel_path.starts_with("codex_reviews/")
        || rel_path.contains("/artifacts/")
    {
        "report"
    } else {
        "other"
    }
}

/// A MIME-ish `source_type` string (ADR-086 Decision item 2) inferred from
/// the file extension.
fn source_type_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("md") | Some("markdown") => "text/markdown",
        _ => "text/plain",
    }
}

/// sha256(content)[:16] as a hex string — the fleet's standing
/// dedup-by-content-hash convention (matches
/// `khive-pack-knowledge`'s `content_hash`).
fn content_checksum(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let hash = hasher.finalize();
    format!("{hash:x}")[..16].to_string()
}

/// Truncate `masked` at a UTF-8-safe boundary once it exceeds
/// [`INLINE_CONTENT_SOFT_LIMIT`], appending a pointer back at the source.
fn truncate_for_inline(masked: &str) -> String {
    if masked.len() <= INLINE_CONTENT_SOFT_LIMIT {
        return masked.to_string();
    }
    let mut end = INLINE_CONTENT_SOFT_LIMIT;
    while !masked.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[... truncated at {INLINE_CONTENT_SOFT_LIMIT} bytes; full content at properties.source_uri ...]",
        &masked[..end]
    )
}

/// Microsecond Unix timestamp of a file's mtime, or `0` when unavailable
/// (e.g. a platform without mtime support) — matches this crate's
/// convention elsewhere of treating an unreadable timestamp as "unknown"
/// rather than failing the whole pass.
fn mtime_micros(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Find the most recent non-deleted `document` entity whose
/// `properties.source_uri` equals `source_uri` — the stable identity key
/// across versions (ADR-087 Decision item 3).
async fn find_prior_version(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source_uri: &str,
) -> Result<Option<Uuid>, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.map_err(|e| {
        RuntimeError::Internal(format!("workspace mirror: prior-version reader: {e}"))
    })?;
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT id FROM entities \
                  WHERE namespace = ?1 AND kind = 'document' AND deleted_at IS NULL \
                    AND json_extract(properties, '$.source_uri') = ?2 \
                  ORDER BY created_at DESC LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(source_uri.to_string()),
            ],
            label: Some("workspace_mirror_find_prior_version".into()),
        })
        .await
        .map_err(|e| {
            RuntimeError::Internal(format!("workspace mirror: prior-version query: {e}"))
        })?;

    let Some(row) = row else {
        return Ok(None);
    };
    match row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).map(Some).map_err(|e| {
            RuntimeError::Internal(format!("workspace mirror: malformed entity id {s:?}: {e}"))
        }),
        _ => Ok(None),
    }
}

/// A loaded cursor row for one file path.
struct CursorRow {
    last_mtime: i64,
    last_hash: String,
}

/// Load the persisted cursor row for `path` from `workspace_mirror_cursor`.
///
/// A missing table (schema not yet applied) or missing row both return
/// `None` rather than an error — the mirror self-bootstraps on the first
/// successful write, exactly as the session mirror's `load_cursors` does.
async fn load_cursor(
    runtime: &KhiveRuntime,
    path: &Path,
) -> Result<Option<CursorRow>, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| RuntimeError::Internal(format!("workspace mirror: cursor reader: {e}")))?;
    let path_str = path.to_string_lossy().into_owned();

    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT last_mtime, last_hash FROM workspace_mirror_cursor WHERE path = ?1".into(),
            params: vec![SqlValue::Text(path_str)],
            label: Some("workspace_mirror_load_cursor".into()),
        })
        .await;

    let row = match row {
        Ok(row) => row,
        Err(e) => {
            tracing::debug!(error = %e, "workspace mirror: cursor table not yet available");
            return Ok(None);
        }
    };
    let Some(row) = row else {
        return Ok(None);
    };

    let last_mtime = match row.get("last_mtime") {
        Some(SqlValue::Integer(n)) => *n,
        _ => return Ok(None),
    };
    let last_hash = match row.get("last_hash") {
        Some(SqlValue::Text(s)) => s.clone(),
        _ => return Ok(None),
    };
    Ok(Some(CursorRow {
        last_mtime,
        last_hash,
    }))
}

/// Upsert the `workspace_mirror_cursor` row for `path`.
async fn upsert_cursor(
    runtime: &KhiveRuntime,
    path: &Path,
    mtime: i64,
    checksum: &str,
) -> Result<(), RuntimeError> {
    let now_us = Utc::now().timestamp_micros();
    let path_str = path.to_string_lossy().into_owned();
    let sql = runtime.sql();
    let mut writer = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(format!("workspace mirror: cursor writer: {e}")))?;
    writer
        .execute(SqlStatement {
            sql:
                "INSERT INTO workspace_mirror_cursor(path, last_mtime, last_hash, last_synced_at) \
                  VALUES(?1, ?2, ?3, ?4) \
                  ON CONFLICT(path) DO UPDATE SET \
                    last_mtime = excluded.last_mtime, \
                    last_hash = excluded.last_hash, \
                    last_synced_at = excluded.last_synced_at"
                    .into(),
            params: vec![
                SqlValue::Text(path_str),
                SqlValue::Integer(mtime),
                SqlValue::Text(checksum.to_string()),
                SqlValue::Integer(now_us),
            ],
            label: Some("workspace_mirror_cursor_upsert".into()),
        })
        .await
        .map_err(|e| RuntimeError::Internal(format!("workspace mirror: cursor upsert: {e}")))?;
    Ok(())
}

// ── unit tests: path mapping, truncation, checksum ────────────────────────

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn entity_type_maps_handoffs_summaries_reports_and_default() {
        assert_eq!(
            entity_type_for_path("notes/handoffs/handoff_1.md"),
            "handoff"
        );
        assert_eq!(
            entity_type_for_path("notes/summaries/summary_1.md"),
            "summary"
        );
        assert_eq!(entity_type_for_path("notes/research/x.md"), "note");
        assert_eq!(entity_type_for_path("reports/audit.md"), "report");
        assert_eq!(
            entity_type_for_path("codex_reviews/codex_review_pr1.md"),
            "report"
        );
        assert_eq!(
            entity_type_for_path("workspaces/topic/artifacts/final.md"),
            "report"
        );
        assert_eq!(entity_type_for_path("some/other/thing.md"), "other");
    }

    #[test]
    fn source_type_maps_markdown_and_falls_back_to_plain_text() {
        assert_eq!(
            source_type_for_path(Path::new("notes/x.md")),
            "text/markdown"
        );
        assert_eq!(
            source_type_for_path(Path::new("notes/x.markdown")),
            "text/markdown"
        );
        assert_eq!(source_type_for_path(Path::new("notes/x.txt")), "text/plain");
        assert_eq!(source_type_for_path(Path::new("notes/x")), "text/plain");
    }

    #[test]
    fn truncate_for_inline_is_a_no_op_under_the_soft_limit() {
        let small = "hello world";
        assert_eq!(truncate_for_inline(small), small);
    }

    #[test]
    fn truncate_for_inline_caps_oversized_content_at_a_char_boundary() {
        // A multi-byte UTF-8 character straddling the exact cutoff must not
        // panic — the boundary search must back off to a valid char start.
        let mut big = "x".repeat(INLINE_CONTENT_SOFT_LIMIT - 1);
        big.push('€'); // 3-byte UTF-8 character landing across the cutoff
        big.push_str(&"y".repeat(1000));

        let truncated = truncate_for_inline(&big);
        assert!(truncated.len() < big.len());
        assert!(truncated.contains("truncated"));
        assert!(truncated.contains("source_uri"));
    }

    #[test]
    fn content_checksum_is_stable_and_content_sensitive() {
        let a = content_checksum("hello");
        let b = content_checksum("hello");
        let c = content_checksum("world");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }
}

// ── integration tests: full mirror_file behavior over a tempdir fixture ───

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;

    use khive_runtime::{AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig};
    use tempfile::TempDir;

    use super::*;

    /// Build a file-backed runtime with the workspace-mirror cursor schema
    /// applied, plus an authorized local token.
    async fn setup() -> (KhiveRuntime, NamespaceToken, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("file-backed runtime");
        apply_mirror_schema(&rt).await;
        let token = rt.authorize(Namespace::local()).expect("authorize");
        (rt, token, dir)
    }

    async fn apply_mirror_schema(rt: &KhiveRuntime) {
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        for stmt in &crate::mirror::WORKSPACE_MIRROR_SCHEMA_PLAN_STMTS {
            w.execute_script(stmt.to_string())
                .await
                .expect("schema stmt");
        }
    }

    fn write_file(khive_dir: &Path, rel: &str, content: &str) -> std::path::PathBuf {
        let full = khive_dir.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        full
    }

    async fn count_documents(rt: &KhiveRuntime) -> i64 {
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT COUNT(*) FROM entities WHERE kind='document'".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("count query")
            .expect("count row");
        match row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Integer(n)) => *n,
            _ => 0,
        }
    }

    #[tokio::test]
    async fn first_pass_creates_a_document_entity_with_correct_type_and_masked_content() {
        let (rt, token, tmp) = setup().await;
        let khive_dir = tmp.path().join(".khive");

        // AWS-key-shaped fixture, assembled at runtime so no credential-
        // shaped literal is committed to the repo (matches the session
        // mirror's own secret-masking test fixtures).
        let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
        let content = format!("# Handoff\n\nhere is my key: {secret}\n");
        let path = write_file(&khive_dir, "notes/handoffs/handoff_1.md", &content);

        let outcome = mirror_file(&rt, &token, &khive_dir, &path)
            .await
            .expect("first pass");

        let entity_id = match outcome {
            MirrorOutcome::Created {
                entity_id,
                superseded,
            } => {
                assert!(superseded.is_none(), "first version has no prior");
                entity_id
            }
            MirrorOutcome::Unchanged => panic!("first pass must create an entity"),
        };

        assert_eq!(count_documents(&rt).await, 1);

        let entity = rt.get_entity(&token, entity_id).await.expect("get entity");
        assert_eq!(entity.entity_type.as_deref(), Some("handoff"));
        let description = entity.description.expect("description set");
        assert!(
            !description.contains(&secret),
            "stored description must not contain the raw secret"
        );
        assert!(
            description.contains("***MASKED***"),
            "stored description must carry the secret_gate redaction marker"
        );
    }

    #[tokio::test]
    async fn second_pass_over_unchanged_file_is_a_no_op() {
        let (rt, token, tmp) = setup().await;
        let khive_dir = tmp.path().join(".khive");
        let path = write_file(&khive_dir, "reports/audit.md", "# Audit\n\nAll clear.\n");

        let first = mirror_file(&rt, &token, &khive_dir, &path)
            .await
            .expect("first pass");
        assert!(matches!(first, MirrorOutcome::Created { .. }));
        assert_eq!(count_documents(&rt).await, 1);

        let second = mirror_file(&rt, &token, &khive_dir, &path)
            .await
            .expect("second pass");
        assert_eq!(
            second,
            MirrorOutcome::Unchanged,
            "unchanged content must not create a new entity"
        );
        assert_eq!(
            count_documents(&rt).await,
            1,
            "no new document entity on a no-op pass"
        );
    }

    #[tokio::test]
    async fn changed_file_creates_a_new_version_and_supersedes_edge_leaving_old_entity_intact() {
        let (rt, token, tmp) = setup().await;
        let khive_dir = tmp.path().join(".khive");
        let path = write_file(&khive_dir, "reports/audit.md", "# Audit v1\n");

        let first = mirror_file(&rt, &token, &khive_dir, &path)
            .await
            .expect("first pass");
        let old_id = match first {
            MirrorOutcome::Created { entity_id, .. } => entity_id,
            MirrorOutcome::Unchanged => panic!("first pass must create"),
        };

        // Mutate the file's content (and force mtime to advance on
        // coarse-grained filesystems).
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_file(&khive_dir, "reports/audit.md", "# Audit v2\n\nRevised.\n");

        let second = mirror_file(&rt, &token, &khive_dir, &path)
            .await
            .expect("second pass over changed content");
        let (new_id, superseded) = match second {
            MirrorOutcome::Created {
                entity_id,
                superseded,
            } => (entity_id, superseded),
            MirrorOutcome::Unchanged => panic!("changed content must create a new version"),
        };
        assert_eq!(
            superseded,
            Some(old_id),
            "the new version must supersede the exact prior entity"
        );
        assert_ne!(new_id, old_id, "a changed file gets a NEW entity id");

        assert_eq!(
            count_documents(&rt).await,
            2,
            "both the old and new versions must exist — no in-place overwrite"
        );

        // The old entity must still be readable and unmodified in content.
        let old_entity = rt
            .get_entity(&token, old_id)
            .await
            .expect("old entity must remain intact");
        assert!(old_entity
            .description
            .as_deref()
            .unwrap_or_default()
            .contains("Audit v1"));

        // The supersedes edge must exist new -> old.
        let edges = rt
            .list_edges(
                &token,
                khive_runtime::curation::EdgeListFilter {
                    source_id: Some(new_id),
                    target_id: Some(old_id),
                    relations: vec![EdgeRelation::Supersedes],
                    ..Default::default()
                },
                10,
            )
            .await
            .expect("list edges");
        assert_eq!(edges.len(), 1, "exactly one supersedes edge new -> old");
    }

    #[tokio::test]
    async fn missing_file_returns_an_error_without_writing_a_cursor() {
        let (rt, _token, tmp) = setup().await;
        let khive_dir = tmp.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).unwrap();
        let bad_path = khive_dir.join("notes/does-not-exist.md");
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = mirror_file(&rt, &token, &khive_dir, &bad_path).await;
        assert!(result.is_err(), "a missing file must return an error");

        let cursor = load_cursor(&rt, &bad_path).await.expect("cursor query");
        assert!(
            cursor.is_none(),
            "an error pass must never write a cursor row"
        );
    }
}
