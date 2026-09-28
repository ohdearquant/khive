//! Tenant-scoped FTS5 search over mirrored session messages.
//!
//! The public verb remains gated until transcript deletion and resume/export
//! continuity support are available. `search_ready` is the row-level
//! implementation that those follow-ons can expose after their gates pass.

use std::num::NonZeroUsize;

use chrono::DateTime;
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
const SEARCH_SQL: &str = r#"
WITH hits AS MATERIALIZED (
  SELECT m.namespace, m.source, m.session_id, m.created_at,
         bm25(session_messages_fts) AS fts_rank,
         snippet(session_messages_fts, 0, '[', ']', '…', 32) AS snippet
    FROM session_messages_fts
    JOIN session_messages AS m ON m.mirror_rowid = session_messages_fts.rowid
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
)
SELECT r.namespace, r.source, r.session_id AS provider_session_id,
       s.cwd, s.first_seen_at, s.last_seen_at, s.message_count,
       (SELECT json_group_array(snippet) FROM (
          SELECT h.snippet FROM hits AS h
           WHERE h.namespace = r.namespace AND h.source = r.source
             AND h.session_id = r.session_id
           ORDER BY h.fts_rank ASC, h.created_at DESC LIMIT 3
       )) AS snippets
  FROM session_ranks AS r
  LEFT JOIN sessions AS s
    ON s.namespace = ?2 AND s.namespace = r.namespace
   AND s.source = r.source AND s.provider_session_id = r.session_id
 ORDER BY r.best_rank ASC, r.source ASC, r.session_id ASC
 LIMIT ?6"#;

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
    let limit = match p.limit {
        None => DEFAULT_LIMIT,
        Some(n) if (1..=MAX_LIMIT).contains(&n) => n,
        Some(n) => {
            return Err(RuntimeError::InvalidInput(format!(
                "{VERB}: limit must be in 1..={MAX_LIMIT}; got {n}"
            )))
        }
    };
    if let Some(source) = p.source.as_deref() {
        if !VALID_SOURCES.contains(&source) {
            return Err(RuntimeError::InvalidInput(format!(
                "{VERB}: source must be one of {}; got {source:?}",
                VALID_SOURCES.join(", ")
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
                        "{VERB}: since must be an RFC 3339 timestamp; got {raw:?}"
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

fn required_text(row: &SqlRow, column: &str) -> Result<String, RuntimeError> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        _ => Err(RuntimeError::Internal(format!(
            "{VERB}: malformed mirror search row: {column} must be text"
        ))),
    }
}

fn optional_text(row: &SqlRow, column: &str) -> Result<Option<String>, RuntimeError> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        Some(SqlValue::Null) => Ok(None),
        _ => Err(RuntimeError::Internal(format!(
            "{VERB}: malformed mirror search row: {column} must be text or null"
        ))),
    }
}

fn optional_integer(row: &SqlRow, column: &str) -> Result<Option<i64>, RuntimeError> {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => Ok(Some(*value)),
        Some(SqlValue::Null) => Ok(None),
        _ => Err(RuntimeError::Internal(format!(
            "{VERB}: malformed mirror search row: {column} must be integer or null"
        ))),
    }
}

fn decode_hit(row: &SqlRow, rank: usize) -> Result<SearchHit, RuntimeError> {
    let snippets_json = required_text(row, "snippets")?;
    let snippets: Vec<String> = serde_json::from_str(&snippets_json).map_err(|e| {
        RuntimeError::Internal(format!("{VERB}: malformed mirror search snippets: {e}"))
    })?;
    let one_based = NonZeroUsize::new(rank + 1).expect("enumerate index plus one is nonzero");
    Ok(SearchHit {
        session: SearchSession {
            provider_session_id: required_text(row, "provider_session_id")?,
            source: required_text(row, "source")?,
            namespace: required_text(row, "namespace")?,
            cwd: optional_text(row, "cwd")?,
            first_seen_at: optional_integer(row, "first_seen_at")?.map(micros_to_iso),
            last_seen_at: optional_integer(row, "last_seen_at")?.map(micros_to_iso),
            message_count: optional_integer(row, "message_count")?,
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
mod tests {
    use khive_runtime::{KhiveRuntime, Namespace, RuntimeError};
    use serde_json::json;

    use super::{handle_search, handle_search_with_scope, search_ready};

    #[tokio::test]
    async fn absent_scope_is_permission_denied_at_handler_seam_even_with_default_allow_gate() {
        // KhiveRuntime::memory uses AllowAllGate. The sealed token cannot
        // represent an absent scope, so exercise the handler's scope seam
        // directly: the allow decision cannot turn None into an unscoped SQL.
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        runtime
            .authorize(Namespace::local())
            .expect("AllowAllGate authorizes the local scope");
        for scope in [None, Some(""), Some("  "), Some("\t")] {
            let err = handle_search_with_scope(&runtime, scope, json!({"query": "needle"}))
                .await
                .unwrap_err();
            assert!(
                matches!(err, RuntimeError::PermissionDenied { .. }),
                "no positive tenant scope must be PermissionDenied"
            );
        }
    }

    #[tokio::test]
    async fn public_handler_is_dependency_gated_and_rejects_caller_scope() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("local token");
        let gated = handle_search(&runtime, &token, json!({"query": "needle"}))
            .await
            .unwrap_err();
        assert!(matches!(gated, RuntimeError::Unconfigured(_)));

        let forged = handle_search(
            &runtime,
            &token,
            json!({"query": "needle", "namespace": "other"}),
        )
        .await
        .unwrap_err();
        assert!(matches!(forged, RuntimeError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn query_returns_only_token_scope_and_keeps_sources_distinct() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = runtime.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute_script(
                r#"INSERT INTO sessions
                   (id, namespace, source, provider_session_id, cwd,
                    first_seen_at, last_seen_at, message_count) VALUES
                   ('same','a','codex','same','/a',1,1,1),
                   ('same','a','claude_code','same','/a',1,1,1),
                   ('same','b','codex','same','/b',1,1,1);
                 INSERT INTO session_messages
                   (id, namespace, source, session_id, seq, msg_type,
                    created_at, text, raw, content_hash) VALUES
                   ('event','a','codex','same',0,'user',1,'scopedneedle alpha','{}','h1'),
                   ('event','a','claude_code','same',0,'user',1,'scopedneedle beta','{}','h2'),
                   ('event','b','codex','same',0,'user',1,'scopedneedle gamma','{}','h3'),
                   ('lost','a','unknown','orphan',0,'user',1,'scopedneedle orphan','{}','h4');"#
                    .to_string(),
            )
            .await
            .expect("search fixture");
        drop(writer);

        let token_a = runtime
            .authorize(Namespace::parse("a").unwrap())
            .expect("scope a");
        let token_b = runtime
            .authorize(Namespace::parse("b").unwrap())
            .expect("scope b");
        let result_a = search_ready(&runtime, &token_a, json!({"query":"scopedneedle"}))
            .await
            .expect("search a");
        let rows_a = result_a["results"].as_array().expect("results");
        assert_eq!(
            rows_a.len(),
            2,
            "session.search must not return another namespace"
        );
        assert!(rows_a.iter().all(|hit| hit["session"]["namespace"] == "a"));
        assert!(rows_a.iter().any(|hit| hit["session"]["source"] == "codex"));
        assert!(rows_a
            .iter()
            .any(|hit| hit["session"]["source"] == "claude_code"));

        let codex_only = search_ready(
            &runtime,
            &token_a,
            json!({"query":"scopedneedle","source":"codex","cwd":"/a"}),
        )
        .await
        .expect("source and cwd narrow the scoped search");
        assert_eq!(codex_only["results"].as_array().unwrap().len(), 1);
        assert_eq!(codex_only["results"][0]["session"]["source"], "codex");

        let too_recent = search_ready(
            &runtime,
            &token_a,
            json!({"query":"scopedneedle","since":"1970-01-01T00:00:00.000002Z"}),
        )
        .await
        .expect("since narrows the scoped search");
        assert!(too_recent["results"].as_array().unwrap().is_empty());

        let result_b = search_ready(&runtime, &token_b, json!({"query":"scopedneedle"}))
            .await
            .expect("search b");
        let rows_b = result_b["results"].as_array().expect("results");
        assert_eq!(rows_b.len(), 1);
        assert_eq!(rows_b[0]["session"]["namespace"], "b");

        let unknown = search_ready(
            &runtime,
            &token_a,
            json!({"query":"scopedneedle","source":"unknown"}),
        )
        .await
        .expect("explicit orphan audit");
        assert_eq!(unknown["results"].as_array().unwrap().len(), 1);
        assert_eq!(
            unknown["results"][0]["session"]["provider_session_id"],
            "orphan"
        );
        assert_eq!(unknown["results"][0]["session"]["source"], "unknown");
        assert!(unknown["results"][0]["session"]["cwd"].is_null());
    }
}
