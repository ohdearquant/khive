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
