//! Tenant-scoped FTS5 search over mirrored session messages.
//!
//! The public verb remains gated until transcript deletion and resume/export
//! continuity support are available. `search_ready` is the row-level
//! implementation that those follow-ons can expose after their gates pass.

use std::num::NonZeroUsize;

use chrono::DateTime;
use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_score::rrf_score_one_based;
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::deser;
use crate::vocab::{DEFAULT_LIMIT, MAX_LIMIT};

const VERB: &str = "session.search";
const RRF_K: usize = 10;
const VALID_SOURCES: &[&str] = &[
    "claude_code",
    "codex",
    "chatgpt_export",
    "claude_ai_export",
    "unknown",
];

/// A single SQL statement carries the scope predicate into both mirror tables.
/// The `unknown` source is intentionally invisible unless it is requested by
/// name; its rows are migration-preserved orphans without a parent session.
/// Rank and limit the narrow match rows before FTS reads message text for snippets.
const SEARCH_SQL: &str = r#"
WITH hits AS MATERIALIZED (
  SELECT m.namespace, m.source, m.session_id, m.created_at,
         session_messages_fts.rowid AS fts_rowid,
         bm25(session_messages_fts) AS fts_rank
    FROM session_messages_fts
    CROSS JOIN session_messages AS m ON m.mirror_rowid = session_messages_fts.rowid
    LEFT JOIN sessions AS s
      ON s.namespace = ?2 AND s.namespace = m.namespace
     AND s.source = m.source AND s.provider_session_id = m.session_id
   WHERE session_messages_fts MATCH ?1
     AND m.namespace = ?2
     AND (?3 IS NOT NULL OR m.source <> 'unknown')
     AND (?3 IS NULL OR m.source = ?3)
     AND (?4 IS NULL OR m.created_at >= ?4)
     AND (?5 IS NULL OR s.cwd = ?5)
     AND (s.rowid IS NOT NULL OR (m.source = 'unknown' AND ?3 = 'unknown'))
), session_ranks AS (
  SELECT namespace, source, session_id, MIN(fts_rank) AS best_rank
    FROM hits
   GROUP BY namespace, source, session_id
), limited_sessions AS MATERIALIZED (
  SELECT namespace, source, session_id, best_rank
    FROM session_ranks
   ORDER BY best_rank ASC, source ASC, session_id ASC
   LIMIT ?6
), top_snippet_rowids AS MATERIALIZED (
  SELECT namespace, source, session_id, fts_rowid, snippet_order
    FROM (
      SELECT h.namespace, h.source, h.session_id, h.fts_rowid,
             ROW_NUMBER() OVER (
               PARTITION BY h.namespace, h.source, h.session_id
               ORDER BY h.fts_rank ASC, h.created_at DESC
             ) AS snippet_order
        FROM limited_sessions AS r
        JOIN hits AS h
          ON h.namespace = r.namespace AND h.source = r.source
         AND h.session_id = r.session_id
    )
   WHERE snippet_order <= 3
), selected_snippets AS MATERIALIZED (
  SELECT c.namespace, c.source, c.session_id, c.snippet_order,
         snippet(session_messages_fts, 0, '[', ']', '…', 32) AS snippet
    FROM top_snippet_rowids AS c
    CROSS JOIN session_messages_fts
   WHERE session_messages_fts MATCH ?1
     AND session_messages_fts.rowid = c.fts_rowid
)
SELECT r.namespace, r.source, r.session_id AS provider_session_id,
       s.cwd, s.first_seen_at, s.last_seen_at, s.message_count,
       (SELECT json_group_array(snippet) FROM (
          SELECT h.snippet FROM selected_snippets AS h
           WHERE h.namespace = r.namespace AND h.source = r.source
             AND h.session_id = r.session_id
           ORDER BY h.snippet_order
       )) AS snippets
  FROM limited_sessions AS r
  LEFT JOIN sessions AS s
    ON s.namespace = ?2 AND s.namespace = r.namespace
   AND s.source = r.source AND s.provider_session_id = r.session_id
 ORDER BY r.best_rank ASC, r.source ASC, r.session_id ASC"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

struct ValidatedSearch {
    fts_query: String,
    limit: u32,
    since_micros: Option<i64>,
    source: Option<String>,
    cwd: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchSession {
    provider_session_id: String,
    source: String,
    namespace: String,
    cwd: Option<String>,
    first_seen_at: Option<String>,
    last_seen_at: Option<String>,
    message_count: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SearchHit {
    session: SearchSession,
    score: f64,
    snippets: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SearchResult {
    ok: bool,
    results: Vec<SearchHit>,
    count: usize,
    limit: u32,
}

/// This is kept as a separate seam so the no-scope refusal can be proved
/// directly. A production `NamespaceToken` is sealed and normally contains a
/// valid namespace; no caller-supplied parameter can substitute for it.
fn require_positive_scope(scope: Option<&str>) -> Result<&str, RuntimeError> {
    let scope = scope.unwrap_or("");
    if scope.trim().is_empty() {
        return Err(RuntimeError::permission_denied(
            VERB,
            "a positive authenticated tenant scope is required",
        ));
    }
    Ok(scope)
}

fn validate(params: Value) -> Result<ValidatedSearch, RuntimeError> {
    let p: SearchParams = deser(params)?;
    let limit = super::validate_limit(p.limit, DEFAULT_LIMIT, MAX_LIMIT).map_err(|n| {
        RuntimeError::InvalidInput(format!("{VERB}: limit must be in 1..={MAX_LIMIT}; got {n}"))
    })?;
    if let Some(source) = p.source.as_deref() {
        if !VALID_SOURCES.contains(&source) {
            return Err(RuntimeError::InvalidInput(format!(
                "{VERB}: source must be one of {}; got {:?}",
                VALID_SOURCES.join(", "),
                mask_for_redaction_surface(RedactionSurface::GateProbe, source)
            )));
        }
    }
    if p.cwd.as_deref().is_some_and(|cwd| cwd.trim().is_empty()) {
        return Err(RuntimeError::InvalidInput(format!(
            "{VERB}: cwd must be a non-empty string when provided"
        )));
    }
    let since_micros = p
        .since
        .as_deref()
        .map(|raw| {
            DateTime::parse_from_rfc3339(raw)
                .map(|stamp| {
                    // The inclusive lower bound must round up: a timestamp
                    // between two stored microseconds cannot include the
                    // earlier message.
                    stamp.timestamp() * 1_000_000
                        + (i64::from(stamp.timestamp_subsec_nanos()) + 999) / 1000
                })
                .map_err(|_| {
                    RuntimeError::InvalidInput(format!(
                        "{VERB}: since must be an RFC 3339 timestamp; got {:?}",
                        mask_for_redaction_surface(RedactionSurface::GateProbe, raw)
                    ))
                })
        })
        .transpose()?;

    // Treat the user's words as literal FTS terms. Quoting each term keeps
    // FTS operators in transcript text from changing query structure.
    let terms: Vec<String> = p
        .query
        .split_whitespace()
        .filter(|term| term.chars().any(char::is_alphanumeric))
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{VERB}: query must contain a searchable word"
        )));
    }

    Ok(ValidatedSearch {
        fts_query: terms.join(" AND "),
        limit,
        since_micros,
        source: p.source,
        cwd: p.cwd,
    })
}

fn decode_hit(row: &SqlRow, rank: usize) -> Result<SearchHit, RuntimeError> {
    let snippets_json = row.text("snippets").map(str::to_owned).map_err(|_| {
        RuntimeError::Internal(format!(
            "{VERB}: malformed mirror search row: snippets must be text"
        ))
    })?;
    let snippets: Vec<String> = serde_json::from_str(&snippets_json).map_err(|e| {
        RuntimeError::Internal(format!("{VERB}: malformed mirror search snippets: {e}"))
    })?;
    let one_based = NonZeroUsize::new(rank + 1).expect("enumerate index plus one is nonzero");
    Ok(SearchHit {
        session: SearchSession {
            provider_session_id: row.text("provider_session_id").map(str::to_owned).map_err(
                |_| {
                    RuntimeError::Internal(format!("{VERB}: malformed mirror search row: provider_session_id must be text"))
                },
            )?,
            source: row.text("source").map(str::to_owned).map_err(|_| {
                RuntimeError::Internal(format!("{VERB}: malformed mirror search row: source must be text"))
            })?,
            namespace: row.text("namespace").map(str::to_owned).map_err(|_| {
                RuntimeError::Internal(format!("{VERB}: malformed mirror search row: namespace must be text"))
            })?,
            cwd: row
                .opt_text("cwd")
                .map(|value| value.map(str::to_owned))
                .map_err(|_| {
                    RuntimeError::Internal(format!("{VERB}: malformed mirror search row: cwd must be text or null"))
                })?,
            first_seen_at: row
                .opt_i64("first_seen_at")
                .map_err(|_| {
                    RuntimeError::Internal(format!("{VERB}: malformed mirror search row: first_seen_at must be integer or null"))
                })?
                .map(micros_to_iso),
            last_seen_at: row
                .opt_i64("last_seen_at")
                .map_err(|_| {
                    RuntimeError::Internal(format!("{VERB}: malformed mirror search row: last_seen_at must be integer or null"))
                })?
                .map(micros_to_iso),
            message_count: row.opt_i64("message_count").map_err(|_| {
                RuntimeError::Internal(format!("{VERB}: malformed mirror search row: message_count must be integer or null"))
            })?,
        },
        score: rrf_score_one_based(one_based, RRF_K).to_f64(),
        snippets,
    })
}

/// Run the scoped row query after all product dependencies are ready.
///
/// There is deliberately no namespace/account field in `SearchParams`.
#[allow(dead_code)]
async fn search_ready(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let scope = require_positive_scope(Some(token.namespace().as_str()))?;
    let p = validate(params)?;
    let mut reader = runtime.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: SEARCH_SQL.to_string(),
            params: vec![
                SqlValue::Text(p.fts_query),
                SqlValue::Text(scope.to_string()),
                p.source.map(SqlValue::Text).unwrap_or(SqlValue::Null),
                p.since_micros
                    .map(SqlValue::Integer)
                    .unwrap_or(SqlValue::Null),
                p.cwd.map(SqlValue::Text).unwrap_or(SqlValue::Null),
                SqlValue::Integer(i64::from(p.limit)),
            ],
            label: Some("session_search_scoped_fts".into()),
        })
        .await?;
    let results: Vec<SearchHit> = rows
        .iter()
        .enumerate()
        .map(|(rank, row)| decode_hit(row, rank))
        .collect::<Result<_, _>>()?;
    let response = SearchResult {
        ok: true,
        count: results.len(),
        results,
        limit: p.limit,
    };
    Ok(serde_json::to_value(response).expect("SearchResult serializes"))
}

/// Dispatch entry point. Transcript deletion and resume/export continuity are
/// not yet available, so the public search surface remains gated.
pub(crate) async fn handle_search(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    handle_search_with_scope(runtime, Some(token.namespace().as_str()), params).await
}

/// The token is sealed upstream, so normal dispatch always supplies `Some`.
/// Keep the absent-scope refusal inside the handler seam for any future
/// connection-identity adapter and for a direct fail-open-gate regression.
async fn handle_search_with_scope(
    _runtime: &KhiveRuntime,
    scope: Option<&str>,
    params: Value,
) -> Result<Value, RuntimeError> {
    require_positive_scope(scope)?;
    let _ = validate(params)?;
    Err(RuntimeError::Unconfigured(
        "session.search requires transcript deletion and resume/export continuity support"
            .to_string(),
    ))
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
