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
mod tests {
    use khive_runtime::{KhiveRuntime, Namespace, RuntimeError};
    use khive_storage::types::{SqlStatement, SqlValue};
    use rusqlite::{params, Connection, StatementStatus};
    use serde_json::json;

    use super::{handle_search, handle_search_with_scope, search_ready, SEARCH_SQL};

    #[tokio::test]
    async fn search_plan_limits_sessions_and_snippet_rows_before_fts_text_reads() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let mut reader = runtime.sql().reader().await.expect("reader");
        let plan = reader
            .query_all(SqlStatement {
                sql: format!("EXPLAIN QUERY PLAN {SEARCH_SQL}"),
                params: vec![
                    SqlValue::Text("\"needle\"".into()),
                    SqlValue::Text("local".into()),
                    SqlValue::Null,
                    SqlValue::Null,
                    SqlValue::Null,
                    SqlValue::Integer(20),
                ],
                label: Some("session_search_fts_plan".into()),
            })
            .await
            .expect("search plan");
        let details: Vec<&str> = plan
            .iter()
            .filter_map(|row| match row.get("detail") {
                Some(SqlValue::Text(detail)) => Some(detail.as_str()),
                _ => None,
            })
            .collect();
        for stage in [
            "MATERIALIZE hits",
            "MATERIALIZE limited_sessions",
            "MATERIALIZE top_snippet_rowids",
            "MATERIALIZE selected_snippets",
        ] {
            assert!(
                details.iter().any(|detail| detail.contains(stage)),
                "search must retain the {stage} stage: {details:?}"
            );
        }
        let fts_scan = details
            .iter()
            .position(|detail| detail.contains("session_messages_fts VIRTUAL TABLE"))
            .expect("MATCH must drive the first stage through FTS");
        let mirror_lookup = details
            .iter()
            .position(|detail| detail.contains("SEARCH m USING INTEGER PRIMARY KEY"))
            .expect("each FTS hit must seek one mirror row by primary key");
        assert!(
            fts_scan < mirror_lookup,
            "FTS must drive mirror-row lookups: {details:?}"
        );
        let fts_paths: Vec<&str> = details
            .iter()
            .copied()
            .filter(|detail| detail.contains("session_messages_fts VIRTUAL TABLE INDEX"))
            .collect();
        assert_eq!(
            fts_paths.len(),
            2,
            "one ranking scan and one snippet probe: {details:?}"
        );
        let snippet_index = fts_paths[1]
            .split("VIRTUAL TABLE INDEX")
            .nth(1)
            .expect("selected snippet FTS access path");
        assert!(
            snippet_index.contains('M') && snippet_index.contains('='),
            "snippet FTS access must combine MATCH with rowid equality: {details:?}"
        );

        let (ranking_sql, snippet_sql) = SEARCH_SQL
            .split_once("), selected_snippets AS MATERIALIZED (")
            .expect("snippet stage must follow the bounded stages");
        assert!(!ranking_sql.contains("snippet(session_messages_fts"));
        assert!(snippet_sql.contains("snippet(session_messages_fts"));
    }

    #[test]
    fn search_sql_snippet_vm_steps_stay_bounded_as_matches_grow() {
        let mut conn = Connection::open_in_memory().expect("in-memory SQLite");
        conn.execute_batch(
            "CREATE TABLE sessions ( \
               namespace TEXT, source TEXT, provider_session_id TEXT, cwd TEXT, \
               first_seen_at INTEGER, last_seen_at INTEGER, message_count INTEGER); \
             CREATE TABLE session_messages ( \
               mirror_rowid INTEGER PRIMARY KEY, namespace TEXT, source TEXT, \
               session_id TEXT, created_at INTEGER, text TEXT); \
             CREATE VIRTUAL TABLE session_messages_fts USING \
               fts5(text, content='session_messages', content_rowid='mirror_rowid'); \
             INSERT INTO sessions VALUES ('local', 'codex', 'one', '/repo', 1, 1, 1000);",
        )
        .expect("FTS fixture");

        fn insert_matches(conn: &mut Connection, first: i64, last: i64) {
            let tx = conn.transaction().expect("fixture transaction");
            for rowid in first..=last {
                tx.execute(
                    "INSERT INTO session_messages VALUES \
                     (?1, 'local', 'codex', 'one', ?1, 'needle message')",
                    params![rowid],
                )
                .expect("mirror row");
                tx.execute(
                    "INSERT INTO session_messages_fts(rowid, text) \
                     VALUES (?1, 'needle message')",
                    params![rowid],
                )
                .expect("FTS row");
            }
            tx.commit().expect("commit matching messages");
        }

        fn search_steps(conn: &Connection, sql: &str) -> i32 {
            let mut stmt = conn.prepare(sql).expect("SEARCH_SQL statement");
            let mut rows = stmt
                .query(params![
                    "\"needle\"",
                    "local",
                    None::<&str>,
                    None::<i64>,
                    None::<&str>,
                    1_i64,
                ])
                .expect("search query");
            let mut sessions = 0;
            while let Some(row) = rows.next().expect("search row") {
                let snippets: String = row.get(7).expect("snippets JSON");
                let snippets: Vec<String> = serde_json::from_str(&snippets).expect("snippet list");
                assert_eq!(snippets.len(), 3, "the returned snippet limit stays fixed");
                sessions += 1;
            }
            assert_eq!(sessions, 1, "fixture has one matching session");
            drop(rows);
            stmt.get_status(StatementStatus::VmStep)
        }

        let snippet_call = "snippet(session_messages_fts, 0, '[', ']', '…', 32)";
        assert_eq!(
            SEARCH_SQL.matches(snippet_call).count(),
            1,
            "the measured statement has one snippet stage"
        );
        let literal_sql = SEARCH_SQL.replacen(snippet_call, "'fixed snippet'", 1);

        insert_matches(&mut conn, 1, 10);
        let small_search_steps = search_steps(&conn, SEARCH_SQL);
        let small_literal_steps = search_steps(&conn, &literal_sql);
        insert_matches(&mut conn, 11, 1000);
        let large_search_steps = search_steps(&conn, SEARCH_SQL);
        let large_literal_steps = search_steps(&conn, &literal_sql);

        let small_snippet_steps = small_search_steps - small_literal_steps;
        let large_snippet_steps = large_search_steps - large_literal_steps;
        assert!(
            large_search_steps > small_search_steps + 500,
            "SEARCH_SQL must visit the additional matches: \
             {small_search_steps} -> {large_search_steps} VM steps"
        );
        assert!(
            small_snippet_steps > 0 && large_snippet_steps > 0,
            "the real snippet call must add VM work over a literal: \
             small {small_search_steps} vs {small_literal_steps}, \
             large {large_search_steps} vs {large_literal_steps} steps"
        );
        // Both statements rank the same matches. Only the selected snippet()
        // calls differ, so three calls may add fixed work while 990 extra hits
        // cannot add snippet work. The 128-step allowance covers fixed FTS
        // setup differences; a late LIMIT computes 990 extra snippets and
        // exceeds it even if each call adds just one VM opcode.
        assert!(
            large_snippet_steps <= small_snippet_steps + 128,
            "SEARCH_SQL must bound snippet work to three rows as matches grow \
             from 10 to 1000: full {small_search_steps} -> {large_search_steps}, \
             literal {small_literal_steps} -> {large_literal_steps}, \
             snippet delta {small_snippet_steps} -> {large_snippet_steps} VM steps"
        );
    }

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
                   ('event2','a','codex','same',1,'user',1,'scopedneedle bravo','{}','h5'),
                   ('event3','a','codex','same',2,'user',1,'scopedneedle delta','{}','h6'),
                   ('event4','a','codex','same',3,'user',1,'scopedneedle echo','{}','h7'),
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
        assert_eq!(
            codex_only["results"][0]["snippets"]
                .as_array()
                .expect("bounded snippets")
                .len(),
            3,
            "four matching messages still return only the best three snippets"
        );

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
