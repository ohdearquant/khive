//! Idempotent file tail + upsert into the session mirror tables.
//!
//! `mirror_file` reads new bytes from a JSONL file starting at `start_offset`,
//! parses complete lines using the parser selected by `MirrorSource`, and writes
//! them to the session mirror tables in a single transaction.  It is safe to call
//! repeatedly on the same file; `INSERT OR IGNORE` keyed by the event UUID ensures
//! idempotency.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use chrono::Utc;
use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlStatement, SqlTxOptions, SqlValue};

use super::parse;

/// Identifies which CLI produced the JSONL file being mirrored.
///
/// The value is written to `sessions.source` and selects the line parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorSource {
    /// Claude Code (`~/.claude/projects/<slug>/<uuid>.jsonl`).
    ClaudeCode,
    /// Codex CLI (`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`).
    Codex,
}

impl MirrorSource {
    /// The string written to `sessions.source`.
    pub fn as_str(self) -> &'static str {
        match self {
            MirrorSource::ClaudeCode => "claude_code",
            MirrorSource::Codex => "codex",
        }
    }
}

/// Statistics returned by a single `mirror_file` call.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MirrorStats {
    /// Number of new message rows inserted (0 if all were already present).
    pub inserted: u64,
    /// Number of complete lines scanned (including duplicates).
    pub scanned: u64,
    /// Byte offset advanced to (only past complete lines; partial trailing line excluded).
    pub new_offset: u64,
}

/// Read new bytes of `path` starting at `start_offset`, parse complete lines
/// using the parser selected by `source`, and upsert them idempotently into the
/// session mirror tables.
///
/// For `MirrorSource::Codex`, `codex_session_id` must be the session UUID
/// derived from the filename; it is used both to key the `sessions` row and to
/// synthesise per-line event UUIDs (`"{session_id}:{abs_byte_offset}"`).
/// For `MirrorSource::ClaudeCode`, `codex_session_id` is ignored (the session
/// UUID is embedded in each line).
///
/// Returns stats including the advanced byte offset.  A partial trailing line
/// (no terminating `\n`) is left for the next poll — `new_offset` is set to
/// the byte after the last complete `\n`.
///
/// One bad file or one bad line does NOT kill the loop: per-file errors propagate
/// to the caller (the service loop logs and continues); per-line parse failures
/// are silently skipped (the parser returns `None`).
pub async fn mirror_file(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    source: MirrorSource,
    codex_session_id: Option<&str>,
) -> Result<MirrorStats, RuntimeError> {
    // ── read new bytes ────────────────────────────────────────────────────────

    let content = read_from_offset(path, start_offset).map_err(|e| {
        RuntimeError::Internal(format!(
            "mirror_file: failed to read {:?} at offset {start_offset}: {e}",
            path
        ))
    })?;

    if content.is_empty() {
        return Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: start_offset,
        });
    }

    // ── find last complete line (ends in '\n') ────────────────────────────────

    let last_newline = content
        .iter()
        .enumerate()
        .rev()
        .find(|(_, &b)| b == b'\n')
        .map(|(i, _)| i);

    let (complete_bytes, partial_len) = match last_newline {
        Some(pos) => (pos + 1, content.len() - pos - 1),
        None => {
            // All bytes form a partial line — nothing to consume.
            return Ok(MirrorStats {
                inserted: 0,
                scanned: 0,
                new_offset: start_offset,
            });
        }
    };

    let new_offset = start_offset + complete_bytes as u64;
    let _ = partial_len; // not needed beyond the offset calculation above

    // ── parse complete lines ──────────────────────────────────────────────────

    let complete_content = String::from_utf8_lossy(&content[..complete_bytes]);

    // For Codex lines we need the absolute byte offset of each line start so
    // the synthesised event UUID is stable across re-tails.  Compute line
    // boundaries once, then iterate with offsets.
    let events: Vec<parse::ParsedEvent> = match source {
        MirrorSource::ClaudeCode => complete_content
            .split('\n')
            .filter(|l| !l.is_empty())
            .filter_map(parse::parse_cc_line)
            .collect(),
        MirrorSource::Codex => {
            let sid = codex_session_id.unwrap_or("");
            let mut line_start: u64 = start_offset;
            let mut evs = Vec::new();
            for raw_line in complete_content.split('\n') {
                let line_byte_len = raw_line.len() as u64 + 1; // +1 for the '\n'
                if !raw_line.is_empty() {
                    if let Some(ev) = parse::parse_codex_line(raw_line, sid, line_start) {
                        evs.push(ev);
                    }
                }
                line_start += line_byte_len;
            }
            evs
        }
    };

    let scanned = complete_content
        .split('\n')
        .filter(|l| !l.is_empty())
        .count() as u64;

    if events.is_empty() {
        // Apply cursor update even when there are no parseable events so we
        // don't re-read the same bytes on the next tick.
        let _ = write_cursor_only(runtime, path, &None, new_offset).await;
        return Ok(MirrorStats {
            inserted: 0,
            scanned,
            new_offset,
        });
    }

    // ── write in one transaction ──────────────────────────────────────────────

    let now_us = Utc::now().timestamp_micros();
    let sql = runtime.sql();

    let mut tx = sql
        .begin_tx(SqlTxOptions::default())
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror_file: begin_tx: {e}")))?;

    let mut inserted: u64 = 0;
    let mut last_session_id: Option<String> = None;

    for ev in &events {
        let created_at = if ev.created_at_micros != 0 {
            ev.created_at_micros
        } else {
            now_us
        };

        // ── sessions row: create-only ─────────────────────────────────────────
        //
        // First sight of a session creates the row (first_seen_at = last_seen_at =
        // this event's timestamp). Replays are a cheap no-op (`DO NOTHING`), so a
        // pass that inserts no new messages writes no session metadata at all —
        // strict replay idempotency. `last_seen_at` is advanced below, but only
        // when a genuinely new message lands.
        tx.execute(SqlStatement {
            sql: format!(
                "INSERT INTO sessions \
                  (id, provider_session_id, source, cwd, git_branch, slug, \
                   message_count, first_seen_at, last_seen_at, namespace) \
                  VALUES(?1, ?1, '{}', ?2, ?3, ?4, 0, ?5, ?5, 'local') \
                  ON CONFLICT(id) DO NOTHING",
                source.as_str()
            ),
            params: vec![
                SqlValue::Text(ev.session_id.clone()),
                ev.cwd
                    .as_deref()
                    .map(|s| SqlValue::Text(s.to_string()))
                    .unwrap_or(SqlValue::Null),
                ev.git_branch
                    .as_deref()
                    .map(|s| SqlValue::Text(s.to_string()))
                    .unwrap_or(SqlValue::Null),
                ev.slug
                    .as_deref()
                    .map(|s| SqlValue::Text(s.to_string()))
                    .unwrap_or(SqlValue::Null),
                SqlValue::Integer(created_at),
            ],
            label: Some("session_mirror_create_session".into()),
        })
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror_file: session create: {e}")))?;

        // ── session_messages insert (idempotent) ──────────────────────────────
        let affected = tx
            .execute(SqlStatement {
                sql: "INSERT OR IGNORE INTO session_messages \
                      (id, session_id, seq, parent_uuid, is_sidechain, role, \
                       msg_type, text, raw, created_at, namespace) \
                      VALUES(?1, ?2, \
                        (SELECT COALESCE(MAX(seq),-1)+1 FROM session_messages WHERE session_id=?2), \
                        ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'local')"
                    .into(),
                params: vec![
                    SqlValue::Text(ev.uuid.clone()),
                    SqlValue::Text(ev.session_id.clone()),
                    ev.parent_uuid
                        .as_deref()
                        .map(|s| SqlValue::Text(s.to_string()))
                        .unwrap_or(SqlValue::Null),
                    SqlValue::Integer(i64::from(ev.is_sidechain)),
                    ev.role
                        .as_deref()
                        .map(|s| SqlValue::Text(s.to_string()))
                        .unwrap_or(SqlValue::Null),
                    SqlValue::Text(ev.msg_type.clone()),
                    ev.text
                        .as_deref()
                        .map(|s| SqlValue::Text(s.to_string()))
                        .unwrap_or(SqlValue::Null),
                    SqlValue::Text(ev.raw.clone()),
                    SqlValue::Integer(created_at),
                ],
                label: Some("session_mirror_insert_message".into()),
            })
            .await
            .map_err(|e| RuntimeError::Internal(format!("mirror_file: message insert: {e}")))?;

        // ── advance session metadata ONLY when a new message landed ────────────
        //
        // Keeps `last_seen_at` monotonic (`MAX`) so a timestamp-missing replay
        // (whose `created_at` fell back to `now_us`) cannot move it forward, and
        // backfills metadata that may have been NULL at create time. A pure
        // replay (`affected == 0`) touches nothing.
        if affected > 0 {
            tx.execute(SqlStatement {
                sql: "UPDATE sessions SET \
                        last_seen_at=MAX(last_seen_at, ?2), \
                        cwd=COALESCE(cwd, ?3), \
                        git_branch=COALESCE(git_branch, ?4), \
                        slug=COALESCE(slug, ?5) \
                      WHERE id=?1"
                    .into(),
                params: vec![
                    SqlValue::Text(ev.session_id.clone()),
                    SqlValue::Integer(created_at),
                    ev.cwd
                        .as_deref()
                        .map(|s| SqlValue::Text(s.to_string()))
                        .unwrap_or(SqlValue::Null),
                    ev.git_branch
                        .as_deref()
                        .map(|s| SqlValue::Text(s.to_string()))
                        .unwrap_or(SqlValue::Null),
                    ev.slug
                        .as_deref()
                        .map(|s| SqlValue::Text(s.to_string()))
                        .unwrap_or(SqlValue::Null),
                ],
                label: Some("session_mirror_touch_session".into()),
            })
            .await
            .map_err(|e| RuntimeError::Internal(format!("mirror_file: session touch: {e}")))?;
        }

        inserted += affected;
        last_session_id = Some(ev.session_id.clone());
    }

    // ── refresh message_count for each distinct session ───────────────────────
    //
    // In practice one JSONL file maps to one session_id, but we refresh every
    // session_id we touched to stay correct even if that changes. Skipped
    // entirely on a pure replay (`inserted == 0`): the counts cannot have
    // changed, so writing them would be needless churn.
    if inserted > 0 {
        let mut seen_sessions: Vec<String> = events
            .iter()
            .map(|e| e.session_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        seen_sessions.sort(); // deterministic order for tests

        for sid in &seen_sessions {
            tx.execute(SqlStatement {
                sql: "UPDATE sessions SET message_count=\
                      (SELECT COUNT(*) FROM session_messages WHERE session_id=?1) \
                      WHERE id=?1"
                    .into(),
                params: vec![SqlValue::Text(sid.clone())],
                label: Some("session_mirror_refresh_count".into()),
            })
            .await
            .map_err(|e| RuntimeError::Internal(format!("mirror_file: count refresh: {e}")))?;
        }
    }

    // ── cursor upsert ─────────────────────────────────────────────────────────
    let path_str = path.to_string_lossy().into_owned();
    tx.execute(SqlStatement {
        sql: "INSERT INTO session_mirror_cursor(file_path, session_id, byte_offset, updated_at) \
              VALUES(?1, ?2, ?3, ?4) \
              ON CONFLICT(file_path) DO UPDATE SET \
                session_id=excluded.session_id, \
                byte_offset=excluded.byte_offset, \
                updated_at=excluded.updated_at"
            .into(),
        params: vec![
            SqlValue::Text(path_str),
            last_session_id
                .as_deref()
                .map(|s| SqlValue::Text(s.to_string()))
                .unwrap_or(SqlValue::Null),
            SqlValue::Integer(new_offset as i64),
            SqlValue::Integer(now_us),
        ],
        label: Some("session_mirror_cursor_upsert".into()),
    })
    .await
    .map_err(|e| RuntimeError::Internal(format!("mirror_file: cursor upsert: {e}")))?;

    // ── commit ────────────────────────────────────────────────────────────────
    tx.commit()
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror_file: commit: {e}")))?;

    Ok(MirrorStats {
        inserted,
        scanned,
        new_offset,
    })
}

/// Read bytes from `path` starting at `offset` to EOF.
///
/// Returns an empty Vec when `offset` is at or past EOF.
fn read_from_offset(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    if offset >= file_len {
        return Ok(Vec::new());
    }
    file.seek(SeekFrom::Start(offset))?;
    let capacity = (file_len - offset) as usize;
    let mut buf = Vec::with_capacity(capacity);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Write only the cursor row (no events).  Used when there are no parseable
/// events so the offset still advances past blank/unparseable content.
async fn write_cursor_only(
    runtime: &KhiveRuntime,
    path: &Path,
    session_id: &Option<String>,
    new_offset: u64,
) -> Result<(), RuntimeError> {
    let now_us = Utc::now().timestamp_micros();
    let path_str = path.to_string_lossy().into_owned();
    let sql = runtime.sql();
    let mut w = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror_file: cursor writer: {e}")))?;
    w.execute(SqlStatement {
        sql: "INSERT INTO session_mirror_cursor(file_path, session_id, byte_offset, updated_at) \
              VALUES(?1, ?2, ?3, ?4) \
              ON CONFLICT(file_path) DO UPDATE SET \
                session_id=COALESCE(excluded.session_id, session_mirror_cursor.session_id), \
                byte_offset=excluded.byte_offset, \
                updated_at=excluded.updated_at"
            .into(),
        params: vec![
            SqlValue::Text(path_str),
            session_id
                .as_deref()
                .map(|s| SqlValue::Text(s.to_string()))
                .unwrap_or(SqlValue::Null),
            SqlValue::Integer(new_offset as i64),
            SqlValue::Integer(now_us),
        ],
        label: Some("session_mirror_cursor_only".into()),
    })
    .await
    .map_err(|e| RuntimeError::Internal(format!("mirror_file: cursor write: {e}")))?;
    Ok(())
}

// ── integration tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;

    use khive_runtime::{
        AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
    };
    use khive_storage::types::{SqlStatement, SqlValue};
    use tempfile::{NamedTempFile, TempDir};

    use super::*;
    use crate::vocab::SESSION_SCHEMA_PLAN_STMTS;

    /// Build a file-backed runtime and apply the session schema.
    ///
    /// `begin_tx` (used by `mirror_file`) requires a file-backed SQLite database;
    /// in-memory SQLite does not support the WAL-mode transactions that `begin_tx`
    /// opens.  The caller must keep the returned `TempDir` alive for the test.
    async fn setup() -> (KhiveRuntime, TempDir) {
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
        apply_session_schema(&rt).await;
        (rt, dir)
    }

    async fn apply_session_schema(rt: &KhiveRuntime) {
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        for stmt in &SESSION_SCHEMA_PLAN_STMTS {
            w.execute_script(stmt.to_string())
                .await
                .expect("schema stmt");
        }
        // w dropped here — releases the writer connection.
    }

    /// Count rows in a table.
    async fn count_rows(rt: &KhiveRuntime, table: &str) -> i64 {
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: format!("SELECT COUNT(*) FROM {table}"),
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

    /// Retrieve the stored byte_offset for a file path.
    async fn cursor_offset(rt: &KhiveRuntime, path_str: &str) -> Option<i64> {
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path=?1".into(),
                params: vec![SqlValue::Text(path_str.to_string())],
                label: None,
            })
            .await
            .expect("cursor query")?;
        match row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Integer(n)) => Some(*n),
            _ => None,
        }
    }

    fn user_line(uuid: &str, session_id: &str, text: &str) -> String {
        format!(
            r#"{{"uuid":"{uuid}","sessionId":"{session_id}","type":"user","timestamp":"2026-06-29T10:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    /// A user line with NO `timestamp` field — `created_at` falls back to `now_us`.
    fn user_line_no_ts(uuid: &str, session_id: &str, text: &str) -> String {
        format!(
            r#"{{"uuid":"{uuid}","sessionId":"{session_id}","type":"user","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    /// Retrieve the stored `last_seen_at` for a session id.
    async fn last_seen_at(rt: &KhiveRuntime, session_id: &str) -> Option<i64> {
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT last_seen_at FROM sessions WHERE id=?1".into(),
                params: vec![SqlValue::Text(session_id.to_string())],
                label: None,
            })
            .await
            .expect("last_seen query")?;
        match row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Integer(n)) => Some(*n),
            _ => None,
        }
    }

    #[tokio::test]
    async fn test_mirror_three_lines_and_idempotency() {
        let (rt, _dir) = setup().await;

        // Build a fixture JSONL with 3 lines, all ending in '\n'.
        let line1 = user_line("uuid-1", "sess-A", "Hello");
        let line2 = user_line("uuid-2", "sess-A", "World");
        let line3 = user_line("uuid-3", "sess-A", "Done");

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{line1}").unwrap();
        writeln!(file, "{line2}").unwrap();
        writeln!(file, "{line3}").unwrap();

        let path = file.path().to_path_buf();

        // First call: should insert all 3 rows.
        let stats = mirror_file(&rt, &path, 0, MirrorSource::ClaudeCode, None)
            .await
            .expect("mirror_file first call");
        assert_eq!(stats.inserted, 3, "should insert 3 new messages");
        assert_eq!(stats.scanned, 3, "should scan 3 lines");
        assert!(stats.new_offset > 0, "offset should advance");

        let msg_count = count_rows(&rt, "session_messages").await;
        assert_eq!(msg_count, 3, "3 messages in DB");

        let session_count = count_rows(&rt, "sessions").await;
        assert_eq!(session_count, 1, "1 session row");

        // Idempotency: second call over the SAME range inserts 0 rows.
        let stats2 = mirror_file(&rt, &path, 0, MirrorSource::ClaudeCode, None)
            .await
            .expect("mirror_file second call");
        assert_eq!(stats2.inserted, 0, "second pass must insert 0 rows");
        assert_eq!(count_rows(&rt, "session_messages").await, 3);

        // Offset-aware: calling from the advanced offset finds nothing new.
        let stats3 = mirror_file(&rt, &path, stats.new_offset, MirrorSource::ClaudeCode, None)
            .await
            .expect("mirror_file from new_offset");
        assert_eq!(stats3.inserted, 0, "no new data past advanced offset");
        assert_eq!(stats3.new_offset, stats.new_offset);

        // Cursor was recorded.
        let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
        assert!(stored_offset.is_some(), "cursor should be recorded");
        assert_eq!(stored_offset.unwrap(), stats.new_offset as i64);
    }

    #[tokio::test]
    async fn test_partial_trailing_line_not_consumed() {
        let (rt, _dir) = setup().await;

        let line1 = user_line("uuid-p1", "sess-B", "Complete");
        // Write one complete line + a partial line without trailing '\n'.
        let partial = r#"{"uuid":"uuid-p2","sessionId":"sess-B","type":"user""#;

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{line1}").unwrap(); // complete line (has \n)
        write!(file, "{partial}").unwrap(); // partial — NO trailing \n

        let path = file.path().to_path_buf();
        let full_len = std::fs::metadata(&path).unwrap().len();

        let stats = mirror_file(&rt, &path, 0, MirrorSource::ClaudeCode, None)
            .await
            .expect("mirror_file partial");

        // Only the complete line should be consumed.
        assert_eq!(stats.inserted, 1, "only 1 complete line inserted");
        assert!(
            stats.new_offset < full_len,
            "new_offset {new} must be less than file_len {full_len}",
            new = stats.new_offset
        );

        // The partial bytes remain; calling again from new_offset finds no complete lines.
        let stats2 = mirror_file(&rt, &path, stats.new_offset, MirrorSource::ClaudeCode, None)
            .await
            .expect("second call");
        assert_eq!(
            stats2.inserted, 0,
            "partial line must not be consumed on re-poll"
        );
        assert_eq!(
            stats2.new_offset, stats.new_offset,
            "offset must not advance on partial-only content"
        );
    }

    #[tokio::test]
    async fn test_duplicate_uuid_across_two_calls() {
        let (rt, _dir) = setup().await;

        let line = user_line("uuid-dup", "sess-C", "First");

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{line}").unwrap();

        let path = file.path().to_path_buf();

        // First call inserts.
        let s1 = mirror_file(&rt, &path, 0, MirrorSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s1.inserted, 1);

        // Append same uuid again.
        writeln!(file, "{line}").unwrap();

        // Second call from offset 0 should see both lines but insert 0 new rows.
        let s2 = mirror_file(&rt, &path, 0, MirrorSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s2.inserted, 0, "duplicate uuid must not be re-inserted");
        assert_eq!(count_rows(&rt, "session_messages").await, 1);

        // Incremental: call from first call's new_offset; the second line is the dup.
        let s3 = mirror_file(&rt, &path, s1.new_offset, MirrorSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s3.inserted, 0, "incremental dup must also insert 0");
    }

    #[tokio::test]
    async fn test_replay_does_not_mutate_session_metadata() {
        // Regression for the replay-idempotency finding: a timestamp-missing
        // event's `created_at` falls back to `now_us`, which differs between
        // calls. A pure replay (0 new messages) must NOT advance `last_seen_at`
        // or otherwise touch the session row.
        let (rt, _dir) = setup().await;

        let line = user_line_no_ts("uuid-nts", "sess-NTS", "no timestamp here");
        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{line}").unwrap();
        let path = file.path().to_path_buf();

        let s1 = mirror_file(&rt, &path, 0, MirrorSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s1.inserted, 1);
        let seen_after_first = last_seen_at(&rt, "sess-NTS")
            .await
            .expect("session row exists");

        // Replay from offset 0: re-scans the same line, inserts 0, and must
        // leave last_seen_at byte-identical even though now_us has advanced.
        let s2 = mirror_file(&rt, &path, 0, MirrorSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s2.inserted, 0, "replay must insert 0 rows");
        let seen_after_replay = last_seen_at(&rt, "sess-NTS").await.unwrap();
        assert_eq!(
            seen_after_first, seen_after_replay,
            "replay must not advance last_seen_at for a timestamp-missing event"
        );
    }

    #[tokio::test]
    async fn test_empty_file_is_a_no_op() {
        let (rt, _dir) = setup().await;

        let file = NamedTempFile::new().expect("tmpfile");
        let path = file.path().to_path_buf();

        let stats = mirror_file(&rt, &path, 0, MirrorSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(stats.inserted, 0);
        assert_eq!(stats.scanned, 0);
        assert_eq!(stats.new_offset, 0);
    }

    #[tokio::test]
    async fn test_missing_file_returns_error() {
        let (rt, _dir) = setup().await;
        let bad_path = std::path::PathBuf::from("/nonexistent/path/session.jsonl");
        let result = mirror_file(&rt, &bad_path, 0, MirrorSource::ClaudeCode, None).await;
        assert!(
            matches!(result, Err(RuntimeError::Internal(_))),
            "missing file should return Internal error"
        );
    }

    // ── Codex source integration tests ────────────────────────────────────────

    /// Build a minimal Codex response_item/message line. Block type mirrors the
    /// real shape: `input_text` for user messages, `output_text` for assistant
    /// messages (the generic `text` type does not occur in real Codex transcripts).
    fn codex_message_line(role: &str, text: &str) -> String {
        let block_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        format!(
            r#"{{"type":"response_item","timestamp":"2026-06-30T08:00:00Z","payload":{{"type":"message","role":"{role}","content":[{{"type":"{block_type}","text":"{text}"}}]}}}}"#
        )
    }

    /// Build a minimal Codex session_meta line.
    fn codex_meta_line(session_id: &str, cwd: &str, branch: &str) -> String {
        format!(
            r#"{{"type":"session_meta","timestamp":"2026-06-30T08:00:00Z","payload":{{"id":"{session_id}","cwd":"{cwd}","git":{{"branch":"{branch}","commit_hash":"abc","repository_url":"https://github.com/example/repo"}}}}}}"#
        )
    }

    /// Build a Codex event_msg line (should be skipped).
    fn codex_event_msg_line() -> String {
        r#"{"type":"event_msg","timestamp":"2026-06-30T08:00:00Z","payload":{"type":"user_message","content":"should be skipped"}}"#.to_string()
    }

    #[tokio::test]
    async fn test_codex_mirror_inserts_with_source_codex() {
        let (rt, _dir) = setup().await;

        let session_id = "cdx-sess-0001-0001-0001-000000000001";
        let meta = codex_meta_line(session_id, "/home/lion/proj", "feat-x");
        let user_msg = codex_message_line("user", "Hello from Codex");
        let asst_msg = codex_message_line("assistant", "Hello back from Codex");
        let skip_msg = codex_event_msg_line();

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{meta}").unwrap();
        writeln!(file, "{user_msg}").unwrap();
        writeln!(file, "{asst_msg}").unwrap();
        writeln!(file, "{skip_msg}").unwrap();

        let path = file.path().to_path_buf();

        // Mirror the file as Codex source.
        let stats = mirror_file(&rt, &path, 0, MirrorSource::Codex, Some(session_id))
            .await
            .expect("codex mirror_file");

        // session_meta + 2 response_item/message rows = 3 parseable, event_msg skipped.
        assert_eq!(stats.inserted, 3, "meta + 2 messages inserted");
        assert_eq!(
            stats.scanned, 4,
            "4 lines total (including skipped event_msg)"
        );
        assert!(stats.new_offset > 0);

        // Session row exists with source='codex'.
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let session_row = r
            .query_row(SqlStatement {
                sql: "SELECT source FROM sessions WHERE id=?1".into(),
                params: vec![SqlValue::Text(session_id.to_string())],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("session row must exist");
        match session_row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Text(s)) => assert_eq!(s, "codex", "source must be 'codex'"),
            other => panic!("unexpected source value: {other:?}"),
        }

        // All 3 message rows are stored.
        assert_eq!(count_rows(&rt, "session_messages").await, 3);

        // The two response_item/message rows carry their real input_text/
        // output_text content through to session_messages.text — not just a
        // row count, but the actual extracted string for each role.
        let mut r2 = sql.reader().await.expect("reader");
        let rows = r2
            .query_all(SqlStatement {
                sql: "SELECT role, text FROM session_messages \
                      WHERE session_id=?1 AND role IS NOT NULL ORDER BY seq"
                    .into(),
                params: vec![SqlValue::Text(session_id.to_string())],
                label: None,
            })
            .await
            .expect("query ok");
        let texts: Vec<(String, String)> = rows
            .iter()
            .map(|row| {
                let role = match row.get("role") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected role value: {other:?}"),
                };
                let text = match row.get("text") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected text value: {other:?}"),
                };
                (role, text)
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                ("user".to_string(), "Hello from Codex".to_string()),
                ("assistant".to_string(), "Hello back from Codex".to_string()),
            ],
            "input_text/output_text blocks must round-trip to session_messages.text by role"
        );
    }

    #[tokio::test]
    async fn test_codex_event_id_is_stable_and_idempotent() {
        // Verifies that: (a) synthetic uuid format is "{session_id}:{offset}",
        // and (b) a second mirror_file pass over the same bytes inserts 0 rows.
        let (rt, _dir) = setup().await;

        let session_id = "cdx-sess-idem-0001-0001-000000000002";
        let user_msg = codex_message_line("user", "Idempotency test");

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{user_msg}").unwrap();

        let path = file.path().to_path_buf();

        // First pass.
        let s1 = mirror_file(&rt, &path, 0, MirrorSource::Codex, Some(session_id))
            .await
            .unwrap();
        assert_eq!(s1.inserted, 1);

        // Verify the stored id matches the expected synthetic format.
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let msg_row = r
            .query_row(SqlStatement {
                sql: "SELECT id FROM session_messages WHERE session_id=?1".into(),
                params: vec![SqlValue::Text(session_id.to_string())],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("message row must exist");
        let stored_id = match msg_row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Text(s)) => s.clone(),
            other => panic!("unexpected id type: {other:?}"),
        };
        // The line starts at byte offset 0.
        let expected_id = format!("{session_id}:0");
        assert_eq!(
            stored_id, expected_id,
            "synthetic uuid must be {{session_id}}:{{offset}}"
        );

        // Second pass from offset 0: same lines, 0 new rows (idempotent).
        let s2 = mirror_file(&rt, &path, 0, MirrorSource::Codex, Some(session_id))
            .await
            .unwrap();
        assert_eq!(s2.inserted, 0, "second pass must be idempotent");
        assert_eq!(count_rows(&rt, "session_messages").await, 1);

        // Incremental pass from advanced offset: no new data.
        let s3 = mirror_file(
            &rt,
            &path,
            s1.new_offset,
            MirrorSource::Codex,
            Some(session_id),
        )
        .await
        .unwrap();
        assert_eq!(s3.inserted, 0, "incremental pass finds nothing new");
    }

    #[tokio::test]
    async fn test_codex_and_cc_are_independent_sessions() {
        // Both sources can coexist in the same DB; source column distinguishes them.
        let (rt, _dir) = setup().await;

        let cc_line = user_line("cc-uuid-1", "cc-sess-1", "from claude code");
        let mut cc_file = NamedTempFile::new().expect("cc tmpfile");
        writeln!(cc_file, "{cc_line}").unwrap();

        let cdx_session_id = "cdx-sess-coex-0001-0001-000000000003";
        let cdx_msg = codex_message_line("user", "from codex");
        let mut cdx_file = NamedTempFile::new().expect("cdx tmpfile");
        writeln!(cdx_file, "{cdx_msg}").unwrap();

        mirror_file(&rt, cc_file.path(), 0, MirrorSource::ClaudeCode, None)
            .await
            .unwrap();

        mirror_file(
            &rt,
            cdx_file.path(),
            0,
            MirrorSource::Codex,
            Some(cdx_session_id),
        )
        .await
        .unwrap();

        assert_eq!(count_rows(&rt, "sessions").await, 2);
        assert_eq!(count_rows(&rt, "session_messages").await, 2);

        // Verify sources are distinct.
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT source FROM sessions ORDER BY source".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok");
        let sources: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("source") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sources, vec!["claude_code", "codex"]);
    }
}
