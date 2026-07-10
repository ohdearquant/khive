//! FTS5-backed `TextSearch`: one virtual table per model, scores normalized to `(0.05, 1.0]`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, SqlStatement, SqlValue, TextDocument, TextFilter,
    TextGatherMode, TextIndexStats, TextQueryMode, TextSearchHit, TextSearchOptions,
    TextSearchRequest, TextTermStats, TextTermStatsRequest,
};
use khive_storage::StorageCapability;
use khive_storage::TextSearch;
use khive_types::SubstrateKind;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;
use crate::sql_bridge::bind_params;
use crate::writer_task::WriterTaskHandle;

/// The exact `DELETE` this store's `delete_document` issues, for a given
/// FTS table (ADR-099 B3 r6 structural cut — see `entity.rs`'s sibling
/// block). `table` must already be a trusted, sanitized table name (this
/// mirrors `delete_document`'s own pre-existing lack of a placeholder for
/// table names — `format!` is required since table identifiers cannot be
/// bound as SQL parameters).
pub fn delete_document_statement(table: &str, namespace: &str, subject_id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: format!("DELETE FROM {table} WHERE namespace = ?1 AND subject_id = ?2"),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(subject_id.to_string()),
        ],
        label: Some(format!("fts-delete-{table}")),
    }
}

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
    writer_task: Option<WriterTaskHandle>,
}

impl Fts5TextSearch {
    /// Create a new FTS5 text search instance.
    ///
    /// The FTS5 virtual table must already exist (created by `StorageBackend::text()`).
    pub(crate) fn new(pool: Arc<ConnectionPool>, is_file_backed: bool, table_key: String) -> Self {
        let table_name = format!("fts_{}", table_key);
        // Best-effort opt-in (ADR-067 Component A, mirrors entity.rs slice 1
        // policy): a missing writer task degrades to the legacy pool-mutex /
        // standalone-connection path rather than failing construction.
        let writer_task = pool.writer_task_handle().ok().flatten();
        Self {
            pool,
            is_file_backed,
            table_name,
            writer_task,
        }
    }

    fn open_standalone_writer(&self) -> Result<rusqlite::Connection, StorageError> {
        self.pool
            .open_standalone_writer()
            .map_err(|e| map_sqlite_err(e, "open_fts_writer"))
    }

    fn open_standalone_reader(&self) -> Result<rusqlite::Connection, StorageError> {
        self.pool
            .open_standalone_reader()
            .map_err(|e| map_sqlite_err(e, "open_fts_reader"))
    }

    /// Route a single-row write through the pool-wide `WriterTask` when
    /// `KHIVE_WRITE_QUEUE=1` and a handle is available; otherwise fall back
    /// to the legacy standalone-connection / pool-mutex path (ADR-067
    /// Component A, Fork C slice 2).
    ///
    /// This is the routing point for `with_writer` callers whose closure is
    /// DML-only (`delete_document`/`fts_delete`, `rebuild`/`fts_rebuild`):
    /// on the flag-on path the closure runs inside the WriterTask's own
    /// transaction, so a bare `BEGIN IMMEDIATE` would violate SQLite's
    /// nested-transaction rule. `upsert_document`/`upsert_documents` (the
    /// single-doc and batch write methods) do their own flag check and
    /// return early on `Some`, so their fallback calls into this helper
    /// only ever execute on the flag-off path (`self.writer_task` is
    /// `None` by construction whenever those calls are reached) — no
    /// double-routing.
    ///
    /// `rename_namespace` (`#[allow(dead_code)]`, no production caller —
    /// see ADR-067's `BEGIN IMMEDIATE` site inventory, EXEMPT) manages its
    /// own manual transaction and calls [`Self::with_writer_unmanaged`]
    /// instead of this helper — routing its closure through the WriterTask
    /// would nest a bare `BEGIN IMMEDIATE` inside the WriterTask's own
    /// transaction.
    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(writer_task) = &self.writer_task {
            return writer_task
                .send(move |conn| f(conn).map_err(|e| map_err(e, op)))
                .await;
        }

        self.with_writer_unmanaged(op, f).await
    }

    /// Legacy standalone-connection / pool-mutex write path, bypassing the
    /// WriterTask channel unconditionally regardless of
    /// `KHIVE_WRITE_QUEUE`.
    ///
    /// Reserved for closures that manage their own transaction (a bare
    /// `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK`) — those cannot be sent through
    /// the WriterTask channel, which already wraps every request in its own
    /// transaction. `rename_namespace` is the only caller.
    async fn with_writer_unmanaged<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
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
///    It also keeps punctuated identifiers searchable: `khive-pack-memory`
///    becomes `khive pack memory`, not `khivepackmemory`.
///    Chars replaced with space: `(`, `)`, `,`, `:`, `-`, `.`
/// 2. **Remove** remaining FTS5 operator characters (H1: `~`, `!` added;
///    issue #388: `$` added — FTS5's MATCH-expression parser treats a bareword
///    starting with, containing, or consisting solely of `$` as a syntax
///    error regardless of tokenizer, e.g. the DSL-doc query `$prev.id`):
///    `*`, `"`, `'`, `+`, `^`, `~`, `!`, `$`, `\0`, control characters
///
/// After character processing, split on whitespace and remove FTS5 keyword
/// tokens: AND, OR, NOT, NEAR.
///
/// For Phrase mode, the caller wraps the result in double quotes.
fn sanitize_fts5_query(query: &str) -> String {
    // Pass 1: replace grouping/separator chars with spaces to isolate tokens.
    // Colon, hyphen, and dot are included here (not in Pass 2) so punctuated
    // identifiers become separate terms rather than merged tokens.
    let spaced: String = query
        .chars()
        .map(|c| {
            if matches!(c, '(' | ')' | ',' | ':' | '-' | '.') {
                ' '
            } else {
                c
            }
        })
        .collect();

    // Pass 2: remove remaining FTS5 special chars and control characters.
    // Single quote (apostrophe) is included because FTS5 Plain-mode queries treat
    // it as a string-literal delimiter causing "syntax error near '''".
    // Dollar sign is included (#388) because FTS5's MATCH parser rejects it
    // unconditionally — `syntax error near "$"` — wherever it appears in the
    // expression, e.g. "$prev.id" (a common agent query for DSL docs).
    let sanitized: String = spaced
        .chars()
        .filter(|c| {
            !matches!(c, '*' | '"' | '\'' | '+' | '^' | '~' | '!' | '$' | '\0') && !c.is_control()
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

/// Legacy (pre-#397) sanitization: hyphen and dot are stripped outright
/// instead of being space-split, so `khive-pack-memory` normalizes to the
/// single merged bareword `khivepackmemory` rather than three terms.
///
/// Used only to build the merged-form OR-alternative in
/// [`sanitize_fts5_token_group`] — never as the sole sanitized query. Content
/// indexed before #397, or under a tokenizer whose own token-splitting rules
/// collapse punctuation differently than the current space-split pass,  may
/// still carry this merged token; kept as a fallback match term so those
/// documents stay reachable.
fn sanitize_fts5_query_legacy_merged(query: &str) -> String {
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

    let sanitized: String = spaced
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '*' | '"' | '\'' | '+' | '-' | '^' | '.' | '~' | '!' | '$' | '\0'
            ) && !c.is_control()
        })
        .collect();

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

/// Strip a raw token down to what is safe inside a double-quoted FTS5 phrase:
/// the closing delimiter (`"`), the trailing-`*` prefix-query trigger, and
/// control characters. Everything else — including `-`, `.`, `:`, `$`, `'`,
/// digits — passes through literally, since FTS5 phrase text is matched
/// against the column's own tokenization of that literal text (word-exact
/// under `unicode61`, substring-exact under `trigram`), not re-sanitized.
///
/// Returns `None` if nothing survives the filter.
fn sanitize_fts5_phrase_literal(token: &str) -> Option<String> {
    let literal: String = token
        .chars()
        .filter(|c| !matches!(c, '"' | '*' | '\0') && !c.is_control())
        .collect();
    if literal.is_empty() {
        None
    } else {
        Some(literal)
    }
}

/// Below this length, FTS5's built-in `trigram` tokenizer (the production
/// default — `backend.rs::StorageBackend::text()`) produces zero tokens for
/// a bareword MATCH term. Verified against a live `tokenize='trigram'`
/// table: a term this short silently drops out of its AND clause instead of
/// constraining the match, e.g. `2026 07 10` matches any row containing
/// `2026` regardless of month/day. `sanitize_fts5_token_group` treats any
/// split segment at or below this length as trigram-unsafe.
const FTS5_TRIGRAM_MIN_SAFE_LEN: usize = 3;

/// Sanitize a single whitespace-isolated raw token into an FTS5
/// match-expression fragment.
///
/// #397 split punctuated identifiers (`khive-pack-memory` -> three terms
/// `khive pack memory`, ANDed together) so they are searchable as distinct
/// words. That is correct against content indexed with word-splitting
/// tokenizers (`unicode61`, and multi-word `trigram` content where each
/// word is long enough to trigram on its own), but two more cases need
/// covering when punctuation actually causes a split:
///
/// - a query for content that only ever matched the pre-#397 merged
///   bareword (`khivepackmemory`) would silently stop matching — kept
///   reachable via the legacy-merged alternative.
/// - under the production `trigram` tokenizer, a split segment shorter
///   than [`FTS5_TRIGRAM_MIN_SAFE_LEN`] (e.g. the `07`/`10` in a
///   `2026-07-10` date) tokenizes to zero trigrams and silently drops out
///   of its AND clause, broadening the match to anything sharing the
///   longer segment (any `2026`, not just that date). Neither the plain
///   split AND-group nor the merged form avoids this — both were checked
///   empirically against a live `tokenize='trigram'` table and both either
///   broaden or simply fail to match. The fix that does work is a literal
///   phrase-quoted alternative: FTS5 matches it by exact substring under
///   `trigram` (confirmed: quoting `"2026-07-10"` discriminates the exact
///   day) and by exact adjacent sub-tokens under word tokenizers. So when
///   any split segment is trigram-unsafe, the split AND-group is dropped
///   entirely rather than paired with the merged/phrase alternatives — an
///   OR would still admit its broadened matches. Retrieval stays correct
///   (still matches the right content, still discriminates) at the cost of
///   requiring adjacency for that token, which is the trade-off intended
///   by the finding this fixes (khive #397 Finding 2): correctness over a
///   marginally more lenient match.
///
/// All emitted readings are additive OR-alternatives otherwise — the result
/// is never a narrower match than any single (safe) form alone.
///
/// Returns `None` if the token sanitizes to nothing.
fn sanitize_fts5_token_group(token: &str) -> Option<String> {
    let split = sanitize_fts5_query(token);
    let split_terms: Vec<&str> = split.split_whitespace().collect();
    if split_terms.is_empty() {
        return None;
    }
    if split_terms.len() == 1 {
        return Some(split_terms[0].to_string());
    }

    let has_trigram_unsafe_segment = split_terms
        .iter()
        .any(|t| t.chars().count() < FTS5_TRIGRAM_MIN_SAFE_LEN);

    let mut alternatives = Vec::new();
    if !has_trigram_unsafe_segment {
        alternatives.push(format!("({})", split_terms.join(" ")));
    }

    // An operator-bearing token (e.g. `NEAR(alpha-beta,5)`) can make the
    // legacy merge itself collapse to multiple space-separated terms rather
    // than one bareword: pass 1 of `sanitize_fts5_query_legacy_merged` spaces
    // out `(`, `)`, and `,` while pass 2 removes `-`/`.` outright, so
    // `NEAR(alpha-beta,5)` merges to `"alphabeta 5"`, not one word. Pushed
    // unguarded, that multi-term fragment carries the same trigram-unsafe
    // `5` the split-group check above exists to exclude, and under FTS5's
    // implicit-AND adjacency it silently drops, broadening the OR-alternative
    // to any row containing `alphabeta`. Apply the same trigram-safety gate
    // here whenever the merge is multi-term.
    let merged = sanitize_fts5_query_legacy_merged(token);
    let merged_terms: Vec<&str> = merged.split_whitespace().collect();
    let merged_has_unsafe_segment = merged_terms.len() > 1
        && merged_terms
            .iter()
            .any(|t| t.chars().count() < FTS5_TRIGRAM_MIN_SAFE_LEN);
    if !merged.is_empty() && merged != split_terms.join("") && !merged_has_unsafe_segment {
        alternatives.push(merged);
    }

    if let Some(phrase) = sanitize_fts5_phrase_literal(token) {
        alternatives.push(format!("\"{}\"", phrase));
    }

    match alternatives.len() {
        0 => Some(format!("({})", split_terms.join(" "))),
        1 => alternatives.into_iter().next(),
        _ => Some(format!("({})", alternatives.join(" OR "))),
    }
}

/// Join Plain-mode per-token groups into one MATCH expression.
///
/// FTS5's implicit-AND adjacency rule (a bare space between two terms)
/// applies only between two *plain* terms — verified against a live table:
/// `GQA KV cache` (all bare) matches, but `GQA AND KV AND cache` does not,
/// because FTS5's explicit `AND` treats a trigram-unsafe short bareword
/// (`KV`, 2 chars, zero trigrams) as an unsatisfiable operand, while
/// implicit adjacency instead lets it drop out harmlessly. So: use a bare
/// space between two plain (unparenthesized) groups to preserve that
/// existing leniency, and fall back to explicit `AND` only where at least
/// one side is a parenthesized OR-group (`sanitize_fts5_token_group`'s
/// output for punctuated tokens) — adjacency there without the operator is
/// a MATCH-expression syntax error (`(a OR b) c` fails; `(a OR b) AND c`
/// does not).
fn join_plain_groups(groups: &[String]) -> String {
    let mut expr = String::new();
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            let prev_compound = groups[i - 1].starts_with('(');
            let this_compound = group.starts_with('(');
            expr.push_str(if prev_compound || this_compound {
                " AND "
            } else {
                " "
            });
        }
        expr.push_str(group);
    }
    expr
}

/// Build the FTS5 MATCH expression for a query string under a given mode.
///
/// Centralizes the AnyTerm/Plain/Phrase branching previously duplicated
/// across `search()`, `search_unranked()`, and `search_rank_within_cap()`.
///
/// AnyTerm and Plain both process the query token-by-token (split on
/// whitespace) through [`sanitize_fts5_token_group`], so a punctuated
/// identifier anywhere in the query gets its split/merged OR-alternative;
/// AnyTerm joins the per-token groups with `OR`; Plain joins them via
/// [`join_plain_groups`] (implicit-AND space where safe, explicit `AND`
/// where a group is parenthesized). Phrase mode keeps the single-string
/// literal behavior — a double-quoted FTS5 phrase cannot contain
/// `OR`/parenthesized groups.
///
/// Returns `None` when the sanitized query is empty (caller short-circuits
/// to an empty result set rather than sending an invalid MATCH expression).
fn build_match_expr(query: &str, mode: TextQueryMode) -> Option<String> {
    match mode {
        TextQueryMode::AnyTerm => {
            let groups: Vec<String> = query
                .split_whitespace()
                .filter_map(sanitize_fts5_token_group)
                .collect();
            if groups.is_empty() {
                None
            } else {
                Some(groups.join(" OR "))
            }
        }
        TextQueryMode::Plain => {
            let groups: Vec<String> = query
                .split_whitespace()
                .filter_map(sanitize_fts5_token_group)
                .collect();
            if groups.is_empty() {
                None
            } else {
                Some(join_plain_groups(&groups))
            }
        }
        TextQueryMode::Phrase => {
            let sanitized = sanitize_fts5_query(query);
            if sanitized.is_empty() {
                None
            } else {
                Some(format!("\"{}\"", sanitized))
            }
        }
    }
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

/// DML-only single-document upsert shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `upsert_document` paths (ADR-067 Component A).
///
/// Issues no `BEGIN` / `COMMIT` / `ROLLBACK` itself — the caller owns the
/// enclosing transaction.
fn upsert_document_dml(
    conn: &rusqlite::Connection,
    table: &str,
    document: &TextDocument,
) -> Result<(), rusqlite::Error> {
    let namespace = &document.namespace;

    let del_sql = format!(
        "DELETE FROM {} WHERE namespace = ?1 AND subject_id = ?2",
        table
    );
    conn.execute(
        &del_sql,
        rusqlite::params![namespace, document.subject_id.to_string()],
    )?;

    let ins_sql = format!(
        "INSERT INTO {} \
         (subject_id, kind, title, body, tags, namespace, metadata, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        table
    );
    let tags_json = tags_to_json(&document.tags);
    let metadata_json: Option<String> = document.metadata.as_ref().map(|v| v.to_string());

    conn.execute(
        &ins_sql,
        rusqlite::params![
            document.subject_id.to_string(),
            document.kind.to_string(),
            document.title.as_deref().unwrap_or(""),
            document.body,
            tags_json,
            namespace,
            metadata_json,
            dt_to_micros(&document.updated_at),
        ],
    )?;
    Ok(())
}

/// DML-only batch upsert loop shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `upsert_documents` paths (ADR-067 Component A).
///
/// Issues no OUTER `BEGIN` / `COMMIT` / `ROLLBACK` — the caller owns the
/// enclosing transaction. The per-row named `SAVEPOINT fts_upsert_doc` is
/// preserved unchanged: it is what gives this loop its partial-success
/// semantics (one bad document does not abort the whole batch) independent
/// of which outer transaction wraps the loop.
fn batch_upsert_documents_dml(
    conn: &rusqlite::Connection,
    table: &str,
    documents: &[TextDocument],
    attempted: u64,
) -> Result<BatchWriteSummary, rusqlite::Error> {
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

    let mut affected = 0u64;
    let mut failed = 0u64;
    let mut first_error = String::new();

    for doc in documents {
        conn.execute_batch("SAVEPOINT fts_upsert_doc")?;
        let id_str = doc.subject_id.to_string();
        let namespace = &doc.namespace;
        let result = (|| {
            conn.execute(&del_sql, rusqlite::params![namespace, &id_str])?;

            let tags_json = tags_to_json(&doc.tags);
            let metadata_json: Option<String> = doc.metadata.as_ref().map(|v| v.to_string());

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
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK TO SAVEPOINT fts_upsert_doc");
                let _ = conn.execute_batch("RELEASE SAVEPOINT fts_upsert_doc");
                if first_error.is_empty() {
                    first_error = e.to_string();
                }
                failed += 1;
            }
        }
    }

    Ok(BatchWriteSummary {
        attempted,
        affected,
        failed,
        first_error,
    })
}

#[async_trait]
impl TextSearch for Fts5TextSearch {
    async fn upsert_document(&self, document: TextDocument) -> Result<(), StorageError> {
        let table = self.table_name.clone();

        // ADR-067 Component A: when the write queue is enabled, route
        // through the pool-wide WriterTask. DML-only closure — no BEGIN
        // IMMEDIATE/COMMIT/ROLLBACK here, since the WriterTask's run loop
        // owns the transaction.
        if let Some(writer_task) = &self.writer_task {
            let table2 = table.clone();
            return writer_task
                .send(move |conn| {
                    upsert_document_dml(conn, &table2, &document)
                        .map_err(|e| map_err(e, "fts_upsert"))
                })
                .await;
        }

        // Flag-off (default) path: byte-for-byte unchanged from pre-ADR-067
        // behavior — the closure owns its own BEGIN IMMEDIATE/COMMIT/ROLLBACK.
        self.with_writer("fts_upsert", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let _tx_handle =
                khive_storage::tx_registry::register(Some("text_upsert_document".to_string()));

            if let Err(e) = upsert_document_dml(conn, &table, &document) {
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

        // ADR-067 Component A: when the write queue is enabled, route
        // through the pool-wide WriterTask. DML-only closure (the per-row
        // `SAVEPOINT fts_upsert_doc` is preserved unchanged — only the OUTER
        // BEGIN IMMEDIATE/COMMIT is removed, since the WriterTask's run loop
        // owns the enclosing transaction).
        if let Some(writer_task) = &self.writer_task {
            let table2 = table.clone();
            return writer_task
                .send(move |conn| {
                    batch_upsert_documents_dml(conn, &table2, &documents, attempted)
                        .map_err(|e| map_err(e, "fts_upsert_batch"))
                })
                .await;
        }

        // Flag-off (default) path: byte-for-byte unchanged from pre-ADR-067
        // behavior — the closure owns its own BEGIN IMMEDIATE/COMMIT.
        self.with_writer("fts_upsert_batch", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let _tx_handle =
                khive_storage::tx_registry::register(Some("text_upsert_batch".to_string()));

            let summary = batch_upsert_documents_dml(conn, &table, &documents, attempted)?;

            conn.execute_batch("COMMIT")?;

            Ok(summary)
        })
        .await
    }

    async fn delete_document(
        &self,
        namespace: &str,
        subject_id: Uuid,
    ) -> Result<bool, StorageError> {
        let statement = delete_document_statement(&self.table_name, namespace, subject_id);

        self.with_writer("fts_delete", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? > 0)
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
            let match_expr = match build_match_expr(&request.query, request.mode) {
                Some(expr) => expr,
                None => return Ok(Vec::new()),
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

    async fn search_with_options(
        &self,
        request: TextSearchRequest,
        options: TextSearchOptions,
    ) -> Result<Vec<TextSearchHit>, StorageError> {
        match options.gather_mode {
            TextGatherMode::Ranked => self.search(request).await,
            TextGatherMode::Unranked => self.search_unranked(request).await,
            TextGatherMode::RankWithinCap => {
                let gather_limit = options
                    .gather_limit
                    .unwrap_or(request.top_k)
                    .max(request.top_k);
                self.search_rank_within_cap(request, gather_limit).await
            }
        }
    }

    async fn term_stats(
        &self,
        request: TextTermStatsRequest,
    ) -> Result<Vec<TextTermStats>, StorageError> {
        let table = self.table_name.clone();

        self.with_reader("fts_term_stats", move |conn| {
            let filter = request.filter.as_ref();

            // Document count uses params starting at ?1 (no MATCH expression).
            let (count_filter_clause, count_filter_params) = if let Some(f) = filter {
                build_filter_clause(f, &table, 1)
            } else {
                (String::new(), Vec::new())
            };

            let document_count: u64 = {
                let count_sql = if count_filter_clause.is_empty() {
                    format!("SELECT COUNT(*) FROM {table}")
                } else {
                    let where_part = count_filter_clause.trim_start_matches(" AND ");
                    format!("SELECT COUNT(*) FROM {table} WHERE {where_part}")
                };
                let mut stmt = conn.prepare(&count_sql)?;
                for (i, param) in count_filter_params.iter().enumerate() {
                    param
                        .to_sql()
                        .map(|val| stmt.raw_bind_parameter(1 + i, val))
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))??;
                }
                let mut rows = stmt.raw_query();
                match rows.next()? {
                    Some(row) => {
                        let c: i64 = row.get(0)?;
                        c as u64
                    }
                    None => 0,
                }
            };

            let mut results = Vec::with_capacity(request.terms.len());
            for term in &request.terms {
                let sanitized = sanitize_fts5_query(term);
                if sanitized.is_empty() {
                    results.push(TextTermStats {
                        term: term.clone(),
                        sanitized_term: sanitized,
                        document_frequency: 0,
                        document_count,
                        inverse_document_frequency: 0.0,
                    });
                    continue;
                }

                // Per-term count: MATCH is ?1, so filter params start at ?2.
                let (term_filter_clause, term_filter_params) = if let Some(f) = filter {
                    build_filter_clause(f, &table, 2)
                } else {
                    (String::new(), Vec::new())
                };

                let count_sql = format!(
                    "SELECT COUNT(*) FROM {table} WHERE {table} MATCH ?1{term_filter_clause}"
                );
                let mut stmt = conn.prepare(&count_sql)?;
                stmt.raw_bind_parameter(1, &sanitized)?;
                for (i, param) in term_filter_params.iter().enumerate() {
                    param
                        .to_sql()
                        .map(|val| stmt.raw_bind_parameter(2 + i, val))
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))??;
                }

                let df: u64 = {
                    let mut rows = stmt.raw_query();
                    match rows.next()? {
                        Some(row) => {
                            let c: i64 = row.get(0)?;
                            c as u64
                        }
                        None => 0,
                    }
                };

                let idf = Fts5TextSearch::bm25_idf(df, document_count);
                results.push(TextTermStats {
                    term: term.clone(),
                    sanitized_term: sanitized,
                    document_frequency: df,
                    document_count,
                    inverse_document_frequency: idf,
                });
            }

            Ok(results)
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
    /// Robertson-Walker BM25 IDF: ln(((N - df + 0.5) / (df + 0.5)) + 1)
    fn bm25_idf(df: u64, document_count: u64) -> f64 {
        let n = document_count as f64;
        let f = df as f64;
        ((n - f + 0.5) / (f + 0.5) + 1.0).ln()
    }

    /// Gather candidates without BM25 ranking; return with uniform score 1.0.
    async fn search_unranked(
        &self,
        request: TextSearchRequest,
    ) -> Result<Vec<TextSearchHit>, StorageError> {
        let table = self.table_name.clone();

        self.with_reader("fts_search_unranked", move |conn| {
            let match_expr = match build_match_expr(&request.query, request.mode) {
                Some(expr) => expr,
                None => return Ok(Vec::new()),
            };

            let (filter_clause, filter_params) = if let Some(ref filter) = request.filter {
                build_filter_clause(filter, &table, 3)
            } else {
                (String::new(), Vec::new())
            };

            // No rank column, no ORDER BY — avoids BM25 computation entirely.
            let sql = format!(
                "SELECT subject_id, title \
                 FROM {table} WHERE {table} MATCH ?1{filter_clause} \
                 LIMIT ?2",
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

            let mut results = Vec::new();
            let mut rows = stmt.raw_query();
            let mut rank_idx = 0u32;

            while let Some(row) = rows.next()? {
                let id_str: String = row.get(0)?;
                let title: String = row.get(1)?;

                let subject_id = Uuid::parse_str(&id_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

                rank_idx += 1;
                results.push(TextSearchHit {
                    subject_id,
                    score: DeterministicScore::from_f64(1.0),
                    rank: rank_idx,
                    title: if title.is_empty() { None } else { Some(title) },
                    snippet: None,
                });
            }

            Ok(results)
        })
        .await
    }

    /// Two-stage gather: cheap unranked LIMIT gather_limit, then BM25-rank the subset.
    async fn search_rank_within_cap(
        &self,
        request: TextSearchRequest,
        gather_limit: u32,
    ) -> Result<Vec<TextSearchHit>, StorageError> {
        let table = self.table_name.clone();

        self.with_reader("fts_search_rank_within_cap", move |conn| {
            let match_expr = match build_match_expr(&request.query, request.mode) {
                Some(expr) => expr,
                None => return Ok(Vec::new()),
            };

            let (filter_clause, filter_params) = if let Some(ref filter) = request.filter {
                build_filter_clause(filter, &table, 3)
            } else {
                (String::new(), Vec::new())
            };

            // Stage 1: cheap unranked gather of rowids.
            let gather_sql = format!(
                "SELECT subject_id FROM {table} WHERE {table} MATCH ?1{filter_clause} LIMIT ?2"
            );

            let mut stmt = conn.prepare(&gather_sql)?;
            stmt.raw_bind_parameter(1, &match_expr)?;
            stmt.raw_bind_parameter(2, gather_limit as i64)?;
            for (i, param) in filter_params.iter().enumerate() {
                param
                    .to_sql()
                    .map(|val| stmt.raw_bind_parameter(3 + i, val))
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))??;
            }

            let mut gathered_ids: Vec<String> = Vec::new();
            let mut rows = stmt.raw_query();
            while let Some(row) = rows.next()? {
                gathered_ids.push(row.get::<_, String>(0)?);
            }

            if gathered_ids.is_empty() {
                return Ok(Vec::new());
            }

            // Stage 2: BM25-rank only the gathered subset via subject_id IN (...).
            let snippet_expr = if request.snippet_chars == 0 {
                "NULL AS snippet".to_string()
            } else {
                let chars = i32::try_from(request.snippet_chars).unwrap_or(i32::MAX);
                format!("snippet({table}, 3, '', '', '...', {chars})")
            };

            // Build IN clause for the gathered IDs.
            let id_placeholders: Vec<String> = gathered_ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", 3 + i))
                .collect();
            let in_clause = id_placeholders.join(", ");

            let rank_sql = format!(
                "SELECT subject_id, rank, title, {snippet_expr} \
                 FROM {table} WHERE {table} MATCH ?1 AND subject_id IN ({in_clause}) \
                 ORDER BY rank LIMIT ?2"
            );

            let mut stmt2 = conn.prepare(&rank_sql)?;
            stmt2.raw_bind_parameter(1, &match_expr)?;
            stmt2.raw_bind_parameter(2, request.top_k as i64)?;
            for (i, id_str) in gathered_ids.iter().enumerate() {
                stmt2.raw_bind_parameter(3 + i, id_str.as_str())?;
            }

            let mut hits = Vec::new();
            let mut rows2 = stmt2.raw_query();
            let mut rank_idx = 0u32;

            while let Some(row) = rows2.next()? {
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

            // Normalize scores within the ranked subset (same formula as search()).
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

    /// Move all FTS5 documents from `old_namespace` to `new_namespace` in a
    /// single transaction.
    ///
    /// FTS5 virtual tables do not support updating indexed columns (`title`,
    /// `body`) via UPDATE. The correct approach is read-then-delete-then-reinsert.
    ///
    /// Callers must invoke this after any SQL-level namespace change on the
    /// backing entity table so that FTS5 keyword search stays consistent with
    /// the entity store.
    // REASON: reserved for namespace migration operations
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

        self.with_writer_unmanaged("fts_rename_namespace", move |conn| {
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
            let _tx_handle =
                khive_storage::tx_registry::register(Some("text_rename_namespace".to_string()));

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
#[path = "text_tests.rs"]
mod tests;
