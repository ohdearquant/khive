//! Knowledge corpus handlers — atoms, domains, TF-IDF search, fold, index.
//!
//! Atoms and domains are stored in dedicated `knowledge_atoms` /
//! `knowledge_domains` tables (V19 migration) separate from the notes/entities
//! substrate. This lets the knowledge corpus scale to hundreds of thousands of
//! items without polluting the general-purpose store.
//!
//! Verbs implemented here:
//! - `upsert_atoms`  — bulk insert/update atoms by slug
//! - `upsert_domains` — bulk insert/update domains (named atom groups)
//! - `knowledge.get`    — fetch one atom or domain by ID or slug
//! - `knowledge.list`   — paginated listing
//! - `delete_atoms`     — soft-delete by slug
//! - `stats`            — corpus statistics
//! - `index`            — backfill embeddings + FTS for atoms
//! - `fold`             — budget-constrained knapsack selection
//! - `knowledge.search` — TF-IDF + embedding rerank (default; opt out with rerank=false)

pub(crate) mod matching;
pub(crate) mod schema;
pub(crate) mod vamana;

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use khive_fold::{GreedySelector, Selector, SelectorInput, SelectorWeights};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_score::DeterministicScore;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::SubstrateKind;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::knowledge::schema::{
    AdjudicateParams, Atom, ChallengeParams, ComposeParams, DeleteAtomsParams, Domain, EditParams,
    FoldCandidate, FoldParams, GetParams, ImportParams, IndexParams, ListParams, SearchParams,
    Section, SectionType, SuggestParams, UpsertAtomsParams, UpsertDomainsParams,
};

// ─── TF-IDF weight defaults ───────────────────────────────────────────────────

const D_W_EXACT_NAME: f32 = 5.0;
const D_W_NAME: f32 = 3.0;
const D_W_DESCRIPTION: f32 = 1.5;
const D_W_TAGS: f32 = 1.25;
const D_W_CONTENT: f32 = 1.0;
const D_EXPAND_DISCOUNT: f32 = 0.35;
const D_COVERAGE_ALPHA: f32 = 0.5;
const D_W_BIGRAM: f32 = 2.0;

const CANDIDATE_POOL: usize = 2000;
const MIN_TERM_LEN: usize = 3;
const EMBED_BATCH: usize = 32;
const MAX_EMBED_BYTES: usize = 32_768;

static STOP_WORDS: &[&str] = &[
    "and", "are", "also", "but", "can", "did", "does", "for", "from", "had", "has", "have", "its",
    "just", "may", "not", "our", "out", "than", "that", "the", "then", "this", "was", "were",
    "will", "with",
];

fn is_stop(w: &str) -> bool {
    STOP_WORDS.contains(&w)
}

// ─── runtime error helpers ───────────────────────────────────────────────────

fn sql_err(ctx: &str, e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Internal(format!("{ctx}: {e}"))
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

// ─── SQL helpers ─────────────────────────────────────────────────────────────

fn now_us() -> i64 {
    Utc::now().timestamp_micros()
}

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

fn tags_to_json(tags: Option<&Vec<String>>) -> String {
    match tags {
        Some(t) => serde_json::to_string(t).unwrap_or_else(|_| "[]".into()),
        None => "[]".to_string(),
    }
}

fn row_str(row: &khive_storage::types::SqlRow, col: &str) -> Option<String> {
    match row.get(col) {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn row_i64(row: &khive_storage::types::SqlRow, col: &str) -> Option<i64> {
    match row.get(col) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

fn row_bool(row: &khive_storage::types::SqlRow, col: &str) -> bool {
    matches!(row.get(col), Some(SqlValue::Integer(1)))
}

fn atom_from_row(row: &khive_storage::types::SqlRow) -> Option<Atom> {
    let id: Uuid = row_str(row, "id")?.parse().ok()?;
    Some(Atom {
        id,
        namespace: row_str(row, "namespace")?,
        slug: row_str(row, "slug")?,
        name: row_str(row, "name")?,
        description: row_str(row, "description"),
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

fn domain_from_row(row: &khive_storage::types::SqlRow) -> Option<Domain> {
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

fn atom_to_json(atom: &Atom) -> Value {
    json!({
        "id": atom.id.to_string(),
        "namespace": atom.namespace,
        "slug": atom.slug,
        "name": atom.name,
        "description": atom.description,
        "content": atom.content,
        "tags": serde_json::from_str::<Value>(&atom.tags).unwrap_or(Value::Array(vec![])),
        "properties": atom.properties.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()),
        "status": atom.status,
        "source_uri": atom.source_uri,
        "source_type": atom.source_type,
        "finalized": atom.finalized,
        "kind": "atom",
        "created_at": atom.created_at,
        "updated_at": atom.updated_at,
    })
}

fn domain_to_json(domain: &Domain) -> Value {
    json!({
        "id": domain.id.to_string(),
        "namespace": domain.namespace,
        "slug": domain.slug,
        "name": domain.name,
        "description": domain.description,
        "tags": serde_json::from_str::<Value>(&domain.tags).unwrap_or(Value::Array(vec![])),
        "members": serde_json::from_str::<Value>(&domain.members).unwrap_or(Value::Array(vec![])),
        "kind": "domain",
        "created_at": domain.created_at,
        "updated_at": domain.updated_at,
    })
}

// ─── public handler entry points ─────────────────────────────────────────────

pub(crate) struct KnowledgeHandlers;

impl KnowledgeHandlers {
    // ── upsert_atoms ──────────────────────────────────────────────────────────

    pub(crate) async fn upsert_atoms(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: UpsertAtomsParams = deser(params)?;
        if p.atoms.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "atoms list must not be empty".into(),
            ));
        }
        if p.atoms.len() > 5000 {
            return Err(RuntimeError::InvalidInput(
                "max 5000 atoms per request".into(),
            ));
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let now = now_us();
        let mut created = 0usize;
        let mut updated = 0usize;

        for atom_in in &p.atoms {
            let slug = atom_in.slug.trim().to_string();
            if slug.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "atom slug must not be empty".into(),
                ));
            }

            let tags_json = tags_to_json(atom_in.tags.as_ref());
            let content = atom_in.content.as_deref().unwrap_or("").to_string();
            let props_json = atom_in
                .properties
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let source_uri = atom_in
                .source_uri
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let source_type = atom_in
                .source_type
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());

            // Check if slug already exists.
            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("upsert_atoms reader", e))?;
            let existing = reader
                .query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(slug.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("upsert_atoms lookup", e))?;

            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("upsert_atoms writer", e))?;
            if let Some(row) = existing {
                let id = row_str(&row, "id").ok_or_else(|| {
                    RuntimeError::Internal("missing id in existing atom row".into())
                })?;
                writer
                    .execute(SqlStatement {
                        // Promote draft -> reviewed when this upsert finalizes the atom, mirroring
                        // the V22 backfill (finalized=1 => reviewed). Never demote an already
                        // reviewed/verified row, and leave status untouched when not finalizing
                        // (codex #527 round 2). ?8 (finalized) is reused in the CASE.
                        sql: "UPDATE knowledge_atoms SET name=?1, description=?2, content=?3, tags=?4, properties=?5, source_uri=?6, source_type=?7, finalized=?8, status = CASE WHEN ?8 = 1 AND status = 'draft' THEN 'reviewed' ELSE status END, updated_at=?9 WHERE id=?10 AND namespace=?11".into(),
                        params: vec![
                            SqlValue::Text(atom_in.name.clone()),
                            atom_in.description.as_ref().map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                            SqlValue::Text(content.clone()),
                            SqlValue::Text(tags_json.clone()),
                            props_json.as_ref().map_or(SqlValue::Null, |p| SqlValue::Text(p.clone())),
                            source_uri.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                            source_type.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                            SqlValue::Integer(atom_in.finalized.unwrap_or(false) as i64),
                            SqlValue::Integer(now),
                            SqlValue::Text(id),
                            SqlValue::Text(ns.clone()),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("upsert_atoms update", e))?;
                updated += 1;
            } else {
                let id = new_id();
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, description, content, tags, properties, source_uri, source_type, status, finalized, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)".into(),
                        params: vec![
                            SqlValue::Text(id),
                            SqlValue::Text(ns.clone()),
                            SqlValue::Text(slug.clone()),
                            SqlValue::Text(atom_in.name.clone()),
                            atom_in.description.as_ref().map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                            SqlValue::Text(content.clone()),
                            SqlValue::Text(tags_json.clone()),
                            props_json.as_ref().map_or(SqlValue::Null, |p| SqlValue::Text(p.clone())),
                            source_uri.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                            source_type.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                            // status mirrors the lifecycle backfill (finalized => reviewed) so a
                            // freshly-finalized atom is never left at the 'draft' default (codex #527).
                            SqlValue::Text(if atom_in.finalized.unwrap_or(false) { "reviewed" } else { "draft" }.to_string()),
                            SqlValue::Integer(atom_in.finalized.unwrap_or(false) as i64),
                            SqlValue::Integer(now),
                            SqlValue::Integer(now),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("upsert_atoms insert", e))?;
                created += 1;
            }
        }

        Ok(json!({
            "created": created,
            "updated": updated,
            "total": p.atoms.len(),
        }))
    }

    // ── upsert_domains ────────────────────────────────────────────────────────

    pub(crate) async fn upsert_domains(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: UpsertDomainsParams = deser(params)?;
        if p.domains.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "domains list must not be empty".into(),
            ));
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let now = now_us();
        let mut created = 0usize;
        let mut updated = 0usize;

        for domain_in in &p.domains {
            let slug = domain_in.slug.trim().to_string();
            let name = domain_in.name.trim().to_string();
            if slug.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "domain slug must not be empty".into(),
                ));
            }
            if name.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "domain name must not be empty".into(),
                ));
            }

            let mut tags: Vec<String> = domain_in.tags.clone().unwrap_or_default();
            if !tags.iter().any(|t| t == "type:domain") {
                tags.push("type:domain".to_string());
            }
            let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into());
            let members_json = match &domain_in.members {
                Some(m) => serde_json::to_string(m).unwrap_or_else(|_| "[]".into()),
                None => "[]".to_string(),
            };
            let properties_json = serde_json::to_string(
                &serde_json::json!({ "members": domain_in.members.as_deref().unwrap_or(&[]) }),
            )
            .unwrap_or_else(|_| "{}".into());

            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("upsert_domains reader", e))?;
            let existing = reader
                .query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_domains WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(slug.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("upsert_domains lookup", e))?;

            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("upsert_domains writer", e))?;
            if let Some(row) = existing {
                let id = row_str(&row, "id").ok_or_else(|| {
                    RuntimeError::Internal("missing id in existing domain row".into())
                })?;
                writer
                    .execute(SqlStatement {
                        sql: "UPDATE knowledge_domains SET name=?1, description=?2, tags=?3, members=?4, updated_at=?5 WHERE id=?6 AND namespace=?7".into(),
                        params: vec![
                            SqlValue::Text(name.clone()),
                            domain_in.description.as_ref().map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                            SqlValue::Text(tags_json.clone()),
                            SqlValue::Text(members_json.clone()),
                            SqlValue::Integer(now),
                            SqlValue::Text(id.clone()),
                            SqlValue::Text(ns.clone()),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("upsert_domains update", e))?;
                // Dual-write: sync the mirror atom in knowledge_atoms for FTS.
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, description, content, tags, properties, status, finalized, created_at, updated_at) \
                              VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'reviewed',1,?9,?10) \
                              ON CONFLICT(namespace, slug) DO UPDATE SET name=?4, description=?5, content=?6, tags=?7, properties=?8, status='reviewed', updated_at=?10".into(),
                        params: vec![
                            SqlValue::Text(id),
                            SqlValue::Text(ns.clone()),
                            SqlValue::Text(slug.clone()),
                            SqlValue::Text(name.clone()),
                            domain_in.description.as_ref().map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                            SqlValue::Text(String::new()),
                            SqlValue::Text(tags_json.clone()),
                            SqlValue::Text(properties_json.clone()),
                            SqlValue::Integer(now),
                            SqlValue::Integer(now),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("upsert_domains atom mirror update", e))?;
                updated += 1;
            } else {
                let id = new_id();
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO knowledge_domains (id, namespace, slug, name, description, tags, members, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)".into(),
                        params: vec![
                            SqlValue::Text(id.clone()),
                            SqlValue::Text(ns.clone()),
                            SqlValue::Text(slug.clone()),
                            SqlValue::Text(name.clone()),
                            domain_in.description.as_ref().map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                            SqlValue::Text(tags_json.clone()),
                            SqlValue::Text(members_json.clone()),
                            SqlValue::Integer(now),
                            SqlValue::Integer(now),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("upsert_domains insert", e))?;
                // Dual-write: mirror atom in knowledge_atoms for FTS indexing.
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, description, content, tags, properties, status, finalized, created_at, updated_at) \
                              VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'reviewed',1,?9,?10)".into(),
                        params: vec![
                            SqlValue::Text(id),
                            SqlValue::Text(ns.clone()),
                            SqlValue::Text(slug.clone()),
                            SqlValue::Text(name.clone()),
                            domain_in.description.as_ref().map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                            SqlValue::Text(String::new()),
                            SqlValue::Text(tags_json.clone()),
                            SqlValue::Text(properties_json.clone()),
                            SqlValue::Integer(now),
                            SqlValue::Integer(now),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("upsert_domains atom mirror insert", e))?;
                created += 1;
            }
        }

        Ok(json!({
            "created": created,
            "updated": updated,
            "total": p.domains.len(),
        }))
    }

    // ── get ───────────────────────────────────────────────────────────────────

    pub(crate) async fn get(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: GetParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let id = p.id.trim().to_string();

        // Try as UUID → atoms first, then domains.
        let is_uuid = id.parse::<Uuid>().is_ok();

        let mut reader = sql.reader().await.map_err(|e| sql_err("get reader", e))?;

        if is_uuid {
            // Lookup by UUID in atoms.
            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT * FROM knowledge_atoms WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("get atom by id", e))?;
            if let Some(r) = row {
                return atom_from_row(&r)
                    .map(|a| atom_to_json(&a))
                    .ok_or_else(|| RuntimeError::Internal("atom row parse failed".into()));
            }
            // Try domains.
            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT * FROM knowledge_domains WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("get domain by id", e))?;
            if let Some(r) = row {
                return domain_from_row(&r)
                    .map(|d| domain_to_json(&d))
                    .ok_or_else(|| RuntimeError::Internal("domain row parse failed".into()));
            }
        }

        // Lookup by slug — domains first (authoritative for members),
        // then atoms.
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_domains WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(id.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("get domain by slug", e))?;
        if let Some(r) = row {
            return domain_from_row(&r)
                .map(|d| domain_to_json(&d))
                .ok_or_else(|| RuntimeError::Internal("domain row parse failed".into()));
        }

        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(id.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("get atom by slug", e))?;
        if let Some(r) = row {
            return atom_from_row(&r)
                .map(|a| atom_to_json(&a))
                .ok_or_else(|| RuntimeError::Internal("atom row parse failed".into()));
        }

        Err(RuntimeError::NotFound(format!(
            "atom or domain not found: {id:?}"
        )))
    }

    // ── list ─────────────────────────────────────────────────────────────────

    pub(crate) async fn list(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ListParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let limit = p.limit.unwrap_or(20).clamp(1, 500) as i64;
        let offset = p.offset.unwrap_or(0) as i64;

        let mut reader = sql.reader().await.map_err(|e| sql_err("list reader", e))?;

        match p.kind.as_deref() {
            Some("domain") => {
                let rows = reader
                    .query_all(SqlStatement {
                        sql: "SELECT * FROM knowledge_domains WHERE namespace = ?1 AND deleted_at IS NULL ORDER BY created_at DESC LIMIT ?2 OFFSET ?3".into(),
                        params: vec![
                            SqlValue::Text(ns.clone()),
                            SqlValue::Integer(limit),
                            SqlValue::Integer(offset),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("list domains", e))?;

                let total_row = reader
                    .query_scalar(SqlStatement {
                        sql: "SELECT COUNT(*) FROM knowledge_domains WHERE namespace = ?1 AND deleted_at IS NULL".into(),
                        params: vec![SqlValue::Text(ns)],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("list domains count", e))?;
                let total = match total_row {
                    Some(SqlValue::Integer(n)) => n,
                    _ => 0,
                };

                let items: Vec<Value> = rows
                    .iter()
                    .filter_map(|r| domain_from_row(r).map(|d| domain_to_json(&d)))
                    .collect();

                Ok(json!({ "results": items, "total": total, "limit": limit, "offset": offset }))
            }
            Some("atom") | None => {
                let requested_statuses = status_values(p.status.as_ref());
                let (data_status_clause, data_status_params) =
                    status_sql_clause(&requested_statuses, p.exclude_status.as_deref(), 4);
                let (count_status_clause, count_status_params) =
                    status_sql_clause(&requested_statuses, p.exclude_status.as_deref(), 2);

                let sql_str = format!(
                    "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'{} ORDER BY created_at DESC LIMIT ?2 OFFSET ?3",
                    data_status_clause
                );
                let count_sql = format!(
                    "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'{}",
                    count_status_clause
                );

                let mut row_params = vec![
                    SqlValue::Text(ns.clone()),
                    SqlValue::Integer(limit),
                    SqlValue::Integer(offset),
                ];
                row_params.extend(data_status_params);

                let rows = reader
                    .query_all(SqlStatement {
                        sql: sql_str,
                        params: row_params,
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("list atoms", e))?;

                let mut count_params = vec![SqlValue::Text(ns)];
                count_params.extend(count_status_params);
                let total_row = reader
                    .query_scalar(SqlStatement {
                        sql: count_sql,
                        params: count_params,
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("list atoms count", e))?;
                let total = match total_row {
                    Some(SqlValue::Integer(n)) => n,
                    _ => 0,
                };

                let items: Vec<Value> = rows
                    .iter()
                    .filter_map(|r| atom_from_row(r).map(|a| atom_to_json(&a)))
                    .collect();

                Ok(json!({ "results": items, "total": total, "limit": limit, "offset": offset }))
            }
            Some(other) => Err(RuntimeError::InvalidInput(format!(
                "unknown type {other:?}; valid: atom | domain"
            ))),
        }
    }

    // ── delete_atoms ──────────────────────────────────────────────────────────

    pub(crate) async fn delete_atoms(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: DeleteAtomsParams = deser(params)?;
        if p.ids.is_empty() {
            return Err(RuntimeError::InvalidInput("ids must not be empty".into()));
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let now = now_us();
        let mut deleted = 0usize;

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| sql_err("delete_atoms writer", e))?;
        for id_or_slug in &p.ids {
            let id_or_slug = id_or_slug.trim().to_string();
            // Soft-delete by id or slug.
            let affected = writer
                .execute(SqlStatement {
                    sql: "UPDATE knowledge_atoms SET deleted_at = ?1 WHERE namespace = ?2 AND (id = ?3 OR slug = ?3) AND deleted_at IS NULL".into(),
                    params: vec![
                        SqlValue::Integer(now),
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(id_or_slug),
                    ],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("delete_atoms update", e))?;
            deleted += affected as usize;
        }

        Ok(json!({
            "deleted": deleted,
            "requested": p.ids.len(),
        }))
    }

    // ── stats ─────────────────────────────────────────────────────────────────

    pub(crate) async fn stats(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        _params: Value,
    ) -> Result<Value, RuntimeError> {
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let mut reader = sql.reader().await.map_err(|e| sql_err("stats reader", e))?;

        let atom_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'".into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("stats atoms", e))?;

        let domain_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM knowledge_domains WHERE namespace = ?1 AND deleted_at IS NULL".into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("stats domains", e))?;

        let finalized_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND finalized = 1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'".into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("stats finalized", e))?;

        let total_atoms = match atom_count {
            Some(SqlValue::Integer(n)) => n,
            _ => 0,
        };
        let total_domains = match domain_count {
            Some(SqlValue::Integer(n)) => n,
            _ => 0,
        };
        let finalized = match finalized_count {
            Some(SqlValue::Integer(n)) => n,
            _ => 0,
        };

        let eval_coverage = if total_atoms > 0 {
            finalized as f64 / total_atoms as f64
        } else {
            0.0
        };

        let embedding_coverage =
            compute_embedding_coverage(runtime, token, &ns, total_atoms).await?;

        Ok(json!({
            "total_atoms": total_atoms,
            "total_domains": total_domains,
            "total_events": 0,
            "eval_coverage": eval_coverage,
            "embedding_coverage": embedding_coverage,
            "namespace": ns,
        }))
    }

    // ── index ─────────────────────────────────────────────────────────────────

    pub(crate) async fn index(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let p: IndexParams = deser(params)?;
        let rebuild_ann = p.rebuild_ann.unwrap_or(false);
        let ns = token.namespace().as_str().to_owned();

        // If no embedder is configured, return immediately — nothing to index.
        if runtime.default_embedder_name().is_empty() {
            return Ok(
                json!({ "indexed": 0, "skipped": 0, "total": 0, "reason": "no embedding model configured" }),
            );
        }

        let sql = runtime.sql();
        let batch_size = p.batch_size.unwrap_or(500).clamp(1, 1000);
        let insert_only = p.insert_only.unwrap_or(false);

        // Resolve which atoms to index.
        let atoms: Vec<Atom> = if let Some(ref ids) = p.ids {
            let mut out = Vec::with_capacity(ids.len());
            let mut reader = sql.reader().await.map_err(|e| sql_err("index reader", e))?;
            for id_or_slug in ids {
                let row = reader
                    .query_row(SqlStatement {
                        sql: "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND (id = ?2 OR slug = ?2) AND deleted_at IS NULL LIMIT 1".into(),
                        params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(id_or_slug.clone())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("index atom lookup", e))?;
                if let Some(r) = row {
                    if let Some(a) = atom_from_row(&r) {
                        out.push(a);
                    }
                }
            }
            out
        } else {
            let mut out = Vec::new();
            let mut offset = 0i64;
            loop {
                let mut reader = sql
                    .reader()
                    .await
                    .map_err(|e| sql_err("index page reader", e))?;
                let rows = reader
                    .query_all(SqlStatement {
                        sql: "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL ORDER BY created_at LIMIT ?2 OFFSET ?3".into(),
                        params: vec![
                            SqlValue::Text(ns.clone()),
                            SqlValue::Integer(batch_size as i64),
                            SqlValue::Integer(offset),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("index page", e))?;
                let n = rows.len();
                out.extend(rows.iter().filter_map(atom_from_row));
                if n < batch_size {
                    break;
                }
                offset += n as i64;
            }
            out
        };

        let total = atoms.len();
        let mut indexed = 0usize;
        let mut skipped = 0usize;

        let mut ann_vectors: Vec<f32> = Vec::new();
        let mut ann_ids: Vec<Uuid> = Vec::new();
        let mut ann_dim: usize = 0;

        for chunk in atoms.chunks(EMBED_BATCH) {
            let mut staged: Vec<(Uuid, String)> = Vec::with_capacity(chunk.len());
            for atom in chunk {
                let text = atom_embed_text(atom);
                if text.trim().is_empty() {
                    skipped += 1;
                    continue;
                }
                staged.push((atom.id, text));
            }
            if staged.is_empty() {
                continue;
            }

            let texts: Vec<String> = staged
                .iter()
                .map(|(_, t)| {
                    if t.len() <= MAX_EMBED_BYTES {
                        t.clone()
                    } else {
                        let mut end = MAX_EMBED_BYTES;
                        while !t.is_char_boundary(end) {
                            end -= 1;
                        }
                        t[..end].to_string()
                    }
                })
                .collect();

            let embeddings = match runtime.embed_batch(&texts).await {
                Ok(e) => e,
                Err(_) => {
                    skipped += staged.len();
                    continue;
                }
            };
            if embeddings.len() != staged.len() {
                skipped += staged.len();
                continue;
            }

            if let Ok(vectors) = runtime.vectors(token) {
                let ns_str = token.namespace().as_str();
                if !insert_only {
                    for (id, _) in &staged {
                        let _ = vectors.delete(*id).await;
                    }
                }
                for ((id, _), emb) in staged.iter().zip(embeddings.iter()) {
                    let _ = vectors
                        .insert(
                            *id,
                            SubstrateKind::Entity,
                            ns_str,
                            "knowledge.atom",
                            vec![emb.clone()],
                        )
                        .await;
                }
            }

            if rebuild_ann {
                for ((id, _), emb) in staged.iter().zip(embeddings.iter()) {
                    if ann_dim == 0 {
                        ann_dim = emb.len();
                    }
                    if emb.len() == ann_dim {
                        ann_ids.push(*id);
                        ann_vectors.extend_from_slice(emb);
                    }
                }
            }

            indexed += staged.len();
        }

        // Any vector write invalidates the existing snapshot — the corpus has changed.
        // If rebuild_ann=true a fresh snapshot is persisted below; if false, ensure_ann
        // rebuilds lazily on the next search call.
        if indexed > 0 {
            vamana::invalidate_snapshot(runtime, &ns).await;
            *ann.index.write().await = None;
        }

        let mut ann_count: Option<usize> = None;
        let is_full_corpus = p.ids.is_none();
        if rebuild_ann && is_full_corpus && !ann_vectors.is_empty() && ann_dim > 0 {
            match vamana::AnnBridge::build(ann_vectors, ann_dim, ann_ids) {
                Ok(bridge) => {
                    ann_count = Some(bridge.num_vectors());
                    let model_name = runtime.default_embedder_name();
                    if let Some(fp) = vamana::compute_fingerprint(runtime, token, model_name).await
                    {
                        if let Err(e) =
                            vamana::persist_snapshot(runtime, &ns, model_name, &bridge, fp).await
                        {
                            tracing::error!(error = %e, "failed to persist Vamana snapshot");
                        }
                    }
                    let mut guard = ann.index.write().await;
                    *guard = Some(bridge);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to build Vamana ANN index");
                }
            }
        }

        Ok(json!({
            "indexed": indexed,
            "skipped": skipped,
            "total": total,
            "ann_vectors": ann_count,
        }))
    }

    // ── fold ─────────────────────────────────────────────────────────────────

    pub(crate) async fn fold(
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: FoldParams = deser(params)?;

        if p.candidates.is_empty() {
            return Ok(json!({
                "selected": [],
                "total_size": 0,
                "budget": p.budget,
                "selected_count": 0,
            }));
        }

        let inputs: Vec<SelectorInput<FoldCandidate>> = p
            .candidates
            .iter()
            .cloned()
            .map(|c| SelectorInput {
                id: c.id.clone(),
                score: c.score,
                size: c.size,
                category: c.category.clone(),
                information_gain: c.information_gain,
                content: c,
            })
            .collect();

        let weights = SelectorWeights {
            min_score: p.min_score.unwrap_or(0.0),
            category_weights: p.category_weights.unwrap_or_default().into_iter().collect(),
            diversity_bias: p.diversity_bias.unwrap_or(0.0),
            epistemic_weight: p.epistemic_weight.unwrap_or(0.0),
        };

        let output = GreedySelector
            .select(inputs, p.budget, &weights)
            .map_err(|e| RuntimeError::Internal(format!("fold selector: {e}")))?;

        let selected: Vec<Value> = output
            .selected
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "score": item.score,
                    "size": item.size,
                    "content": item.content.content,
                    "category": item.content.category,
                })
            })
            .collect();

        Ok(json!({
            "selected": selected,
            "total_size": output.total_size,
            "budget": p.budget,
            "selected_count": output.selected.len(),
        }))
    }

    // ── search ────────────────────────────────────────────────────────────────

    pub(crate) async fn search(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let p: SearchParams = deser(params)?;
        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }

        let limit = p.limit.unwrap_or(10).clamp(1, 100);
        let min_score = p.min_score.unwrap_or(0.0) as f32;
        let w = Weights::from_opts(&p);
        let type_filter = p.kind.as_deref();
        let do_decompose = p.decompose.unwrap_or(false);
        let decompose_threshold = p.decompose_threshold.unwrap_or(4);
        let intersection_bonus = p.intersection_bonus.unwrap_or(0.25) as f32;
        let requested_rerank = p.rerank.unwrap_or(true);
        let do_rerank = requested_rerank && !runtime.default_embedder_name().is_empty();
        let rerank_alpha = p.rerank_alpha.unwrap_or(0.7) as f32;
        let fetch_limit = if do_rerank { limit * 3 } else { limit }.min(100);

        let non_stop_count = raw_query
            .split_whitespace()
            .filter(|w| w.len() >= MIN_TERM_LEN && !is_stop(&w.to_lowercase()))
            .count();

        let ns = token.namespace().as_str().to_owned();
        let requested_statuses = status_values(p.status.as_ref());
        let include_deprecated = explicitly_requested_status(&requested_statuses, "deprecated");

        let ctx = SearchCtx {
            runtime,
            ns: &ns,
            role: p.role.as_deref(),
            type_filter,
            min_score,
            w: &w,
            fetch_limit,
            statuses: &requested_statuses,
            exclude_status: p.exclude_status.as_deref(),
        };

        let mut hits = if do_decompose && non_stop_count >= decompose_threshold {
            search_decomposed(&ctx, &raw_query, intersection_bonus).await?
        } else {
            search_core(&ctx, &raw_query).await?
        };

        // Trigger a fire-once background warm (ADR-049): never block search on the
        // ANN rebuild. The fusion below only runs if the index is already populated.
        vamana::ensure_ann_background(runtime, token, ann);

        // ANN parallel signal: embed query, search Vamana, fuse via RRF
        let ann_guard = ann.index.read().await;
        if let Some(ref bridge) = *ann_guard {
            if let Ok(query_emb) = runtime.embed(&raw_query).await {
                let ann_k = fetch_limit.max(20);
                let ann_hits = bridge.search(&query_emb, ann_k);
                if !ann_hits.is_empty() {
                    fuse_ann_hits(&mut hits, &ann_hits, min_score);
                    hydrate_empty_hits(runtime, &ns, &mut hits).await;
                }
            }
        }
        drop(ann_guard);

        if do_rerank && !hits.is_empty() {
            rerank_with_embeddings(runtime, &raw_query, &mut hits, rerank_alpha).await?;
        }

        apply_status_multipliers(&mut hits, include_deprecated);
        hits.truncate(limit);

        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "id": h.id,
                    "slug": h.slug,
                    "name": h.name,
                    "description": h.description,
                    "tags": h.tags,
                    "status": h.status,
                    "finalized": h.finalized,
                    "kind": if h.is_domain { "domain" } else { "atom" },
                    "score": h.score,
                })
            })
            .collect();
        let count = results.len();

        Ok(json!({
            "status": "ok",
            "data": { "results": results, "count": count },
        }))
    }

    // ── suggest ───────────────────────────────────────────────────────────────

    pub(crate) async fn suggest(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let p: SuggestParams = deser(params)?;
        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }
        let limit = p.limit.unwrap_or(8).clamp(1, 100);
        let ns = token.namespace().as_str().to_owned();

        let ctx = SearchCtx {
            runtime,
            ns: &ns,
            role: p.role.as_deref(),
            type_filter: Some("domain"),
            min_score: 0.0,
            w: &Weights::default(),
            fetch_limit: limit * 3,
            statuses: &[],
            exclude_status: None,
        };

        let mut hits = search_core(&ctx, &raw_query).await?;

        vamana::ensure_ann_background(runtime, token, ann);
        let ann_guard = ann.index.read().await;
        if let Some(ref bridge) = *ann_guard {
            if let Ok(query_emb) = runtime.embed(&raw_query).await {
                let ann_k = (limit * 3).max(20);
                let ann_hits = bridge.search(&query_emb, ann_k);
                if !ann_hits.is_empty() {
                    fuse_ann_hits(&mut hits, &ann_hits, 0.0);
                    hydrate_empty_hits(runtime, &ns, &mut hits).await;
                }
            }
        }
        drop(ann_guard);

        rerank_with_embeddings(runtime, &raw_query, &mut hits, 0.7).await?;

        hits.retain(|h| h.is_domain);
        hits.truncate(limit);

        let results: Vec<Value> = hits
            .iter()
            .map(|h| json!({ "id": h.id, "name": h.name, "score": h.score }))
            .collect();
        let count = results.len();

        Ok(json!({
            "status": "ok",
            "data": { "results": results, "count": count },
        }))
    }

    // ── compose ───────────────────────────────────────────────────────────────

    pub(crate) async fn compose(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ComposeParams = deser(params)?;
        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }

        let domain_ids: Vec<String> = p
            .domain_ids
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let atom_ids: Vec<String> = p
            .atom_ids
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();

        if domain_ids.is_empty() && atom_ids.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "domain_ids or atom_ids must be provided".into(),
            ));
        }

        let ns = token.namespace().as_str().to_owned();

        let mut resolved_domains: Vec<Domain> = Vec::new();
        let mut member_slugs: Vec<String> = Vec::new();

        for id in &domain_ids {
            let domain = load_domain_by_id_or_slug(runtime, &ns, id).await?;
            let members = parse_domain_members(&domain)?;
            member_slugs.extend(members);
            resolved_domains.push(domain);
        }

        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut ordered_atoms: Vec<Atom> = Vec::new();

        for slug in &member_slugs {
            let atom = load_atom_by_id_or_slug(runtime, &ns, slug).await?;
            if seen_ids.insert(atom.id.to_string()) {
                ordered_atoms.push(atom);
            }
        }
        for id in &atom_ids {
            let atom = load_atom_by_id_or_slug(runtime, &ns, id).await?;
            if seen_ids.insert(atom.id.to_string()) {
                ordered_atoms.push(atom);
            }
        }

        if ordered_atoms.is_empty() {
            return Ok(json!({
                "status": "ok",
                "data": {
                    "query": raw_query,
                    "markdown": "# Knowledge Briefing\n\nNo atoms found.",
                    "domains": [],
                    "atoms": [],
                    "count": 0,
                },
            }));
        }

        let mut items: Vec<ScoredTextItem> = ordered_atoms
            .iter()
            .map(|a| ScoredTextItem {
                id: a.id.to_string(),
                slug: a.slug.clone(),
                name: a.name.clone(),
                text: atom_embed_text(a),
                score: 1.0,
            })
            .collect();

        rerank_text_items(runtime, &raw_query, &mut items).await?;

        let sorted_atoms: Vec<(&Atom, f32)> = items
            .iter()
            .filter_map(|item| {
                ordered_atoms
                    .iter()
                    .find(|a| a.id.to_string() == item.id)
                    .map(|a| (a, item.score))
            })
            .collect();

        let markdown = format_compose_markdown(&raw_query, &resolved_domains, &sorted_atoms);

        let atom_json: Vec<Value> = items
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "slug": item.slug,
                    "name": item.name,
                    "score": item.score,
                })
            })
            .collect();

        let domain_json: Vec<Value> = resolved_domains
            .iter()
            .map(|d| json!({ "id": d.id.to_string(), "slug": d.slug, "name": d.name }))
            .collect();

        let count = atom_json.len();

        Ok(json!({
            "status": "ok",
            "data": {
                "query": raw_query,
                "markdown": markdown,
                "domains": domain_json,
                "atoms": atom_json,
                "count": count,
            },
        }))
    }
}

// ─── TF-IDF weight container ─────────────────────────────────────────────────

struct Weights {
    w_exact_name: f32,
    w_name: f32,
    w_description: f32,
    w_tags: f32,
    w_content: f32,
    expand_discount: f32,
    coverage_alpha: f32,
    w_bigram: f32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            w_exact_name: D_W_EXACT_NAME,
            w_name: D_W_NAME,
            w_description: D_W_DESCRIPTION,
            w_tags: D_W_TAGS,
            w_content: D_W_CONTENT,
            expand_discount: D_EXPAND_DISCOUNT,
            coverage_alpha: D_COVERAGE_ALPHA,
            w_bigram: D_W_BIGRAM,
        }
    }
}

impl Weights {
    fn from_opts(opts: &SearchParams) -> Self {
        let w = opts.weights.as_ref();
        Self {
            w_exact_name: w
                .and_then(|w| w.w_exact_name)
                .map_or(D_W_EXACT_NAME, |v| v as f32),
            w_name: w.and_then(|w| w.w_name).map_or(D_W_NAME, |v| v as f32),
            w_description: w
                .and_then(|w| w.w_description)
                .map_or(D_W_DESCRIPTION, |v| v as f32),
            w_tags: w.and_then(|w| w.w_tags).map_or(D_W_TAGS, |v| v as f32),
            w_content: w
                .and_then(|w| w.w_content)
                .map_or(D_W_CONTENT, |v| v as f32),
            expand_discount: w
                .and_then(|w| w.expand_discount)
                .map_or(D_EXPAND_DISCOUNT, |v| v as f32),
            coverage_alpha: w
                .and_then(|w| w.coverage_alpha)
                .map_or(D_COVERAGE_ALPHA, |v| v as f32),
            w_bigram: w.and_then(|w| w.w_bigram).map_or(D_W_BIGRAM, |v| v as f32),
        }
    }
}

// ─── scored hit (internal) ────────────────────────────────────────────────────

struct ScoredHit {
    id: String,
    slug: String,
    name: String,
    description: Option<String>,
    tags: Option<String>,
    finalized: bool,
    is_domain: bool,
    status: Option<String>,
    score: f32,
}

// ─── ANN fusion (symmetric RRF) ─────────────────────────────────────────────

const RRF_K: usize = 60;

fn normalize_rrf_score(raw: f32, source_count: usize, k: usize) -> f32 {
    if source_count == 0 {
        return 0.0;
    }
    let theoretical_max = source_count as f32 / (k as f32 + 1.0);
    (raw / theoretical_max).clamp(0.0, 1.0)
}

fn fuse_ann_hits(fts_hits: &mut Vec<ScoredHit>, ann_hits: &[(Uuid, f32)], min_score: f32) {
    let drained: Vec<ScoredHit> = std::mem::take(fts_hits);

    let fts_source: Vec<(String, DeterministicScore)> = drained
        .iter()
        .map(|hit| (hit.id.clone(), DeterministicScore::from_f32(hit.score)))
        .collect();
    let mut by_id: HashMap<String, ScoredHit> = drained
        .into_iter()
        .map(|hit| (hit.id.clone(), hit))
        .collect();
    let ann_source: Vec<(String, DeterministicScore)> = ann_hits
        .iter()
        .map(|(uuid, score)| (uuid.to_string(), DeterministicScore::from_f32(*score)))
        .collect();

    let source_count = usize::from(!fts_source.is_empty()) + usize::from(!ann_source.is_empty());
    let fused = khive_fusion::reciprocal_rank_fusion(vec![fts_source, ann_source], RRF_K);

    for (id, fused_score) in fused {
        let raw_score = fused_score.to_f64() as f32;
        let score = normalize_rrf_score(raw_score, source_count, RRF_K);
        if score < min_score {
            continue;
        }

        if let Some(mut hit) = by_id.remove(&id) {
            hit.score = score;
            fts_hits.push(hit);
        } else {
            fts_hits.push(ScoredHit {
                id,
                slug: String::new(),
                name: String::new(),
                description: None,
                tags: None,
                finalized: false,
                is_domain: false,
                status: None,
                score,
            });
        }
    }
}

async fn hydrate_empty_hits(runtime: &KhiveRuntime, ns: &str, hits: &mut Vec<ScoredHit>) {
    let ids: Vec<String> = hits
        .iter()
        .filter(|hit| hit.slug.is_empty())
        .map(|hit| hit.id.clone())
        .collect();
    if ids.is_empty() {
        return;
    }

    let sql = runtime.sql();
    let mut reader = match sql.reader().await {
        Ok(r) => r,
        Err(_) => return,
    };

    let placeholders = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(ids.iter().cloned().map(SqlValue::Text));

    let atom_rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT id, slug, name, description, tags, finalized, status FROM knowledge_atoms WHERE namespace = ?1 AND id IN ({placeholders}) AND deleted_at IS NULL"
            ),
            params,
            label: None,
        })
        .await
        .unwrap_or_default();

    let mut atom_rows_by_id: HashMap<String, khive_storage::types::SqlRow> = HashMap::new();
    for row in atom_rows {
        if let Some(id) = row_str(&row, "id") {
            atom_rows_by_id.insert(id, row);
        }
    }

    for hit in hits.iter_mut().filter(|hit| hit.slug.is_empty()) {
        if let Some(row) = atom_rows_by_id.get(&hit.id) {
            hit.slug = row_str(row, "slug").unwrap_or_default();
            hit.name = row_str(row, "name").unwrap_or_default();
            hit.description = row_str(row, "description");
            hit.tags = row_str(row, "tags");
            hit.finalized = row_bool(row, "finalized");
            hit.status = row_str(row, "status");
            let tags_arr: Vec<String> = hit
                .tags
                .as_deref()
                .and_then(|tags| serde_json::from_str(tags).ok())
                .unwrap_or_default();
            hit.is_domain = tags_arr.iter().any(|t| t == "type:domain");
        }
    }

    let missing_ids: Vec<String> = hits
        .iter()
        .filter(|hit| hit.slug.is_empty())
        .map(|hit| hit.id.clone())
        .collect();
    if missing_ids.is_empty() {
        return;
    }

    let placeholders = missing_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(missing_ids.iter().cloned().map(SqlValue::Text));

    let domain_rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT id, slug, name, description, tags FROM knowledge_domains WHERE namespace = ?1 AND id IN ({placeholders}) AND deleted_at IS NULL"
            ),
            params,
            label: None,
        })
        .await
        .unwrap_or_default();

    let mut domain_rows_by_id: HashMap<String, khive_storage::types::SqlRow> = HashMap::new();
    for row in domain_rows {
        if let Some(id) = row_str(&row, "id") {
            domain_rows_by_id.insert(id, row);
        }
    }

    for hit in hits.iter_mut().filter(|hit| hit.slug.is_empty()) {
        if let Some(row) = domain_rows_by_id.get(&hit.id) {
            hit.slug = row_str(row, "slug").unwrap_or_default();
            hit.name = row_str(row, "name").unwrap_or_default();
            hit.description = row_str(row, "description");
            hit.tags = row_str(row, "tags");
            hit.finalized = false;
            hit.is_domain = true;
        }
    }

    hits.retain(|hit| !hit.slug.is_empty());
}

// ─── status helpers (W5) ─────────────────────────────────────────────────────

fn status_values(value: Option<&Value>) -> Vec<String> {
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

fn status_sql_clause(
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

fn explicitly_requested_status(statuses: &[String], status: &str) -> bool {
    statuses.iter().any(|s| s == status)
}

fn status_multiplier(status: Option<&str>) -> f32 {
    match status.unwrap_or("reviewed") {
        "verified" => 1.2,
        "reviewed" => 1.0,
        "draft" => 0.8,
        "deprecated" => 0.0,
        _ => 1.0,
    }
}

fn apply_status_multipliers(hits: &mut Vec<ScoredHit>, include_deprecated: bool) {
    hits.retain_mut(|hit| {
        let multiplier = status_multiplier(hit.status.as_deref());
        // Squash raw score to (0,1) via monotonic s/(s+1) before applying the status
        // multiplier so that TF-IDF scores > 1 don't saturate ranking. RRF-normalized
        // scores (already ≤ 1) are squashed at most to 0.5, preserving relative order.
        hit.score = (hit.score / (hit.score + 1.0) * multiplier).clamp(0.0, 1.0);
        include_deprecated || multiplier > 0.0
    });
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.slug.cmp(&b.slug))
    });
}

// ─── candidate (tokenized) ───────────────────────────────────────────────────

struct Candidate {
    id: String,
    slug: String,
    name_raw: String,
    description_raw: Option<String>,
    tags_raw: Option<String>,
    status_raw: Option<String>,
    finalized: bool,
    is_domain: bool,
    name: Vec<String>,
    description: Vec<String>,
    tags: Vec<String>,
    content: Vec<String>,
}

fn load_candidates_from_atoms(atoms: &[Atom], type_filter: Option<&str>) -> Vec<Candidate> {
    let want_domain = type_filter == Some("domain");
    let want_atom = type_filter == Some("atom");

    atoms
        .iter()
        .filter_map(|atom| {
            let tags_str = atom.tags_display();
            let is_domain = {
                let tags_arr: Vec<String> = serde_json::from_str(&atom.tags).unwrap_or_default();
                tags_arr.iter().any(|t| t == "type:domain")
            };
            if (want_domain && !is_domain) || (want_atom && is_domain) {
                return None;
            }
            Some(Candidate {
                id: atom.id.to_string(),
                slug: atom.slug.clone(),
                name_raw: atom.name.clone(),
                description_raw: atom.description.clone(),
                tags_raw: Some(tags_str.clone()),
                status_raw: atom.status.clone(),
                finalized: atom.finalized,
                is_domain,
                name: matching::tokenize_field(&atom.name),
                description: atom
                    .description
                    .as_deref()
                    .map(matching::tokenize_field)
                    .unwrap_or_default(),
                tags: matching::tokenize_field(&tags_str),
                content: matching::tokenize_field(&atom.content),
            })
        })
        .collect()
}

// ─── IDF computation ──────────────────────────────────────────────────────────

fn compute_idf(
    candidates: &[Candidate],
    terms: &[String],
    expanded: &HashSet<String>,
    discount: f32,
) -> HashMap<String, f32> {
    let n = candidates.len() as f32;
    let mut df: HashMap<String, usize> = terms.iter().map(|t| (t.clone(), 0)).collect();
    for cand in candidates {
        for term in terms {
            if matching::has_in_tokens(&cand.content, term)
                || matching::has_in_tokens(&cand.name, term)
                || matching::has_in_tokens(&cand.description, term)
                || matching::has_in_tokens(&cand.tags, term)
            {
                if let Some(d) = df.get_mut(term) {
                    *d += 1;
                }
            }
        }
    }
    df.into_iter()
        .map(|(term, d)| {
            let raw = (n / (d as f32 + 1.0)).ln().max(0.1);
            let idf = if expanded.contains(&term) {
                raw * discount
            } else {
                raw
            };
            (term, idf)
        })
        .collect()
}

fn score_field(tokens: &[String], terms: &[String], idf: &HashMap<String, f32>) -> f32 {
    let mut score = 0.0;
    for term in terms {
        let count = matching::count_in_tokens(tokens, term);
        if count > 0 {
            let tf = 1.0 + (count as f32).ln();
            score += tf * idf.get(term).copied().unwrap_or(1.0);
        }
    }
    score
}

fn bigram_bonus_field(tokens: &[String], query_order: &[String]) -> f32 {
    if query_order.len() < 2 {
        return 0.0;
    }
    let filtered: Vec<&str> = tokens
        .iter()
        .filter(|t| !is_stop(t))
        .map(|t| t.as_str())
        .collect();
    let mut bonus = 0.0f32;
    for window in query_order.windows(2) {
        let (a, b) = (window[0].as_str(), window[1].as_str());
        for w in filtered.windows(2) {
            if w[0] == a && w[1] == b {
                bonus += 1.0;
                break;
            }
        }
    }
    bonus
}

fn exact_name_bonus(name: &str, raw_query: &str, bonus: f32) -> f32 {
    let q = raw_query.trim().to_lowercase();
    if !q.is_empty() && name.to_lowercase().contains(&q) {
        bonus
    } else {
        0.0
    }
}

fn score_candidate(
    cand: &Candidate,
    terms: &[String],
    original_terms: &[String],
    query_order: &[String],
    idf: &HashMap<String, f32>,
    raw_query: &str,
    w: &Weights,
) -> f32 {
    let bigrams = bigram_bonus_field(&cand.name, query_order)
        + bigram_bonus_field(&cand.description, query_order)
        + bigram_bonus_field(&cand.tags, query_order)
        + bigram_bonus_field(&cand.content, query_order);

    let base = exact_name_bonus(&cand.name_raw, raw_query, w.w_exact_name)
        + w.w_name * score_field(&cand.name, terms, idf)
        + w.w_description * score_field(&cand.description, terms, idf)
        + w.w_tags * score_field(&cand.tags, terms, idf)
        + w.w_content * score_field(&cand.content, terms, idf)
        + w.w_bigram * bigrams;

    if w.coverage_alpha > 0.0 && !original_terms.is_empty() {
        // For each original query term, check whether it OR any of its expanded
        // variants matches the candidate. This ensures that "agents" → "agent"
        // expansion still earns coverage credit.
        let matched = original_terms
            .iter()
            .filter(|orig| {
                // Check the original term or any term in `terms` that starts with it
                // (expansion variants share the stem with the original).
                let has_exact = matching::has_in_tokens(&cand.name, orig)
                    || matching::has_in_tokens(&cand.description, orig)
                    || matching::has_in_tokens(&cand.tags, orig)
                    || matching::has_in_tokens(&cand.content, orig);
                if has_exact {
                    return true;
                }
                // Check if any expansion of this original matches.
                terms.iter().filter(|t| *t != *orig).any(|exp| {
                    matching::has_in_tokens(&cand.name, exp)
                        || matching::has_in_tokens(&cand.description, exp)
                        || matching::has_in_tokens(&cand.tags, exp)
                        || matching::has_in_tokens(&cand.content, exp)
                })
            })
            .count();
        let coverage = matched as f32 / original_terms.len() as f32;
        base * coverage.powf(w.coverage_alpha)
    } else {
        base
    }
}

fn expand_terms(terms: &mut Vec<String>) -> HashSet<String> {
    let originals: HashSet<String> = terms.iter().cloned().collect();
    let snapshot: Vec<String> = terms.clone();
    for t in &snapshot {
        if !t.ends_with('s') && t.len() >= 3 {
            terms.push(format!("{t}s"));
        }
        if t.ends_with("ies") && t.len() > 4 {
            let s = format!("{}y", &t[..t.len() - 3]);
            if s.len() >= 3 {
                terms.push(s);
            }
        } else if t.ends_with('s') && !t.ends_with("ss") && t.len() > 3 {
            let s = t[..t.len() - 1].to_string();
            if s.len() >= 3 {
                terms.push(s);
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms
        .iter()
        .filter(|t| !originals.contains(*t))
        .cloned()
        .collect()
}

// ─── FTS5 phrase quoting ─────────────────────────────────────────────────────

fn quote_fts5_phrase(raw_query: &str) -> String {
    let escaped = raw_query.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

// ─── FTS5 candidate pool fetch ────────────────────────────────────────────────

async fn fetch_fts_candidates(
    runtime: &KhiveRuntime,
    ns: &str,
    raw_query: &str,
    type_filter: Option<&str>,
    statuses: &[String],
    exclude_status: Option<&str>,
    fetch_limit: usize,
) -> Result<Vec<Atom>, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("search fts reader", e))?;

    // Use the FTS5 virtual table to get candidate atom IDs quickly.
    let match_expr = quote_fts5_phrase(raw_query);
    let fts_rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id FROM fts_knowledge WHERE fts_knowledge MATCH ?1 AND namespace = ?2 LIMIT ?3".into(),
            params: vec![
                SqlValue::Text(match_expr),
                SqlValue::Text(ns.to_owned()),
                SqlValue::Integer(fetch_limit as i64),
            ],
            label: None,
        })
        .await
        .map_err(|e| sql_err("search fts query", e))?;

    if fts_rows.is_empty() {
        // FTS returned nothing — fall back to full scan (small corpora) capped at CANDIDATE_POOL.
        let (status_clause, status_params) = status_sql_clause(statuses, exclude_status, 3);
        let sql_str = format!(
            "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL{} ORDER BY created_at DESC LIMIT ?2",
            status_clause
        );
        let mut params = vec![
            SqlValue::Text(ns.to_owned()),
            SqlValue::Integer(CANDIDATE_POOL as i64),
        ];
        params.extend(status_params);

        let rows = reader
            .query_all(SqlStatement {
                sql: sql_str,
                params,
                label: None,
            })
            .await
            .map_err(|e| sql_err("search full scan", e))?;

        let mut atoms: Vec<Atom> = rows.iter().filter_map(atom_from_row).collect();
        if let Some(filt) = type_filter {
            let want_domain = filt == "domain";
            atoms.retain(|a| {
                let tags_arr: Vec<String> = serde_json::from_str(&a.tags).unwrap_or_default();
                let is_domain = tags_arr.iter().any(|t| t == "type:domain");
                if want_domain {
                    is_domain
                } else {
                    !is_domain
                }
            });
        }
        return Ok(atoms);
    }

    let ids: Vec<String> = fts_rows.iter().filter_map(|r| row_str(r, "id")).collect();
    let placeholders: String = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");

    let (status_clause, status_params) = status_sql_clause(statuses, exclude_status, ids.len() + 2);
    let mut params: Vec<SqlValue> = vec![SqlValue::Text(ns.to_owned())];
    params.extend(ids.iter().map(|id| SqlValue::Text(id.clone())));
    params.extend(status_params);

    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND id IN ({placeholders}) AND deleted_at IS NULL{status_clause}"
            ),
            params,
            label: None,
        })
        .await
        .map_err(|e| sql_err("search load atoms", e))?;

    Ok(rows.iter().filter_map(atom_from_row).collect())
}

// ─── search context (groups args to stay under clippy limit) ─────────────────

struct SearchCtx<'a> {
    runtime: &'a KhiveRuntime,
    ns: &'a str,
    role: Option<&'a str>,
    type_filter: Option<&'a str>,
    min_score: f32,
    w: &'a Weights,
    fetch_limit: usize,
    statuses: &'a [String],
    exclude_status: Option<&'a str>,
}

// ─── core single-pass search ──────────────────────────────────────────────────

async fn search_core(ctx: &SearchCtx<'_>, query: &str) -> Result<Vec<ScoredHit>, RuntimeError> {
    let runtime = ctx.runtime;
    let ns = ctx.ns;
    let role = ctx.role;
    let type_filter = ctx.type_filter;
    let min_score = ctx.min_score;
    let w = ctx.w;
    let fetch_limit = ctx.fetch_limit;
    let raw_query = query.trim().to_string();
    if raw_query.is_empty() {
        return Ok(Vec::new());
    }

    let scored_query = match role {
        Some(r) if !r.trim().is_empty() => format!("{} {}", r.trim(), raw_query),
        _ => raw_query.clone(),
    };

    let (terms, original_terms, query_order, expanded) = {
        let raw_tokens: Vec<String> = matching::tokenize_field(&scored_query)
            .into_iter()
            .filter(|w| w.len() >= MIN_TERM_LEN && !is_stop(w))
            .collect();
        let mut seen = HashSet::new();
        let qo: Vec<String> = raw_tokens
            .iter()
            .filter(|w| seen.insert(w.as_str()))
            .cloned()
            .collect();
        let mut t = raw_tokens;
        t.sort();
        t.dedup();
        let originals = t.clone();
        let exp = expand_terms(&mut t);
        (t, originals, qo, exp)
    };
    // When all query tokens are shorter than MIN_TERM_LEN (e.g. "RAG", "GQA", "LoRA"),
    // fall through to exact-name-bonus-only scoring rather than returning early.
    let terms_only_exact = terms.is_empty();

    let atoms = fetch_fts_candidates(
        runtime,
        ns,
        &raw_query,
        type_filter,
        ctx.statuses,
        ctx.exclude_status,
        CANDIDATE_POOL,
    )
    .await?;
    if atoms.is_empty() {
        return Ok(Vec::new());
    }

    let candidates = load_candidates_from_atoms(&atoms, type_filter);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let idf = compute_idf(&candidates, &terms, &expanded, w.expand_discount);
    let mut scored: Vec<(f32, &Candidate)> = candidates
        .iter()
        .filter_map(|cand| {
            let base = if terms_only_exact {
                // All query terms were sub-MIN_TERM_LEN (short acronyms).
                // Score only via exact_name_bonus so e.g. "LoRA" or "RAG" match their atom.
                exact_name_bonus(&cand.name_raw, &raw_query, w.w_exact_name)
            } else {
                score_candidate(
                    cand,
                    &terms,
                    &original_terms,
                    &query_order,
                    &idf,
                    &raw_query,
                    w,
                )
            };
            (base > min_score).then_some((base, cand))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.slug.cmp(&b.1.slug))
    });

    Ok(scored
        .into_iter()
        .take(fetch_limit)
        .map(|(score, cand)| ScoredHit {
            id: cand.id.clone(),
            slug: cand.slug.clone(),
            name: cand.name_raw.clone(),
            description: cand.description_raw.clone(),
            tags: cand.tags_raw.clone(),
            status: cand.status_raw.clone(),
            finalized: cand.finalized,
            is_domain: cand.is_domain,
            score,
        })
        .collect())
}

// ─── decomposed search ───────────────────────────────────────────────────────

async fn search_decomposed(
    ctx: &SearchCtx<'_>,
    query: &str,
    intersection_bonus: f32,
) -> Result<Vec<ScoredHit>, RuntimeError> {
    let non_stop: Vec<&str> = query
        .split_whitespace()
        .filter(|w| w.len() >= MIN_TERM_LEN && !is_stop(&w.to_lowercase()))
        .collect();

    let mid = non_stop.len() / 2;
    let sub_q1: String = non_stop[..mid].join(" ");
    let sub_q2: String = non_stop[mid..].join(" ");
    let sub_limit = ctx.fetch_limit.min(50);

    let full = search_core(ctx, query).await?;
    let sub_ctx1 = SearchCtx {
        runtime: ctx.runtime,
        ns: ctx.ns,
        role: None,
        type_filter: ctx.type_filter,
        min_score: 0.0,
        w: ctx.w,
        fetch_limit: sub_limit,
        statuses: ctx.statuses,
        exclude_status: ctx.exclude_status,
    };
    let s1 = search_core(&sub_ctx1, &sub_q1).await?;
    let s2 = search_core(&sub_ctx1, &sub_q2).await?;

    let mut scores: HashMap<String, f32> = HashMap::new();
    let mut data: HashMap<String, ScoredHit> = HashMap::new();

    for hit in full {
        scores.insert(hit.id.clone(), hit.score);
        data.insert(hit.id.clone(), hit);
    }

    let mut sub_counts: HashMap<String, u32> = HashMap::new();
    for hits in [s1, s2] {
        let mut seen: HashSet<String> = HashSet::new();
        for hit in hits {
            if !seen.insert(hit.id.clone()) {
                continue;
            }
            *sub_counts.entry(hit.id.clone()).or_default() += 1;
            if !data.contains_key(&hit.id) {
                scores.insert(hit.id.clone(), hit.score * 0.3);
                data.insert(hit.id.clone(), hit);
            }
        }
    }

    for (id, count) in &sub_counts {
        if *count >= 2 {
            if let Some(s) = scores.get_mut(id) {
                *s *= 1.0 + intersection_bonus * (*count as f32 - 1.0);
            }
        }
    }

    let mut ranked: Vec<ScoredHit> = data
        .into_values()
        .map(|mut h| {
            if let Some(&s) = scores.get(&h.id) {
                h.score = s;
            }
            h
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    ranked.truncate(ctx.fetch_limit);
    Ok(ranked)
}

// ─── embedding rerank ────────────────────────────────────────────────────────

// Shared embed+cosine core: embeds [query] + candidate texts and returns one
// cosine score per candidate, or None if the embedder is absent or fails.
async fn embed_cosine_scores(
    runtime: &KhiveRuntime,
    query: &str,
    candidate_texts: &[String],
) -> Option<Vec<f32>> {
    if runtime.default_embedder_name().is_empty() || candidate_texts.is_empty() {
        return None;
    }
    let mut texts = Vec::with_capacity(candidate_texts.len() + 1);
    texts.push(query.to_string());
    texts.extend_from_slice(candidate_texts);
    let embeddings = runtime.embed_batch(&texts).await.ok()?;
    if embeddings.len() != texts.len() {
        return None;
    }
    let query_emb = &embeddings[0];
    Some(
        embeddings[1..]
            .iter()
            .map(|emb| cosine_similarity(query_emb, emb))
            .collect(),
    )
}

async fn rerank_with_embeddings(
    runtime: &KhiveRuntime,
    query: &str,
    hits: &mut [ScoredHit],
    alpha: f32,
) -> Result<(), RuntimeError> {
    if hits.is_empty() {
        return Ok(());
    }
    let texts: Vec<String> = hits
        .iter()
        .map(|h| format!("{} {}", h.name, h.description.as_deref().unwrap_or("")))
        .collect();
    if let Some(cosines) = embed_cosine_scores(runtime, query, &texts).await {
        let max_tfidf = hits
            .iter()
            .map(|h| h.score)
            .fold(0.0f32, f32::max)
            .max(1e-6);
        for (hit, cos) in hits.iter_mut().zip(cosines.iter()) {
            let norm_tfidf = hit.score / max_tfidf;
            hit.score = alpha * norm_tfidf + (1.0 - alpha) * cos.max(0.0);
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.slug.cmp(&b.slug))
        });
    }
    Ok(())
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < 1e-8 {
        0.0
    } else {
        dot / denom
    }
}

// ─── embedding coverage ───────────────────────────────────────────────────────

async fn compute_embedding_coverage(
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
    let table_name = format!("vec_{}", vamana::sanitize_model_key(&model));
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

// ─── compose helpers ──────────────────────────────────────────────────────────

struct ScoredTextItem {
    id: String,
    slug: String,
    name: String,
    text: String,
    score: f32,
}

async fn load_domain_by_id_or_slug(
    runtime: &KhiveRuntime,
    ns: &str,
    id_or_slug: &str,
) -> Result<Domain, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("compose domain reader", e))?;
    let id = id_or_slug.trim().to_string();
    let row = if id.parse::<Uuid>().is_ok() {
        reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_domains WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose domain by id", e))?
    } else {
        reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_domains WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose domain by slug", e))?
    };
    row.and_then(|r| domain_from_row(&r))
        .ok_or_else(|| RuntimeError::NotFound(format!("domain not found: {id:?}")))
}

async fn load_atom_by_id_or_slug(
    runtime: &KhiveRuntime,
    ns: &str,
    id_or_slug: &str,
) -> Result<Atom, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("compose atom reader", e))?;
    let id = id_or_slug.trim().to_string();
    let row = if id.parse::<Uuid>().is_ok() {
        reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_atoms WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose atom by id", e))?
    } else {
        reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_atoms WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose atom by slug", e))?
    };
    row.and_then(|r| atom_from_row(&r))
        .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {id:?}")))
}

fn parse_domain_members(domain: &Domain) -> Result<Vec<String>, RuntimeError> {
    if domain.members.is_empty() || domain.members == "[]" {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<String>>(&domain.members).map_err(|e| {
        RuntimeError::Internal(format!(
            "domain {:?} has invalid members JSON: {e}",
            domain.slug
        ))
    })
}

async fn rerank_text_items(
    runtime: &KhiveRuntime,
    query: &str,
    items: &mut [ScoredTextItem],
) -> Result<(), RuntimeError> {
    if items.is_empty() {
        return Ok(());
    }
    let texts: Vec<String> = items.iter().map(|item| item.text.clone()).collect();
    if let Some(cosines) = embed_cosine_scores(runtime, query, &texts).await {
        for (item, cos) in items.iter_mut().zip(cosines.iter()) {
            item.score = cos.max(0.0);
        }
        items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.slug.cmp(&b.slug))
        });
    }
    Ok(())
}

fn format_compose_markdown(query: &str, domains: &[Domain], atoms: &[(&Atom, f32)]) -> String {
    let mut out = String::from("# Knowledge Briefing\n\n");
    out.push_str(&format!("Query: {query}\n"));
    for (atom, score) in atoms {
        out.push_str(&format!("\n## {}\n\n", atom.name));
        out.push_str(&format!("Source: {}\n", atom.slug));
        out.push_str(&format!("Score: {:.4}\n", score));
        if let Some(ref desc) = atom.description {
            if !desc.is_empty() {
                out.push('\n');
                out.push_str(desc);
                out.push('\n');
            }
        }
        if !atom.content.is_empty() {
            out.push('\n');
            out.push_str(&atom.content);
            out.push('\n');
        }
    }
    if !domains.is_empty() {
        out.push_str("\n---\n\nDomains: ");
        let names: Vec<&str> = domains.iter().map(|d| d.name.as_str()).collect();
        out.push_str(&names.join(", "));
        out.push('\n');
    }
    out
}

// ─── embed text helper ────────────────────────────────────────────────────────

fn atom_embed_text(atom: &Atom) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    if !atom.name.is_empty() {
        parts.push(&atom.name);
    }
    if let Some(ref desc) = atom.description {
        if !desc.is_empty() {
            parts.push(desc.as_str());
        }
    }
    if !atom.content.is_empty() {
        parts.push(&atom.content);
    }
    parts.join("\n\n")
}

// ─── section helpers ──────────────────────────────────────────────────────────

#[allow(dead_code)]
fn section_from_row(row: &khive_storage::types::SqlRow) -> Option<Section> {
    let id: Uuid = row_str(row, "id")?.parse().ok()?;
    let st_str = row_str(row, "section_type")?;
    let section_type = SectionType::from_str_loose(&st_str)?;
    Some(Section {
        id,
        atom_id: row_str(row, "atom_id")?,
        namespace: row_str(row, "namespace")?,
        section_type,
        heading: row_str(row, "heading").unwrap_or_default(),
        content: row_str(row, "content").unwrap_or_default(),
        tokens: row_i64(row, "tokens").unwrap_or(0),
        sort_order: row_i64(row, "sort_order").unwrap_or(0),
        created_at: row_i64(row, "created_at").unwrap_or(0),
        updated_at: row_i64(row, "updated_at").unwrap_or(0),
    })
}

#[allow(dead_code)]
fn section_to_json(s: &Section) -> Value {
    json!({
        "id": s.id.to_string(),
        "atom_id": s.atom_id,
        "namespace": s.namespace,
        "section_type": s.section_type.as_str(),
        "heading": s.heading,
        "content": s.content,
        "tokens": s.tokens,
        "sort_order": s.sort_order,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

/// Naive token count: whitespace-split word count.
fn count_tokens(text: &str) -> i64 {
    text.split_whitespace().count() as i64
}

/// Parse a SectionUpdate's `section_type` field into a `SectionType` enum,
/// returning a descriptive error on unknown values.
fn parse_section_type(s: &str) -> Result<SectionType, RuntimeError> {
    SectionType::from_str_loose(s).ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "unknown section_type {s:?}; valid values: {}",
            SectionType::NAMES.join(", ")
        ))
    })
}

async fn resolve_atom_id(
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

impl KnowledgeHandlers {
    // ── edit ─────────────────────────────────────────────────────────────────

    /// Upsert sections for a knowledge atom without touching sibling sections.
    ///
    /// Each (atom_id, section_type) pair is upserted atomically using SQLite's
    /// `INSERT OR REPLACE` semantics backed by the UNIQUE(atom_id, section_type)
    /// constraint. Sections not named in the call are left untouched.
    pub(crate) async fn edit(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: EditParams = deser(params)?;
        if p.sections.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "sections must not be empty".into(),
            ));
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();

        // Resolve the atom (by UUID or slug).
        let atom_id = {
            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("edit atom reader", e))?;
            let id = p.id.trim().to_string();
            let row = if id.parse::<Uuid>().is_ok() {
                reader
                    .query_row(SqlStatement {
                        sql: "SELECT id FROM knowledge_atoms WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                        params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit atom lookup by id", e))?
            } else {
                reader
                    .query_row(SqlStatement {
                        sql: "SELECT id FROM knowledge_atoms WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                        params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit atom lookup by slug", e))?
            };
            row.and_then(|r| row_str(&r, "id"))
                .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {:?}", p.id)))?
        };

        let now = now_us();
        let mut upserted = 0usize;
        let mut section_results: Vec<Value> = Vec::with_capacity(p.sections.len());

        for su in &p.sections {
            let stype = parse_section_type(&su.section_type)?;
            let heading = su.heading.as_deref().unwrap_or(stype.as_str()).to_string();
            let tokens = count_tokens(&su.content);
            let sort_order = su.sort_order.unwrap_or_else(|| {
                SectionType::ALL
                    .iter()
                    .position(|&t| t == stype)
                    .unwrap_or(9) as i64
            });

            // Fetch the existing section id (and status) if it exists (for stable IDs on re-edit).
            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("edit section reader", e))?;
            let existing_section = reader
                .query_row(SqlStatement {
                    sql: "SELECT id, status FROM knowledge_sections WHERE atom_id = ?1 AND section_type = ?2 LIMIT 1".into(),
                    params: vec![
                        SqlValue::Text(atom_id.clone()),
                        SqlValue::Text(stype.as_str().to_string()),
                    ],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("edit section lookup", e))?;

            let was_verified = existing_section
                .as_ref()
                .and_then(|r| row_str(r, "status"))
                .as_deref()
                == Some("verified");
            let section_id = existing_section
                .as_ref()
                .and_then(|r| row_str(r, "id"))
                .unwrap_or_else(new_id);

            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("edit section writer", e))?;
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO knowledge_sections \
                          (id, atom_id, namespace, section_type, heading, content, tokens, sort_order, created_at, updated_at) \
                          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                          ON CONFLICT(atom_id, section_type) DO UPDATE SET \
                            heading=excluded.heading, \
                            content=excluded.content, \
                            tokens=excluded.tokens, \
                            sort_order=excluded.sort_order, \
                            embedding=NULL, \
                            updated_at=excluded.updated_at"
                        .into(),
                    params: vec![
                        SqlValue::Text(section_id.clone()),
                        SqlValue::Text(atom_id.clone()),
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(stype.as_str().to_string()),
                        SqlValue::Text(heading.clone()),
                        SqlValue::Text(su.content.clone()),
                        SqlValue::Integer(tokens),
                        SqlValue::Integer(sort_order),
                        SqlValue::Integer(now),
                        SqlValue::Integer(now),
                    ],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("edit section upsert", e))?;

            if was_verified {
                writer
                    .execute(SqlStatement {
                        sql: "UPDATE knowledge_sections SET status='reviewed' WHERE atom_id=?1 AND section_type=?2 AND status='verified'".into(),
                        params: vec![
                            SqlValue::Text(atom_id.clone()),
                            SqlValue::Text(stype.as_str().to_string()),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit section status transition", e))?;
            }

            upserted += 1;
            section_results.push(json!({
                "id": section_id,
                "atom_id": atom_id,
                "section_type": stype.as_str(),
                "heading": heading,
                "tokens": tokens,
            }));
        }

        Ok(json!({
            "atom_id": atom_id,
            "upserted": upserted,
            "sections": section_results,
        }))
    }

    // ── import ────────────────────────────────────────────────────────────────

    /// Ingest a markdown file (or directory of markdown files) into the knowledge corpus.
    ///
    /// Parses the markdown into section-typed atoms using the atlas heading normalization
    /// map in `SectionType::from_str_loose`. Each `## Heading` creates one section of
    /// the detected type; content before the first `##` heading becomes the atom's
    /// `content` field (flat body).
    ///
    /// The atom slug is derived from the file stem (lower-kebab). If an atom with that
    /// slug already exists it is updated (upsert semantics). Sections are upserted
    /// individually, so re-importing a file only changes sections whose content changed.
    pub(crate) async fn import(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ImportParams = deser(params)?;
        let path_str = p.path.trim().to_string();
        if path_str.is_empty() {
            return Err(RuntimeError::InvalidInput("path must not be empty".into()));
        }

        let chunk_strategy = p
            .chunk_strategy
            .as_deref()
            .unwrap_or("section")
            .to_ascii_lowercase();
        if !["section", "atom"].contains(&chunk_strategy.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown chunk_strategy {:?}; valid: section | atom",
                chunk_strategy
            )));
        }
        let format = p.format.as_deref().unwrap_or("atlas_md");
        if format != "atlas_md" {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown format {format:?}; only \"atlas_md\" is supported"
            )));
        }

        let md_path = std::path::Path::new(&path_str);
        if !md_path.exists() {
            return Err(RuntimeError::NotFound(format!(
                "path does not exist: {path_str:?}"
            )));
        }

        // Collect markdown files to import.
        let files: Vec<std::path::PathBuf> = if md_path.is_file() {
            vec![md_path.to_path_buf()]
        } else if md_path.is_dir() {
            let mut v = Vec::new();
            collect_md_files(md_path, &mut v);
            v
        } else {
            return Err(RuntimeError::InvalidInput(format!(
                "path is not a file or directory: {path_str:?}"
            )));
        };

        if files.is_empty() {
            return Ok(json!({
                "imported_atoms": 0,
                "imported_sections": 0,
                "files_processed": 0,
            }));
        }

        let mut imported_atoms = 0usize;
        let mut imported_sections = 0usize;

        for file in &files {
            let content = std::fs::read_to_string(file)
                .map_err(|e| RuntimeError::Internal(format!("failed to read {:?}: {e}", file)))?;

            let stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            let slug = to_slug(stem);

            let (atom_name, atom_body, sections) = parse_atlas_md(&content);
            let name = if atom_name.is_empty() {
                slug.replace('-', " ")
            } else {
                atom_name
            };

            // Upsert the atom.
            let atlas_id = extract_atlas_id(&content);
            let citation_count = sections
                .iter()
                .filter(|(stype, _, _)| *stype == SectionType::References)
                .map(|(_, _, body)| body.lines().filter(|line| !line.trim().is_empty()).count())
                .sum::<usize>();
            let source_uri = atlas_id.as_ref().map(|id| format!("atlas:{id}"));
            let source_type = if citation_count > 0 {
                "paper"
            } else {
                "imported"
            };
            let mut properties = serde_json::Map::new();
            if let Some(ref id) = atlas_id {
                properties.insert("atlas_id".to_string(), Value::String(id.clone()));
            }

            let upsert_params = serde_json::json!({
                "atoms": [{
                    "slug": slug,
                    "name": name,
                    "content": atom_body,
                    "properties": Value::Object(properties),
                    "source_uri": source_uri,
                    "source_type": source_type,
                }]
            });
            KnowledgeHandlers::upsert_atoms(runtime, token, upsert_params).await?;
            imported_atoms += 1;

            // Upsert sections (if chunk_strategy == "section").
            if chunk_strategy == "section" && !sections.is_empty() {
                let section_updates: Vec<Value> = sections
                    .iter()
                    .map(|(stype, heading, body)| {
                        json!({
                            "section_type": stype.as_str(),
                            "heading": heading,
                            "content": body,
                        })
                    })
                    .collect();
                let edit_params = json!({
                    "id": slug,
                    "sections": section_updates,
                });
                let result = KnowledgeHandlers::edit(runtime, token, edit_params).await?;
                if let Some(n) = result.get("upserted").and_then(|v| v.as_u64()) {
                    imported_sections += n as usize;
                }
            }
        }

        Ok(json!({
            "imported_atoms": imported_atoms,
            "imported_sections": imported_sections,
            "files_processed": files.len(),
        }))
    }

    // ── challenge ─────────────────────────────────────────────────────────────

    pub(crate) async fn challenge(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ChallengeParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();

        let atom_id = resolve_atom_id(runtime, &ns, &p.atom_id).await?;
        let stype = parse_section_type(&p.section_type)?;

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| sql_err("challenge writer", e))?;

        let affected = writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_sections SET status='disputed' WHERE atom_id=?1 AND section_type=?2 AND status NOT IN ('disputed','deprecated')".into(),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(stype.as_str().to_string()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("challenge section status", e))?;

        if affected == 0 {
            return Err(RuntimeError::InvalidInput(
                "section not found, already disputed, or deprecated".into(),
            ));
        }

        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{}'),'$.dispute_count',coalesce(json_extract(properties,'$.dispute_count'),0)+1) WHERE id=?1 AND namespace=?2".into(),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(ns.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("challenge dispute_count increment", e))?;

        Ok(json!({
            "atom_id": atom_id,
            "section_type": stype.as_str(),
            "reason": p.reason,
        }))
    }

    // ── adjudicate ────────────────────────────────────────────────────────────

    pub(crate) async fn adjudicate(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: AdjudicateParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();

        let resolution = p.resolution.trim().to_ascii_lowercase();
        if resolution != "accept" && resolution != "reject" {
            return Err(RuntimeError::InvalidInput(
                "resolution must be \"accept\" or \"reject\"".into(),
            ));
        }

        let atom_id = resolve_atom_id(runtime, &ns, &p.atom_id).await?;
        let stype = parse_section_type(&p.section_type)?;

        let new_status = if resolution == "accept" {
            "verified"
        } else {
            "reviewed"
        };

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| sql_err("adjudicate writer", e))?;

        let affected = writer
            .execute(SqlStatement {
                sql: format!(
                    "UPDATE knowledge_sections SET status='{new_status}' WHERE atom_id=?1 AND section_type=?2 AND status='disputed'"
                ),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(stype.as_str().to_string()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("adjudicate section status", e))?;

        if affected == 0 {
            return Err(RuntimeError::InvalidInput(
                "section not found or not in disputed state".into(),
            ));
        }

        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{}'),'$.dispute_count',CASE WHEN coalesce(json_extract(properties,'$.dispute_count'),0) > 0 THEN coalesce(json_extract(properties,'$.dispute_count'),0)-1 ELSE 0 END) WHERE id=?1 AND namespace=?2".into(),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(ns.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("adjudicate dispute_count decrement", e))?;

        Ok(json!({
            "atom_id": atom_id,
            "section_type": stype.as_str(),
            "resolution": resolution,
            "new_status": new_status,
        }))
    }
}

// ─── markdown parsing helpers ─────────────────────────────────────────────────

/// Collect all `.md` files recursively under `dir`.
fn collect_md_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_md_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                out.push(path);
            }
        }
    }
}

/// Convert a file stem to a URL-safe slug.
///
/// Lower-cases the stem and replaces spaces and underscores with hyphens,
/// keeping alphanumeric characters and hyphens.
fn to_slug(stem: &str) -> String {
    stem.to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Parse atlas-format markdown into (title, pre-section body, sections).
///
/// Atlas markdown structure:
/// ```text
/// # Title
///
/// Optional introductory text that becomes the atom body.
///
/// ## Section Heading
///
/// Section content...
///
/// ## Another Section
///
/// More content...
/// ```
///
/// Returns `(name, atom_body, Vec<(SectionType, heading, content)>)`.
/// `name` is the `# Title` text (empty if absent).
/// `atom_body` is text before the first `##` heading.
/// Each tuple in the vec is `(SectionType, heading_text, body_text)`.
/// Headings that don't map to a `SectionType` are classified as `Other`.
fn extract_atlas_id(content: &str) -> Option<String> {
    content.lines().take(32).find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("atlas_id:")
            .or_else(|| trimmed.strip_prefix("atlas-id:"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn parse_atlas_md(content: &str) -> (String, String, Vec<(SectionType, String, String)>) {
    let mut name = String::new();
    let mut pre_body = String::new();
    let mut sections: Vec<(SectionType, String, String)> = Vec::new();

    // State: None = pre-first-heading, Some(idx) = inside section at index
    let mut in_pre = true;
    let mut current_heading: Option<(SectionType, String)> = None;
    let mut current_body = String::new();

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            if name.is_empty() {
                // Document title.
                name = rest.trim().to_string();
                in_pre = true;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("## ") {
            // Save the previous section (if any).
            if let Some((stype, heading)) = current_heading.take() {
                sections.push((stype, heading, current_body.trim_end().to_string()));
                current_body.clear();
            } else if in_pre {
                // The pre-section body ends here.
                pre_body = current_body.trim_end().to_string();
                current_body.clear();
                in_pre = false;
            }
            let heading_text = rest.trim().to_string();
            let stype = SectionType::from_str_loose(&heading_text).unwrap_or(SectionType::Other);
            current_heading = Some((stype, heading_text));
            continue;
        }
        // Accumulate content.
        current_body.push_str(line);
        current_body.push('\n');
    }

    // Flush the last section or pre-body.
    if let Some((stype, heading)) = current_heading {
        sections.push((stype, heading, current_body.trim_end().to_string()));
    } else {
        pre_body = current_body.trim_end().to_string();
    }

    (name, pre_body, sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    // #523: RRF normalization must be monotonic and bounded.
    #[test]
    fn normalize_rrf_score_is_bounded_and_monotonic() {
        let k = RRF_K;
        // Single source: theoretical max = 1/(k+1).
        let max_single = 1.0f32 / (k as f32 + 1.0);
        let scores_single = [
            max_single * 0.25,
            max_single * 0.5,
            max_single,
            max_single * 1.5,
        ];
        let normed_single: Vec<f32> = scores_single
            .iter()
            .map(|&r| normalize_rrf_score(r, 1, k))
            .collect();
        // All in [0,1].
        for &s in &normed_single {
            assert!((0.0..=1.0).contains(&s), "score out of range: {s}");
        }
        // Monotonic for values under the max (clamping only at theoretical max).
        assert!(normed_single[0] < normed_single[1]);
        assert!(normed_single[1] < normed_single[2]);
        // Clamped at 1.0 for values above the theoretical max.
        assert_eq!(normed_single[3], 1.0);

        // Two sources: theoretical max = 2/(k+1).
        let max_two = 2.0f32 / (k as f32 + 1.0);
        let scores_two = [max_two * 0.25, max_two * 0.75, max_two, max_two * 2.0];
        let normed_two: Vec<f32> = scores_two
            .iter()
            .map(|&r| normalize_rrf_score(r, 2, k))
            .collect();
        for &s in &normed_two {
            assert!((0.0..=1.0).contains(&s), "score out of range: {s}");
        }
        assert!(normed_two[0] < normed_two[1]);
        assert!(normed_two[1] < normed_two[2]);
        assert_eq!(normed_two[3], 1.0);

        // Raw order equals normalized order for unequal inputs (no rank inversion).
        let raw = [0.001f32, 0.005, 0.010, 0.015];
        let normed: Vec<f32> = raw.iter().map(|&r| normalize_rrf_score(r, 1, k)).collect();
        let raw_order: Vec<usize> = {
            let mut idx: Vec<usize> = (0..raw.len()).collect();
            idx.sort_by(|&a, &b| raw[b].partial_cmp(&raw[a]).unwrap());
            idx
        };
        let norm_order: Vec<usize> = {
            let mut idx: Vec<usize> = (0..normed.len()).collect();
            idx.sort_by(|&a, &b| normed[b].partial_cmp(&normed[a]).unwrap());
            idx
        };
        assert_eq!(
            raw_order, norm_order,
            "normalization must not invert ranking"
        );
    }

    #[test]
    fn normalize_rrf_score_zero_source_count_returns_zero() {
        assert_eq!(normalize_rrf_score(0.5, 0, RRF_K), 0.0);
    }
}
