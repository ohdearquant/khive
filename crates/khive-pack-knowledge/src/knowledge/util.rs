//! Shared SQL helpers and row-to-type converters for knowledge handlers.

use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::schema::{Atom, Domain};

// ─── TF-IDF weight defaults ───────────────────────────────────────────────────

pub(super) const D_W_EXACT_NAME: f32 = 5.0;
pub(super) const D_W_NAME: f32 = 3.0;
pub(super) const D_W_TAGS: f32 = 1.25;
pub(super) const D_W_CONTENT: f32 = 1.5;
pub(super) const D_EXPAND_DISCOUNT: f32 = 0.35;
pub(super) const D_COVERAGE_ALPHA: f32 = 0.5;
pub(super) const D_W_BIGRAM: f32 = 2.0;

pub(super) const CANDIDATE_POOL: usize = 2000;
pub(super) const MIN_TERM_LEN: usize = 3;
pub(super) const EMBED_BATCH: usize = 32;
pub(super) const MAX_EMBED_BYTES: usize = 32_768;

pub(super) static STOP_WORDS: &[&str] = &[
    "and", "are", "also", "but", "can", "did", "does", "for", "from", "had", "has", "have", "its",
    "just", "may", "not", "our", "out", "than", "that", "the", "then", "this", "was", "were",
    "will", "with",
];

pub(super) fn is_stop(w: &str) -> bool {
    STOP_WORDS.contains(&w)
}

// ─── content hash and validation ─────────────────────────────────────────────

/// Minimum section content length in bytes.
pub(super) const MIN_SECTION_CONTENT_LEN: usize = 80;

/// Minimum atom content length in words.
pub(super) const MIN_ATOM_CONTENT_WORDS: usize = 20;

/// Compute sha256(content)[:16] as a hex string for dedup keying.
pub(super) fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let hash = hasher.finalize();
    format!("{hash:x}")[..16].to_string()
}

/// Validate that section content meets the 80-character minimum.
pub(super) fn validate_section_content(content: &str) -> Result<(), RuntimeError> {
    if content.len() < MIN_SECTION_CONTENT_LEN {
        return Err(RuntimeError::InvalidInput(format!(
            "section content must be at least {} characters (got {})",
            MIN_SECTION_CONTENT_LEN,
            content.len()
        )));
    }
    Ok(())
}

/// Validate that atom content meets the 20-word minimum.
pub(super) fn validate_atom_content(content: &str) -> Result<(), RuntimeError> {
    let word_count = content.split_whitespace().count();
    if word_count < MIN_ATOM_CONTENT_WORDS {
        return Err(RuntimeError::InvalidInput(format!(
            "atom content must be at least {} words (got {})",
            MIN_ATOM_CONTENT_WORDS, word_count
        )));
    }
    Ok(())
}

// ─── error helpers ───────────────────────────────────────────────────────────

pub(super) fn sql_err(ctx: &str, e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Internal(format!("{ctx}: {e}"))
}

pub(super) fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

// ─── SQL helpers ─────────────────────────────────────────────────────────────

pub(super) fn now_us() -> i64 {
    Utc::now().timestamp_micros()
}

pub(super) fn new_id() -> String {
    Uuid::new_v4().to_string()
}

pub(super) fn tags_to_json(tags: Option<&Vec<String>>) -> String {
    match tags {
        Some(t) => serde_json::to_string(t).unwrap_or_else(|_| "[]".into()),
        None => "[]".to_string(),
    }
}

pub(super) fn row_str(row: &khive_storage::types::SqlRow, col: &str) -> Option<String> {
    match row.get(col) {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

pub(super) fn row_i64(row: &khive_storage::types::SqlRow, col: &str) -> Option<i64> {
    match row.get(col) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

pub(super) fn row_bool(row: &khive_storage::types::SqlRow, col: &str) -> bool {
    matches!(row.get(col), Some(SqlValue::Integer(1)))
}

pub(super) fn atom_from_row(row: &khive_storage::types::SqlRow) -> Option<Atom> {
    let id: Uuid = row_str(row, "id")?.parse().ok()?;
    Some(Atom {
        id,
        namespace: row_str(row, "namespace")?,
        slug: row_str(row, "slug")?,
        name: row_str(row, "name")?,
        content: row_str(row, "content").unwrap_or_default(),
        tags: row_str(row, "tags").unwrap_or_else(|| "[]".into()),
        properties: row_str(row, "properties"),
        status: row_str(row, "status"),
        source_uri: row_str(row, "source_uri"),
        source_type: row_str(row, "source_type"),
        finalized: row_bool(row, "finalized"),
        created_at: row_i64(row, "created_at").unwrap_or(0),
        updated_at: row_i64(row, "updated_at").unwrap_or(0),
        deleted_at: row_i64(row, "deleted_at"),
    })
}

pub(super) fn domain_from_row(row: &khive_storage::types::SqlRow) -> Option<Domain> {
    let id: Uuid = row_str(row, "id")?.parse().ok()?;
    Some(Domain {
        id,
        namespace: row_str(row, "namespace")?,
        slug: row_str(row, "slug")?,
        name: row_str(row, "name")?,
        description: row_str(row, "description"),
        tags: row_str(row, "tags").unwrap_or_else(|| "[]".into()),
        members: row_str(row, "members").unwrap_or_else(|| "[]".into()),
        created_at: row_i64(row, "created_at").unwrap_or(0),
        updated_at: row_i64(row, "updated_at").unwrap_or(0),
        deleted_at: row_i64(row, "deleted_at"),
    })
}

pub(super) fn atom_to_json(atom: &Atom) -> Value {
    json!({
        "id": atom.id.to_string(),
        "namespace": atom.namespace,
        "slug": atom.slug,
        "name": atom.name,
        "content": atom.content,
        "tags": serde_json::from_str::<Value>(&atom.tags).unwrap_or(Value::Array(vec![])),
        "properties": atom.properties.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()),
        "status": atom.status,
        "source_uri": atom.source_uri,
        "source_type": atom.source_type,
        "finalized": atom.finalized,
        "kind": "atom",
        "created_at": micros_to_iso(atom.created_at),
        "updated_at": micros_to_iso(atom.updated_at),
    })
}

pub(super) fn domain_to_json(domain: &Domain) -> Value {
    json!({
        "id": domain.id.to_string(),
        "namespace": domain.namespace,
        "slug": domain.slug,
        "name": domain.name,
        "description": domain.description,
        "tags": serde_json::from_str::<Value>(&domain.tags).unwrap_or(Value::Array(vec![])),
        "members": serde_json::from_str::<Value>(&domain.members).unwrap_or(Value::Array(vec![])),
        "kind": "domain",
        "created_at": micros_to_iso(domain.created_at),
        "updated_at": micros_to_iso(domain.updated_at),
    })
}

// ─── status helpers ───────────────────────────────────────────────────────────

pub(super) fn status_values(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s.to_string()]
            }
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn status_sql_clause(
    statuses: &[String],
    exclude_status: Option<&str>,
    first_param: usize,
) -> (String, Vec<SqlValue>) {
    if !statuses.is_empty() {
        let placeholders = statuses
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", first_param + i))
            .collect::<Vec<_>>()
            .join(",");
        let clause = if statuses.len() == 1 {
            format!(" AND status = ?{first_param}")
        } else {
            format!(" AND status IN ({placeholders})")
        };
        let params = statuses.iter().cloned().map(SqlValue::Text).collect();
        return (clause, params);
    }

    if let Some(status) = exclude_status.map(str::trim).filter(|s| !s.is_empty()) {
        return (
            format!(" AND (status IS NULL OR status != ?{first_param})"),
            vec![SqlValue::Text(status.to_string())],
        );
    }

    (
        " AND (status IS NULL OR status != 'deprecated')".to_string(),
        Vec::new(),
    )
}

pub(super) fn explicitly_requested_status(statuses: &[String], status: &str) -> bool {
    statuses.iter().any(|s| s == status)
}

pub(super) fn status_multiplier(status: Option<&str>) -> f32 {
    match status.unwrap_or("reviewed") {
        "verified" => 1.2,
        "reviewed" => 1.0,
        "draft" => 0.8,
        "deprecated" => 0.0,
        _ => 1.0,
    }
}

// ─── embed text helper ────────────────────────────────────────────────────────

pub(super) fn atom_embed_text(atom: &Atom) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(2);
    if !atom.name.is_empty() {
        parts.push(&atom.name);
    }
    if !atom.content.is_empty() {
        parts.push(&atom.content);
    }
    parts.join("\n\n")
}

// ─── atom id resolver ─────────────────────────────────────────────────────────

pub(super) async fn resolve_atom_id(
    runtime: &KhiveRuntime,
    ns: &str,
    id_or_slug: &str,
) -> Result<String, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("resolve_atom_id reader", e))?;
    let id = id_or_slug.trim().to_string();
    let row = if id.parse::<Uuid>().is_ok() {
        reader
            .query_row(SqlStatement {
                sql: "SELECT id FROM knowledge_atoms WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("resolve_atom_id by id", e))?
    } else {
        reader
            .query_row(SqlStatement {
                sql: "SELECT id FROM knowledge_atoms WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("resolve_atom_id by slug", e))?
    };
    row.and_then(|r| row_str(&r, "id"))
        .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {id:?}")))
}

// ─── embedding coverage ───────────────────────────────────────────────────────

pub(super) async fn compute_embedding_coverage(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    ns: &str,
    total_atoms: i64,
) -> Result<f64, RuntimeError> {
    if total_atoms <= 0 || runtime.default_embedder_name().is_empty() {
        return Ok(0.0);
    }

    match runtime.vectors(token) {
        Ok(_) => {}
        Err(RuntimeError::Unconfigured(_)) => return Ok(0.0),
        Err(e) => return Err(e),
    }

    let model = runtime.default_embedder_name().to_owned();
    let table_name = format!("vec_{}", super::vamana::sanitize_model_key(&model));
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("stats embedding coverage reader", e))?;

    let count = reader
        .query_scalar(SqlStatement {
            sql: format!(
                "SELECT COUNT(DISTINCT a.id) \
                 FROM knowledge_atoms a \
                 WHERE a.namespace = ?1 \
                   AND a.deleted_at IS NULL \
                   AND a.tags NOT LIKE '%type:domain%' \
                   AND a.id IN ( \
                       SELECT v.subject_id FROM {table_name} v \
                       WHERE v.namespace = ?1 \
                         AND v.embedding_model = ?2 \
                         AND v.field = 'knowledge.atom' \
                   )"
            ),
            params: vec![SqlValue::Text(ns.to_owned()), SqlValue::Text(model.clone())],
            label: Some("knowledge_stats_embedding_coverage".into()),
        })
        .await
        .map_err(|e| sql_err("stats embedding coverage", e))?;

    let atoms_with_vector = match count {
        Some(SqlValue::Integer(n)) => n,
        Some(other) => {
            return Err(RuntimeError::Internal(format!(
                "stats embedding coverage returned non-integer count: {other:?}"
            )));
        }
        None => 0,
    };

    Ok(atoms_with_vector as f64 / total_atoms as f64)
}
