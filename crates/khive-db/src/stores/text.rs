//! FTS5-backed `TextSearch` implementation.
//!
//! Each `Fts5TextSearch` manages a single FTS5 virtual table for full-text
//! search. The table stores document metadata alongside the indexed text
//! columns (`title` and `body`), with non-searchable columns marked
//! `UNINDEXED`.
//!
//! # FTS5 table layout
//!
//! ```sql
//! CREATE VIRTUAL TABLE fts_{key} USING fts5(
//!     subject_id UNINDEXED,
//!     kind UNINDEXED,
//!     title,
//!     body,
//!     tags UNINDEXED,
//!     namespace UNINDEXED,
//!     metadata UNINDEXED,
//!     updated_at UNINDEXED
//! );
//! ```
//!
//! Only `title` and `body` are full-text indexed. The remaining columns are
//! stored for retrieval and filtering but do not participate in FTS ranking.
//!
//! # Connection strategy
//!
//! Follows the same dual-mode pattern as `SqliteVecStore`:
//! - **File-backed**: Opens standalone connections per operation.
//! - **In-memory**: Acquires pool connections via `spawn_blocking`.
//!
//! # Score normalization
//!
//! FTS5 `rank` values are negative (more negative = more relevant). We
//! normalize within each result set so scores span `(0.05, 1.0]`, preserving
//! relative ordering across all tokenizers (including trigram, which produces
//! a narrow absolute range that the old `1/(1+|rank|)` formula collapsed into
//! near-uniform noise). The best hit in a result set receives score `1.0`;
//! the worst receives `0.05`. When all hits have the same rank (single hit or
//! degenerate match), score is `1.0`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, TextDocument, TextFilter, TextIndexStats, TextQueryMode,
    TextSearchHit, TextSearchRequest,
};
use khive_storage::StorageCapability;
use khive_storage::TextSearch;
use khive_types::SubstrateKind;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

/// Ensure the FTS5 virtual table for `table_key` exists.
///
/// Used in tests to set up an in-memory FTS5 table without the full `StorageBackend`.
#[cfg(test)]
pub(crate) fn ensure_fts5_schema(
    conn: &rusqlite::Connection,
    table_key: &str,
) -> Result<(), rusqlite::Error> {
    let table_name = format!("fts_{}", table_key);
    let ddl = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {} USING fts5(\
         subject_id UNINDEXED, \
         kind UNINDEXED, \
         title, \
         body, \
         tags UNINDEXED, \
         namespace UNINDEXED, \
         metadata UNINDEXED, \
         updated_at UNINDEXED\
         )",
        table_name
    );
    conn.execute_batch(&ddl)
}

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Text, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Text, op, e)
}

/// A TextSearch backed by SQLite FTS5 virtual tables.
///
/// Each instance manages one table: `fts_{table_key}`. Documents are stored
/// with their metadata in UNINDEXED columns; only `title` and `body` are
/// full-text indexed.
pub struct Fts5TextSearch {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    table_name: String,
}

impl Fts5TextSearch {
    /// Create a new FTS5 text search instance.
    ///
    /// The FTS5 virtual table must already exist (created by `StorageBackend::text()`).
    pub(crate) fn new(pool: Arc<ConnectionPool>, is_file_backed: bool, table_key: String) -> Self {
        let table_name = format!("fts_{}", table_key);
        Self {
            pool,
            is_file_backed,
            table_name,
        }
    }

    fn open_standalone_writer(&self) -> Result<rusqlite::Connection, StorageError> {
        let config = self.pool.config();
        let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
            operation: "fts_writer".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_fts_writer"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_fts_writer"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_fts_writer"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_fts_writer"))?;

        Ok(conn)
    }

    fn open_standalone_reader(&self) -> Result<rusqlite::Connection, StorageError> {
        let config = self.pool.config();
        let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
            operation: "fts_reader".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_fts_reader"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_fts_reader"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_fts_reader"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_fts_reader"))?;

        Ok(conn)
    }

    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if self.is_file_backed {
            let conn = self.open_standalone_writer()?;
            tokio::task::spawn_blocking(move || f(&conn).map_err(|e| map_err(e, op)))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Text, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Text, op, e))?
        }
    }

    async fn with_reader<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if self.is_file_backed {
            let conn = self.open_standalone_reader()?;
            tokio::task::spawn_blocking(move || f(&conn).map_err(|e| map_err(e, op)))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Text, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Text, op, e))?
        }
    }
}

// -- Helper functions --

fn tags_to_json(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string())
}

fn tags_from_json(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn dt_to_micros(dt: &DateTime<Utc>) -> i64 {
    dt.timestamp_micros()
}

fn micros_to_dt(micros: i64) -> DateTime<Utc> {
    Utc.timestamp_micros(micros)
        .single()
        .unwrap_or_else(Utc::now)
}

/// Sanitize an FTS5 query string to prevent driver errors from special chars.
///
/// Two-pass approach:
/// 1. **Replace** grouping/separator chars with spaces so adjacent tokens are
///    not merged. This prevents `NEAR(smile,5)` from becoming `NEARsmile5`.
///    Chars replaced with space: `(`, `)`, `,`
/// 2. **Remove** remaining FTS5 operator characters (H1: `~`, `!` added):
///    `*`, `"`, `+`, `-`, `:`, `^`, `.`, `~`, `!`, `\0`, control characters
///
/// After character processing, split on whitespace and remove FTS5 keyword
/// tokens: AND, OR, NOT, NEAR.
///
/// For Phrase mode, the caller wraps the result in double quotes.
fn sanitize_fts5_query(query: &str) -> String {
    // Pass 1: replace grouping/separator chars with spaces to isolate tokens.
    // Colon is included here (not in Pass 2) so that "tenant:isolation" becomes
    // "tenant isolation" rather than "tenantisolation".
    let spaced: String = query
        .chars()
        .map(|c| {
            if matches!(c, '(' | ')' | ',' | ':') {
                ' '
            } else {
                c
            }
        })
        .collect();

    // Pass 2: remove remaining FTS5 special chars and control characters.
    // Single quote (apostrophe) is included because FTS5 Plain-mode queries treat
    // it as a string-literal delimiter causing "syntax error near '''".
    let sanitized: String = spaced
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '*' | '"' | '\'' | '+' | '-' | '^' | '.' | '~' | '!' | '\0'
            ) && !c.is_control()
        })
        .collect();

    // Pass 3: filter FTS5 operator keywords.
    sanitized
        .split_whitespace()
        .filter(|t| {
            !matches!(
                t.to_ascii_uppercase().as_str(),
                "AND" | "OR" | "NOT" | "NEAR"
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build a WHERE clause fragment and params for a `TextFilter`.
///
/// Returns `(clause, params)` where clause is empty if no filters are active.
/// Parameter indices start at `?{start_idx}`.
fn build_filter_clause(
    filter: &TextFilter,
    table: &str,
    start_idx: usize,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = start_idx;

    if !filter.ids.is_empty() {
        let placeholders: Vec<String> = filter
            .ids
            .iter()
            .map(|_| {
                let p = format!("?{}", idx);
                idx += 1;
                p
            })
            .collect();
        conditions.push(format!(
            "{}.subject_id IN ({})",
            table,
            placeholders.join(", ")
        ));
        for id in &filter.ids {
            params.push(Box::new(id.to_string()));
        }
    }

    if !filter.kinds.is_empty() {
        let placeholders: Vec<String> = filter
            .kinds
            .iter()
            .map(|_| {
                let p = format!("?{}", idx);
                idx += 1;
                p
            })
            .collect();
        conditions.push(format!("{}.kind IN ({})", table, placeholders.join(", ")));
        for kind in &filter.kinds {
            params.push(Box::new(kind.to_string()));
        }
    }

    if !filter.namespaces.is_empty() {
        let placeholders: Vec<String> = filter
            .namespaces
            .iter()
            .map(|_| {
                let p = format!("?{}", idx);
                idx += 1;
                p
            })
            .collect();
        conditions.push(format!(
            "{}.namespace IN ({})",
            table,
            placeholders.join(", ")
        ));
        for ns in &filter.namespaces {
            params.push(Box::new(ns.clone()));
        }
    }

    if conditions.is_empty() {
        (String::new(), params)
    } else {
        (format!(" AND {}", conditions.join(" AND ")), params)
    }
}

#[async_trait]
impl TextSearch for Fts5TextSearch {
    async fn upsert_document(&self, document: TextDocument) -> Result<(), StorageError> {
        let table = self.table_name.clone();
        let namespace = document.namespace.clone();

        self.with_writer("fts_upsert", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;

            let del_sql = format!(
                "DELETE FROM {} WHERE namespace = ?1 AND subject_id = ?2",
                table
            );
            if let Err(e) = conn.execute(
                &del_sql,
                rusqlite::params![&namespace, document.subject_id.to_string()],
            ) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }

            let ins_sql = format!(
                "INSERT INTO {} \
                 (subject_id, kind, title, body, tags, namespace, metadata, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                table
            );
            let tags_json = tags_to_json(&document.tags);
            let metadata_json: Option<String> = document.metadata.as_ref().map(|v| v.to_string());

            if let Err(e) = conn.execute(
                &ins_sql,
                rusqlite::params![
                    document.subject_id.to_string(),
                    document.kind.to_string(),
                    document.title.as_deref().unwrap_or(""),
                    document.body,
                    tags_json,
                    &namespace,
                    metadata_json,
                    dt_to_micros(&document.updated_at),
                ],
            ) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }

            conn.execute_batch("COMMIT")?;
            Ok(())
        })
        .await
    }

    async fn upsert_documents(
        &self,
        documents: Vec<TextDocument>,
    ) -> Result<BatchWriteSummary, StorageError> {
        let table = self.table_name.clone();
        let attempted = documents.len() as u64;

        self.with_writer("fts_upsert_batch", move |conn| {
            let del_sql = format!(
                "DELETE FROM {} WHERE namespace = ?1 AND subject_id = ?2",
                table
            );
            let ins_sql = format!(
                "INSERT INTO {} \
                 (subject_id, kind, title, body, tags, namespace, metadata, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                table
            );

            conn.execute_batch("BEGIN IMMEDIATE")?;
            let mut affected = 0u64;
            let mut failed = 0u64;

            for doc in &documents {
                conn.execute_batch("SAVEPOINT fts_upsert_doc")?;
                let id_str = doc.subject_id.to_string();
                let namespace = &doc.namespace;
                let result = (|| {
                    conn.execute(&del_sql, rusqlite::params![namespace, &id_str])?;

                    let tags_json = tags_to_json(&doc.tags);
                    let metadata_json: Option<String> =
                        doc.metadata.as_ref().map(|v| v.to_string());

                    conn.execute(
                        &ins_sql,
                        rusqlite::params![
                            &id_str,
                            &doc.kind.to_string(),
                            doc.title.as_deref().unwrap_or(""),
                            &doc.body,
                            &tags_json,
                            namespace,
                            &metadata_json,
                            dt_to_micros(&doc.updated_at),
                        ],
                    )?;
                    Ok::<(), rusqlite::Error>(())
                })();

                match result {
                    Ok(()) => {
                        conn.execute_batch("RELEASE SAVEPOINT fts_upsert_doc")?;
                        affected += 1;
                    }
                    Err(_) => {
                        let _ = conn.execute_batch("ROLLBACK TO SAVEPOINT fts_upsert_doc");
                        let _ = conn.execute_batch("RELEASE SAVEPOINT fts_upsert_doc");
                        failed += 1;
                    }
                }
            }

            conn.execute_batch("COMMIT")?;

            Ok(BatchWriteSummary {
                attempted,
                affected,
                failed,
                first_error: String::new(),
            })
        })
        .await
    }

    async fn delete_document(
        &self,
        namespace: &str,
        subject_id: Uuid,
    ) -> Result<bool, StorageError> {
        let namespace = namespace.to_string();
        let table = self.table_name.clone();

        self.with_writer("fts_delete", move |conn| {
            let sql = format!(
                "DELETE FROM {} WHERE namespace = ?1 AND subject_id = ?2",
                table
            );
            let deleted =
                conn.execute(&sql, rusqlite::params![namespace, subject_id.to_string()])?;
            Ok(deleted > 0)
        })
        .await
    }

    async fn get_document(
        &self,
        namespace: &str,
        subject_id: Uuid,
    ) -> Result<Option<TextDocument>, StorageError> {
        let namespace = namespace.to_string();
        let table = self.table_name.clone();

        self.with_reader("fts_get", move |conn| {
            let sql = format!(
                "SELECT subject_id, kind, title, body, tags, namespace, metadata, updated_at \
                 FROM {} WHERE namespace = ?1 AND subject_id = ?2",
                table
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params![namespace, subject_id.to_string()])?;

            match rows.next()? {
                Some(row) => {
                    let id_str: String = row.get(0)?;
                    let kind_str: String = row.get(1)?;
                    let title: String = row.get(2)?;
                    let body: String = row.get(3)?;
                    let tags_json: String = row.get(4)?;
                    let ns: String = row.get(5)?;
                    let metadata_json: Option<String> = row.get(6)?;
                    let updated_at_micros: i64 = row.get(7)?;

                    let sid = Uuid::parse_str(&id_str).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;

                    let kind = kind_str.parse::<SubstrateKind>().map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;

                    Ok(Some(TextDocument {
                        subject_id: sid,
                        kind,
                        title: if title.is_empty() { None } else { Some(title) },
                        body,
                        tags: tags_from_json(&tags_json),
                        namespace: ns,
                        metadata: metadata_json.and_then(|s| serde_json::from_str(&s).ok()),
                        updated_at: micros_to_dt(updated_at_micros),
                    }))
                }
                None => Ok(None),
            }
        })
        .await
    }

    async fn search(&self, request: TextSearchRequest) -> Result<Vec<TextSearchHit>, StorageError> {
        let table = self.table_name.clone();

        self.with_reader("fts_search", move |conn| {
            let match_expr = match request.mode {
                TextQueryMode::AnyTerm => {
                    // Sanitize each token independently so the OR joiner isn't
                    // stripped by sanitize_fts5_query's keyword filter.
                    let parts: Vec<String> = request
                        .query
                        .split_whitespace()
                        .map(sanitize_fts5_query)
                        .filter(|t| !t.is_empty())
                        .collect();
                    if parts.is_empty() {
                        return Ok(Vec::new());
                    }
                    parts.join(" OR ")
                }
                _ => {
                    let sanitized = sanitize_fts5_query(&request.query);
                    if sanitized.is_empty() {
                        return Ok(Vec::new());
                    }
                    match request.mode {
                        TextQueryMode::Phrase => format!("\"{}\"", sanitized),
                        TextQueryMode::Plain => sanitized,
                        TextQueryMode::AnyTerm => unreachable!(),
                    }
                }
            };

            // Snippet column index 3 = body in the FTS5 schema.
            // snippet_chars == 0 is the sentinel for "no snippet" — skip the
            // snippet(...) call entirely and return NULL instead.  This avoids
            // the ~12ms BM25 snippet computation on the hot recall path where
            // snippets are unused.  Callers that need snippets (diagnostics) pass
            // snippet_chars > 0 and get the same behaviour as before.
            let snippet_expr = if request.snippet_chars == 0 {
                "NULL AS snippet".to_string()
            } else {
                let chars = i32::try_from(request.snippet_chars).unwrap_or(i32::MAX);
                format!("snippet({table}, 3, '', '', '...', {chars})")
            };

            let (filter_clause, filter_params) = if let Some(ref filter) = request.filter {
                build_filter_clause(filter, &table, 3)
            } else {
                (String::new(), Vec::new())
            };

            let sql = format!(
                "SELECT subject_id, rank, title, {snippet_expr} \
                 FROM {table} WHERE {table} MATCH ?1{filter_clause} \
                 ORDER BY rank LIMIT ?2",
            );

            let mut stmt = conn.prepare(&sql)?;
            stmt.raw_bind_parameter(1, &match_expr)?;
            stmt.raw_bind_parameter(2, request.top_k as i64)?;

            for (i, param) in filter_params.iter().enumerate() {
                param
                    .to_sql()
                    .map(|val| stmt.raw_bind_parameter(3 + i, val))
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))??;
            }

            let mut hits = Vec::new();
            let mut rows = stmt.raw_query();
            let mut rank_idx = 0u32;

            while let Some(row) = rows.next()? {
                let id_str: String = row.get(0)?;
                let fts_rank: f64 = row.get(1)?;
                let title: String = row.get(2)?;
                let snippet: Option<String> = row.get(3)?;

                let subject_id = Uuid::parse_str(&id_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

                rank_idx += 1;
                hits.push((subject_id, fts_rank, rank_idx, title, snippet));
            }

            // Normalize scores within the result set to (0.05, 1.0].
            // Best rank (most negative) maps to 1.0, worst to 0.05.
            let min_rank = hits.iter().map(|h| h.1).fold(f64::INFINITY, f64::min);
            let max_rank = hits.iter().map(|h| h.1).fold(f64::NEG_INFINITY, f64::max);
            let range = max_rank - min_rank;

            let results = hits
                .into_iter()
                .map(|(subject_id, raw_rank, rank, title, snippet)| {
                    let score = if range.abs() < 1e-12 {
                        1.0
                    } else {
                        let t = (max_rank - raw_rank) / range;
                        0.05 + 0.95 * t
                    };
                    TextSearchHit {
                        subject_id,
                        score: DeterministicScore::from_f64(score),
                        rank,
                        title: if title.is_empty() { None } else { Some(title) },
                        snippet: snippet.filter(|s| !s.is_empty()),
                    }
                })
                .collect();

            Ok(results)
        })
        .await
    }

    async fn count(&self, filter: TextFilter) -> Result<u64, StorageError> {
        let table = self.table_name.clone();

        self.with_reader("fts_count", move |conn| {
            let (filter_clause, filter_params) = build_filter_clause(&filter, &table, 1);

            let sql = if filter_clause.is_empty() {
                format!("SELECT COUNT(*) FROM {}", table)
            } else {
                let where_part = filter_clause.trim_start_matches(" AND ");
                format!("SELECT COUNT(*) FROM {} WHERE {}", table, where_part)
            };

            let mut stmt = conn.prepare(&sql)?;

            for (i, param) in filter_params.iter().enumerate() {
                param
                    .to_sql()
                    .map(|val| stmt.raw_bind_parameter(1 + i, val))
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))??;
            }

            let mut rows = stmt.raw_query();
            match rows.next()? {
                Some(row) => {
                    let count: i64 = row.get(0)?;
                    Ok(count as u64)
                }
                None => Ok(0),
            }
        })
        .await
    }

    async fn stats(&self) -> Result<TextIndexStats, StorageError> {
        let table = self.table_name.clone();

        self.with_reader("fts_stats", move |conn| {
            let sql = format!("SELECT COUNT(*) FROM {}", table);
            let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;

            Ok(TextIndexStats {
                document_count: count as u64,
                needs_rebuild: false,
                last_rebuild_at: None,
            })
        })
        .await
    }

    async fn rebuild(&self, _scope: IndexRebuildScope) -> Result<TextIndexStats, StorageError> {
        let table = self.table_name.clone();

        self.with_writer("fts_rebuild", move |conn| {
            // FTS5 rebuild command: repopulates the internal index structures.
            let sql = format!("INSERT INTO {}({}) VALUES('rebuild')", table, table);
            conn.execute(&sql, [])?;

            let count_sql = format!("SELECT COUNT(*) FROM {}", table);
            let count: i64 = conn.query_row(&count_sql, [], |row| row.get(0))?;

            Ok(TextIndexStats {
                document_count: count as u64,
                needs_rebuild: false,
                last_rebuild_at: Some(Utc::now()),
            })
        })
        .await
    }
}

impl Fts5TextSearch {
    /// Move all FTS5 documents from `old_namespace` to `new_namespace` in a
    /// single transaction.
    ///
    /// FTS5 virtual tables do not support updating indexed columns (`title`,
    /// `body`) via UPDATE. The correct approach is read-then-delete-then-reinsert.
    ///
    /// Callers must invoke this after any SQL-level namespace change on the
    /// backing entity table so that FTS5 keyword search stays consistent with
    /// the entity store.
    #[allow(dead_code)]
    pub(crate) async fn rename_namespace(
        &self,
        old_namespace: &str,
        new_namespace: &str,
    ) -> Result<u64, StorageError> {
        if old_namespace == new_namespace {
            return Ok(0);
        }
        let table = self.table_name.clone();
        let old_ns = old_namespace.to_string();
        let new_ns = new_namespace.to_string();

        self.with_writer("fts_rename_namespace", move |conn| {
            let sel_sql = format!(
                "SELECT subject_id, kind, title, body, tags, metadata, updated_at \
                 FROM {} WHERE namespace = ?1",
                table
            );
            struct Row {
                subject_id: String,
                kind: String,
                title: String,
                body: String,
                tags: String,
                metadata: Option<String>,
                updated_at: i64,
            }
            let rows: Vec<Row> = {
                let mut stmt = conn.prepare(&sel_sql)?;
                let iter = stmt.query_map(rusqlite::params![&old_ns], |row| {
                    Ok(Row {
                        subject_id: row.get(0)?,
                        kind: row.get(1)?,
                        title: row.get(2)?,
                        body: row.get(3)?,
                        tags: row.get(4)?,
                        metadata: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                })?;
                iter.collect::<Result<Vec<_>, _>>()?
            };
            let moved = rows.len() as u64;
            if moved == 0 {
                return Ok(0u64);
            }

            conn.execute_batch("BEGIN IMMEDIATE")?;

            let del_sql = format!("DELETE FROM {} WHERE namespace = ?1", table);
            if let Err(e) = conn.execute(&del_sql, rusqlite::params![&old_ns]) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }

            let ins_sql = format!(
                "INSERT INTO {} \
                 (subject_id, kind, title, body, tags, namespace, metadata, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                table
            );
            for row in &rows {
                if let Err(e) = conn.execute(
                    &ins_sql,
                    rusqlite::params![
                        row.subject_id,
                        row.kind,
                        row.title,
                        row.body,
                        row.tags,
                        &new_ns,
                        row.metadata,
                        row.updated_at,
                    ],
                ) {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            }

            conn.execute_batch("COMMIT")?;
            Ok(moved)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;

    fn setup_memory_store(table_key: &str) -> Fts5TextSearch {
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());

        {
            let writer = pool.writer().unwrap();
            ensure_fts5_schema(writer.conn(), table_key).unwrap();
        }

        Fts5TextSearch::new(pool, false, table_key.to_string())
    }

    fn make_document(subject_id: Uuid, title: &str, body: &str) -> TextDocument {
        TextDocument {
            subject_id,
            kind: SubstrateKind::Note,
            title: if title.is_empty() {
                None
            } else {
                Some(title.to_string())
            },
            body: body.to_string(),
            tags: vec![],
            namespace: "test_ns".to_string(),
            metadata: None,
            updated_at: Utc::now(),
        }
    }

    fn ns_filter(namespace: &str) -> TextFilter {
        TextFilter {
            namespaces: vec![namespace.to_string()],
            ..TextFilter::default()
        }
    }

    #[tokio::test]
    async fn test_upsert_and_search() {
        let store = setup_memory_store("upsert_search");

        let id = Uuid::new_v4();
        let doc = TextDocument {
            subject_id: id,
            kind: SubstrateKind::Entity,
            title: Some("Rust Programming".to_string()),
            body: "Rust is a systems programming language focused on safety and performance."
                .to_string(),
            tags: vec!["rust".to_string(), "programming".to_string()],
            namespace: "tech".to_string(),
            metadata: None,
            updated_at: Utc::now(),
        };

        store.upsert_document(doc).await.unwrap();

        let hits = store
            .search(TextSearchRequest {
                query: "Rust programming".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("tech")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject_id, id);
        assert_eq!(hits[0].rank, 1);
        assert!(hits[0].score.to_f64() > 0.0);
        assert!(hits[0].title.is_some());
    }

    #[tokio::test]
    async fn test_phrase_search() {
        let store = setup_memory_store("phrase");

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        store
            .upsert_document(make_document(
                id1,
                "Animals",
                "The quick brown fox jumps over the lazy dog.",
            ))
            .await
            .unwrap();

        store
            .upsert_document(make_document(
                id2,
                "Colors",
                "The brown paint was quick to dry, unlike the fox.",
            ))
            .await
            .unwrap();

        let hits = store
            .search(TextSearchRequest {
                query: "quick brown fox".to_string(),
                mode: TextQueryMode::Phrase,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject_id, id1);

        let hits = store
            .search(TextSearchRequest {
                query: "quick brown fox".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();

        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn test_delete_document() {
        let store = setup_memory_store("delete");

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        store
            .upsert_document(make_document(id1, "Doc One", "First document content."))
            .await
            .unwrap();
        store
            .upsert_document(make_document(id2, "Doc Two", "Second document content."))
            .await
            .unwrap();

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.document_count, 2);

        let deleted = store.delete_document("test_ns", id1).await.unwrap();
        assert!(deleted);

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.document_count, 1);

        let deleted_again = store.delete_document("test_ns", id1).await.unwrap();
        assert!(!deleted_again);

        let doc = store.get_document("test_ns", id2).await.unwrap();
        assert!(doc.is_some());

        let doc = store.get_document("test_ns", id1).await.unwrap();
        assert!(doc.is_none());
    }

    #[tokio::test]
    async fn test_count_with_filter() {
        let store = setup_memory_store("count_filter");
        let ns = "test_ns".to_string();

        for i in 0..5 {
            let kind = if i % 2 == 0 {
                SubstrateKind::Entity
            } else {
                SubstrateKind::Note
            };
            let doc = TextDocument {
                subject_id: Uuid::new_v4(),
                kind,
                title: Some(format!("Doc {}", i)),
                body: format!("Content for document number {}", i),
                tags: vec![],
                namespace: ns.clone(),
                metadata: None,
                updated_at: Utc::now(),
            };
            store.upsert_document(doc).await.unwrap();
        }

        let total = store
            .count(TextFilter {
                namespaces: vec![ns.clone()],
                ..TextFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(total, 5);

        let entities = store
            .count(TextFilter {
                namespaces: vec![ns.clone()],
                kinds: vec![SubstrateKind::Entity],
                ..TextFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(entities, 3);

        let notes = store
            .count(TextFilter {
                namespaces: vec![ns.clone()],
                kinds: vec![SubstrateKind::Note],
                ..TextFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(notes, 2);
    }

    #[tokio::test]
    async fn test_get_document_roundtrip() {
        let store = setup_memory_store("get_roundtrip");

        let id = Uuid::new_v4();
        let original = TextDocument {
            subject_id: id,
            kind: SubstrateKind::Note,
            title: Some("Important Memo".to_string()),
            body: "This memo contains critical information.".to_string(),
            tags: vec!["important".to_string(), "memo".to_string()],
            namespace: "work".to_string(),
            metadata: Some(serde_json::json!({"priority": "high"})),
            updated_at: Utc::now(),
        };

        store.upsert_document(original.clone()).await.unwrap();

        let retrieved = store.get_document("work", id).await.unwrap().unwrap();
        assert_eq!(retrieved.subject_id, id);
        assert_eq!(retrieved.kind, SubstrateKind::Note);
        assert_eq!(retrieved.title, Some("Important Memo".to_string()));
        assert_eq!(retrieved.body, "This memo contains critical information.");
        assert_eq!(retrieved.tags, vec!["important", "memo"]);
        assert_eq!(retrieved.namespace, "work");
    }

    #[tokio::test]
    async fn test_upsert_replaces_existing() {
        let store = setup_memory_store("replace");

        let id = Uuid::new_v4();
        store
            .upsert_document(make_document(id, "Original", "Original body text."))
            .await
            .unwrap();

        store
            .upsert_document(make_document(id, "Updated", "Updated body text."))
            .await
            .unwrap();

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.document_count, 1);

        let doc = store.get_document("test_ns", id).await.unwrap().unwrap();
        assert_eq!(doc.title, Some("Updated".to_string()));
        assert_eq!(doc.body, "Updated body text.");
    }

    #[tokio::test]
    async fn test_batch_upsert() {
        let store = setup_memory_store("batch");

        let docs: Vec<TextDocument> = (0..50)
            .map(|i| TextDocument {
                subject_id: Uuid::new_v4(),
                kind: SubstrateKind::Entity,
                title: Some(format!("Item {}", i)),
                body: format!("This is the body content for item number {}", i),
                tags: vec![format!("tag_{}", i % 5)],
                namespace: "batch_ns".to_string(),
                metadata: None,
                updated_at: Utc::now(),
            })
            .collect();

        let summary = store.upsert_documents(docs).await.unwrap();
        assert_eq!(summary.attempted, 50);
        assert_eq!(summary.affected, 50);
        assert_eq!(summary.failed, 0);

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.document_count, 50);
    }

    #[tokio::test]
    async fn test_empty_search() {
        let store = setup_memory_store("empty");

        let hits = store
            .search(TextSearchRequest {
                query: "nonexistent".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();

        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn test_rebuild() {
        let store = setup_memory_store("rebuild");

        store
            .upsert_document(make_document(
                Uuid::new_v4(),
                "Test",
                "Test document for rebuild.",
            ))
            .await
            .unwrap();

        let stats = store.rebuild(IndexRebuildScope::Full).await.unwrap();
        assert_eq!(stats.document_count, 1);
        assert!(!stats.needs_rebuild);
        assert!(stats.last_rebuild_at.is_some());
    }

    #[tokio::test]
    async fn test_search_with_kind_filter() {
        let store = setup_memory_store("filter_kind");

        let id_entity = Uuid::new_v4();
        let id_note = Uuid::new_v4();

        store
            .upsert_document(TextDocument {
                subject_id: id_entity,
                kind: SubstrateKind::Entity,
                title: Some("Rust Guide".to_string()),
                body: "A comprehensive guide to Rust programming.".to_string(),
                tags: vec![],
                namespace: "test_ns".to_string(),
                metadata: None,
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        store
            .upsert_document(TextDocument {
                subject_id: id_note,
                kind: SubstrateKind::Note,
                title: Some("Rust Notes".to_string()),
                body: "Quick notes about Rust concepts.".to_string(),
                tags: vec![],
                namespace: "test_ns".to_string(),
                metadata: None,
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        let hits = store
            .search(TextSearchRequest {
                query: "Rust".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    kinds: vec![SubstrateKind::Entity],
                    namespaces: vec!["test_ns".to_string()],
                    ..TextFilter::default()
                }),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject_id, id_entity);
    }

    #[tokio::test]
    async fn test_sanitize_fts5_query() {
        assert_eq!(sanitize_fts5_query("hello world"), "hello world");
        assert_eq!(sanitize_fts5_query("hello*world"), "helloworld");
        assert_eq!(sanitize_fts5_query("\"quoted\""), "quoted");
        assert_eq!(sanitize_fts5_query("(parens)"), "parens");
        assert_eq!(sanitize_fts5_query("a + b - c"), "a b c");
        assert_eq!(sanitize_fts5_query("col:value"), "col value");
        assert_eq!(sanitize_fts5_query(""), "");
        assert_eq!(sanitize_fts5_query("***"), "");
        // M-C4: decimal numbers must not produce "syntax error near '.'"
        assert_eq!(
            sanitize_fts5_query("salience 0.9 vs 0.3"),
            "salience 09 vs 03"
        );
        assert_eq!(sanitize_fts5_query("version 1.2.3"), "version 123");
        // H1: tilde and comma must be stripped to prevent FTS5 syntax errors
        assert_eq!(sanitize_fts5_query("~hello"), "hello");
        assert_eq!(sanitize_fts5_query("\"+_~!\""), "_");
        assert_eq!(sanitize_fts5_query("NEAR(smile, 5)"), "smile 5");
        assert_eq!(sanitize_fts5_query("a,b,c"), "a b c");
        // #570: full operator-class matrix
        // Apostrophe fix: single quote is an FTS5 string-literal delimiter in Plain mode.
        assert_eq!(sanitize_fts5_query("Bob's tenant"), "Bobs tenant");
        assert_eq!(
            sanitize_fts5_query("tenant AND isolation"),
            "tenant isolation"
        );
        assert_eq!(
            sanitize_fts5_query("tenant OR isolation"),
            "tenant isolation"
        );
        assert_eq!(
            sanitize_fts5_query("tenant NOT isolation"),
            "tenant isolation"
        );
        assert_eq!(
            sanitize_fts5_query("tenant NEAR(isolation, 5)"),
            "tenant isolation 5"
        );
        assert_eq!(sanitize_fts5_query("tenant:isolation"), "tenant isolation");
        assert_eq!(
            sanitize_fts5_query("tenant ^ isolation"),
            "tenant isolation"
        );
        assert_eq!(
            sanitize_fts5_query("(tenant isolation)"),
            "tenant isolation"
        );
        // whitespace-only becomes empty
        assert_eq!(sanitize_fts5_query("   "), "");
        // operator-only after stripping becomes empty
        assert_eq!(sanitize_fts5_query("AND OR NOT"), "");
    }

    /// H1 regression: queries with tilde (~) must not produce "fts5: syntax error near '~'".
    #[tokio::test]
    async fn test_search_with_tilde_does_not_crash() {
        let store = setup_memory_store("tilde_query");

        store
            .upsert_document(make_document(Uuid::new_v4(), "smile", "smiling face"))
            .await
            .unwrap();

        let result = store
            .search(TextSearchRequest {
                query: "~smile".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await;
        assert!(
            result.is_ok(),
            "tilde query must not crash FTS5, got: {:?}",
            result.err()
        );
    }

    /// H1 regression: NEAR() queries must not produce "fts5: syntax error near ','".
    #[tokio::test]
    async fn test_search_with_near_operator_does_not_crash() {
        let store = setup_memory_store("near_query");

        store
            .upsert_document(make_document(Uuid::new_v4(), "smile", "quokka smile happy"))
            .await
            .unwrap();

        let result = store
            .search(TextSearchRequest {
                query: "quokka NEAR(smile, 5)".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await;
        assert!(
            result.is_ok(),
            "NEAR() query must not crash FTS5, got: {:?}",
            result.err()
        );
    }

    /// M-C4 regression: searching with decimal numbers must succeed (not crash FTS5).
    ///
    /// Previously `.` was not stripped, causing FTS5 to return
    /// "fts5: syntax error near '.'" when queries contained decimal literals like "0.9".
    #[tokio::test]
    async fn test_search_with_decimal_query_does_not_crash() {
        let store = setup_memory_store("decimal_query");

        // Insert a document that contains decimal-like content.
        store
            .upsert_document(make_document(
                Uuid::new_v4(),
                "salience thresholds",
                "salience 09 vs 03 comparison",
            ))
            .await
            .unwrap();

        // Must not return an error — previously "fts5: syntax error near '.'"
        let result = store
            .search(TextSearchRequest {
                query: "salience 0.9 vs 0.3".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await;
        assert!(
            result.is_ok(),
            "decimal query must succeed, got error: {:?}",
            result.err()
        );

        // Also test with version strings.
        let result2 = store
            .search(TextSearchRequest {
                query: "salience 0.9 vs version 1.2.3".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await;
        assert!(
            result2.is_ok(),
            "version-string query must succeed, got error: {:?}",
            result2.err()
        );
    }

    /// #570: all FTS5 operator classes must not crash the generic text search surface.
    #[tokio::test]
    async fn test_search_with_fts_operator_matrix_does_not_crash() {
        let store = setup_memory_store("fts_operator_matrix");

        store
            .upsert_document(make_document(
                Uuid::new_v4(),
                "tenant isolation",
                "multi-tenant isolation operator regression anchor content",
            ))
            .await
            .unwrap();

        let cases: &[&str] = &[
            "\"tenant isolation\"",
            "Bob \"quoted\" tenant",
            "tenant AND isolation",
            "tenant OR isolation",
            "tenant NOT isolation",
            "tenant NEAR(isolation, 5)",
            "tenant*",
            "***",
            "tenant:isolation",
            "tenant ^ isolation",
            "(tenant isolation)",
            "(\"+_~!\")",
            "tenant:foo^bar*",
            "multi-tenant isolation",
            "   ",
            "",
        ];

        for query in cases {
            let result = store
                .search(TextSearchRequest {
                    query: query.to_string(),
                    mode: TextQueryMode::Plain,
                    filter: Some(ns_filter("test_ns")),
                    top_k: 10,
                    snippet_chars: 64,
                })
                .await;
            assert!(
                result.is_ok(),
                "#570 DB search query {query:?} must not crash FTS5, got: {:?}",
                result.err()
            );
        }
    }

    #[tokio::test]
    async fn test_score_is_bounded() {
        let store = setup_memory_store("score_bounds");

        for i in 0..5 {
            store
                .upsert_document(make_document(
                    Uuid::new_v4(),
                    &format!("Doc {}", i),
                    &format!("This document discusses topic number {}", i),
                ))
                .await
                .unwrap();
        }

        let hits = store
            .search(TextSearchRequest {
                query: "document topic".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();

        for hit in &hits {
            let score = hit.score.to_f64();
            assert!(
                score > 0.0 && score <= 1.0,
                "score out of (0, 1] range: {}",
                score
            );
        }

        for (i, hit) in hits.iter().enumerate() {
            assert_eq!(hit.rank, (i + 1) as u32);
        }
    }

    #[tokio::test]
    async fn test_rename_namespace() {
        let store = setup_memory_store("rename_ns");

        let id = Uuid::new_v4();
        let doc = TextDocument {
            subject_id: id,
            kind: SubstrateKind::Note,
            title: Some("Rename test".to_string()),
            body: "keyword_unique_xyz".to_string(),
            tags: vec![],
            namespace: "old_ns".to_string(),
            metadata: None,
            updated_at: Utc::now(),
        };
        store.upsert_document(doc).await.unwrap();

        let before = store
            .search(TextSearchRequest {
                query: "keyword_unique_xyz".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("old_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();
        assert_eq!(before.len(), 1);

        let moved = store.rename_namespace("old_ns", "new_ns").await.unwrap();
        assert_eq!(moved, 1);

        let after_new = store
            .search(TextSearchRequest {
                query: "keyword_unique_xyz".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("new_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();
        assert_eq!(after_new.len(), 1);

        let after_old = store
            .search(TextSearchRequest {
                query: "keyword_unique_xyz".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("old_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();
        assert!(after_old.is_empty());
    }

    #[tokio::test]
    async fn test_metadata_none_roundtrip() {
        let store = setup_memory_store("meta_none");
        let id = uuid::Uuid::new_v4();
        let doc = TextDocument {
            subject_id: id,
            kind: SubstrateKind::Note,
            namespace: "test_ns".to_string(),
            title: None,
            body: "no metadata".to_string(),
            tags: vec![],
            metadata: None,
            updated_at: Utc::now(),
        };
        store.upsert_document(doc).await.unwrap();
        let fetched = store.get_document("test_ns", id).await.unwrap().unwrap();
        assert!(fetched.metadata.is_none());
    }

    #[tokio::test]
    async fn test_rename_namespace_noop() {
        let store = setup_memory_store("rename_noop");

        let id = Uuid::new_v4();
        let doc = TextDocument {
            subject_id: id,
            kind: SubstrateKind::Note,
            title: None,
            body: "noop_test_content".to_string(),
            tags: vec![],
            namespace: "same_ns".to_string(),
            metadata: None,
            updated_at: Utc::now(),
        };
        store.upsert_document(doc).await.unwrap();

        let moved = store.rename_namespace("same_ns", "same_ns").await.unwrap();
        assert_eq!(moved, 0);

        let hits = store
            .search(TextSearchRequest {
                query: "noop_test_content".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("same_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    /// snippet_chars=0 omits snippet computation without changing IDs, ranks, or scores.
    ///
    /// Regression for the snippet-free FTS optimization: verifies the `NULL AS snippet`
    /// path returns identical candidate identity and ordering to the regular path, and
    /// that every snippet field is None when snippet_chars=0.
    #[tokio::test]
    async fn search_snippet_chars_zero_omits_snippets_without_changing_rank() {
        let store = setup_memory_store("snippet_zero");

        let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        let bodies = [
            "alpha bravo charlie delta the quick fox jumped",
            "bravo charlie delta echo the slow fox slept",
            "charlie delta echo foxtrot the lazy dog barked",
            "delta echo foxtrot golf a completely different document",
        ];
        for (id, body) in ids.iter().zip(bodies.iter()) {
            store
                .upsert_document(make_document(*id, "title", body))
                .await
                .unwrap();
        }

        let req_with = TextSearchRequest {
            query: "bravo charlie".to_string(),
            mode: TextQueryMode::AnyTerm,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        };
        let req_zero = TextSearchRequest {
            snippet_chars: 0,
            ..req_with.clone()
        };

        let hits_with = store.search(req_with).await.unwrap();
        let hits_zero = store.search(req_zero).await.unwrap();

        assert!(!hits_with.is_empty(), "snippet path must return hits");
        assert_eq!(
            hits_with.len(),
            hits_zero.len(),
            "hit count must be identical regardless of snippet_chars"
        );

        for (hw, hz) in hits_with.iter().zip(hits_zero.iter()) {
            assert_eq!(hw.subject_id, hz.subject_id, "subject_id must match");
            assert_eq!(hw.rank, hz.rank, "rank must match");
            assert!(
                (hw.score.to_f64() - hz.score.to_f64()).abs() < 1e-12,
                "score must match: with={} zero={}",
                hw.score.to_f64(),
                hz.score.to_f64()
            );
            assert!(
                hz.snippet.is_none(),
                "snippet must be None when snippet_chars=0, got {:?}",
                hz.snippet
            );
        }
    }

    // Boundary case: a hit ranked near the last position in a multi-result set
    // must still have snippet=None when snippet_chars=0.
    #[tokio::test]
    async fn search_snippet_chars_zero_bottom_ranked_hit_has_no_snippet() {
        let store = setup_memory_store("snippet_zero_boundary");

        // Insert enough docs so the last-ranked result is a "boundary" case.
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for (i, id) in ids.iter().enumerate() {
            let body = format!("keyword_boundary doc number {i} with varying relevance");
            store
                .upsert_document(make_document(*id, "t", &body))
                .await
                .unwrap();
        }

        let hits = store
            .search(TextSearchRequest {
                query: "keyword_boundary".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .unwrap();

        assert_eq!(hits.len(), 5, "all 5 docs must match");
        // The last-ranked hit (boundary) must also have no snippet.
        let last = hits.last().unwrap();
        assert!(
            last.snippet.is_none(),
            "bottom-ranked hit must have snippet=None when snippet_chars=0, got {:?}",
            last.snippet
        );
    }

    /// Score normalization: all scores stay in (0, 1], and a single-hit result
    /// scores ≈ 1.0. This validates the normalization formula independent of
    /// FTS5 rank ordering guarantees (which are already tested via `rank` field).
    #[tokio::test]
    async fn test_score_normalization_range() {
        let store = setup_memory_store("score_range");

        // Insert three documents; only two match the query.
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();
        store
            .upsert_document(make_document(
                id1,
                "normtest topic",
                "normtest normtest normtest",
            ))
            .await
            .unwrap();
        store
            .upsert_document(make_document(
                id2,
                "normtest light",
                "other content without the keyword",
            ))
            .await
            .unwrap();
        store
            .upsert_document(make_document(
                id3,
                "irrelevant title",
                "completely different document content",
            ))
            .await
            .unwrap();

        let hits = store
            .search(TextSearchRequest {
                query: "normtest".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();

        // id3 must not match; id1 and id2 should.
        assert!(!hits.is_empty(), "at least one doc must match");
        assert!(
            hits.iter().all(|h| h.subject_id != id3),
            "id3 must not appear"
        );

        // All scores must be in (0, 1].
        for h in &hits {
            let s = h.score.to_f64();
            assert!(s > 0.0 && s <= 1.0, "score out of (0,1]: {s}");
        }
        // Rank field must be 1-indexed and contiguous.
        for (i, h) in hits.iter().enumerate() {
            assert_eq!(h.rank, (i + 1) as u32, "rank must equal position+1");
        }
        // Best hit (rank=1) must score ≈ 1.0 — normalization anchors the best
        // rank to 1.0 regardless of absolute BM25 magnitude.
        assert!(
            hits[0].score.to_f64() > 0.99,
            "top hit must score ≈ 1.0, got {}",
            hits[0].score.to_f64()
        );

        // Single-hit result: the only match scores ≈ 1.0 (degenerate case:
        // range == 0 → all hits get 1.0).
        let single_id = Uuid::new_v4();
        store
            .upsert_document(make_document(
                single_id,
                "xqzplurp_unique_marker",
                "xqzplurp_unique_marker body",
            ))
            .await
            .unwrap();
        let single = store
            .search(TextSearchRequest {
                query: "xqzplurp_unique_marker".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();
        assert_eq!(single.len(), 1);
        assert!(
            single[0].score.to_f64() > 0.99,
            "single-hit must score ≈ 1.0, got {}",
            single[0].score.to_f64()
        );
    }
}
