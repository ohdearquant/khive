//! Idempotent file tail + upsert into the session mirror tables.
//!
//! `mirror_file` reads new bytes from a JSONL file starting at `start_offset`
//! via a buffered, line-at-a-time reader bounded by [`MirrorLimits`], parses
//! complete lines using the parser selected by [`LineTailSource`] (mapped
//! internally to [`MirrorSource`]), and writes the resulting bounded chunk to
//! the session mirror tables in a single transaction.  A single call
//! processes at most one bounded chunk — never the whole file at once — so
//! the caller's polling loop advances the persisted cursor incrementally
//! across multiple calls for large deltas.  It is safe to call repeatedly on
//! the same file; `INSERT OR IGNORE` keyed by the event UUID ensures
//! idempotency.

use std::io::{BufRead, Seek, SeekFrom};
use std::path::Path;

use chrono::Utc;
use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlStatement, SqlTxOptions, SqlValue};
use khive_storage::SqlTransaction;

use super::parse;

/// The full ADR-080 mirror-source contract — the closed set of sources
/// `sessions.source` can hold (`docs/adr/ADR-080-session-pack-oss-storage-mechanism.md`,
/// "Mirror sources — closed set"). Adding a source requires amending that ADR
/// section and this enum together.
///
/// This is a superset of [`LineTailSource`]: `ChatGptExport` ingests via
/// whole-file re-parse (`mirror_chatgpt_export_file`), not the per-line
/// dispatch `LineTailSource` selects, so it has no `LineTailSource` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorSource {
    /// Claude Code (`~/.claude/projects/<slug>/<uuid>.jsonl`).
    ClaudeCode,
    /// Codex CLI (`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`).
    Codex,
    /// ChatGPT data export (`<exports dir>/**/conversations.json`).
    ChatGptExport,
}

impl MirrorSource {
    /// The string written to `sessions.source`.
    pub fn as_str(self) -> &'static str {
        match self {
            MirrorSource::ClaudeCode => "claude_code",
            MirrorSource::Codex => "codex",
            MirrorSource::ChatGptExport => "chatgpt_export",
        }
    }
}

impl From<LineTailSource> for MirrorSource {
    fn from(source: LineTailSource) -> Self {
        match source {
            LineTailSource::ClaudeCode => MirrorSource::ClaudeCode,
            LineTailSource::Codex => MirrorSource::Codex,
        }
    }
}

/// Identifies which CLI produced the JSONL file being mirrored, for the
/// purpose of selecting `mirror_file`'s per-line parser.
///
/// This is narrower than [`MirrorSource`]: it covers only the line-tail
/// sources (append-only JSONL, tailed by byte offset). ChatGPT export
/// ingestion is whole-file re-parse, not line-tail, so it has no variant
/// here — see [`mirror_chatgpt_export_file`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTailSource {
    /// Claude Code (`~/.claude/projects/<slug>/<uuid>.jsonl`).
    ClaudeCode,
    /// Codex CLI (`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`).
    Codex,
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

/// Ceiling on bytes read per `mirror_file` call in production. Bounds worst-case
/// memory use when a file has accumulated a very large delta (e.g. after daemon
/// downtime or a multi-GB transcript).
const MIRROR_MAX_BYTES_PER_PASS: usize = 8 * 1024 * 1024;

/// Ceiling on parsed events collected per `mirror_file` call in production.
const MIRROR_MAX_EVENTS_PER_PASS: usize = 1024;

/// Per-call caps on how much of a file's delta `mirror_file` will read and
/// parse before writing a bounded chunk. Production always uses
/// [`MirrorLimits::production`]; tests use a much smaller cap to force
/// multi-pass behavior without giant fixtures.
#[derive(Clone, Copy, Debug)]
struct MirrorLimits {
    max_bytes_per_pass: usize,
    max_events_per_pass: usize,
}

impl MirrorLimits {
    fn production() -> Self {
        Self {
            max_bytes_per_pass: MIRROR_MAX_BYTES_PER_PASS,
            max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
        }
    }
}

/// Read new bytes of `path` starting at `start_offset`, parse complete lines
/// using the parser selected by `source`, and upsert them idempotently into the
/// session mirror tables.
///
/// For `LineTailSource::Codex`, `codex_session_id` must be the session UUID
/// derived from the filename; it is used both to key the `sessions` row and to
/// synthesise per-line event UUIDs (`"{session_id}:{abs_byte_offset}"`).
/// For `LineTailSource::ClaudeCode`, `codex_session_id` is ignored (the session
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
    source: LineTailSource,
    codex_session_id: Option<&str>,
) -> Result<MirrorStats, RuntimeError> {
    mirror_file_with_limits(
        runtime,
        path,
        start_offset,
        source,
        codex_session_id,
        MirrorLimits::production(),
    )
    .await
}

/// A single bounded read pass: at most `limits.max_bytes_per_pass` bytes and
/// `limits.max_events_per_pass` parsed events, always stopping on a complete
/// line boundary.
struct MirrorChunk {
    events: Vec<parse::ParsedEvent>,
    scanned: u64,
    new_offset: u64,
}

/// Read at most one bounded chunk of `path` starting at `start_offset`, one
/// complete line at a time via a buffered reader — never allocating more than
/// the file's current line. A partial trailing line (no terminating `\n`) is
/// left for the next call.
///
/// The first complete line of a pass is always consumed even if it alone
/// exceeds `limits.max_bytes_per_pass`, so an unusually long single line
/// cannot livelock the pass.
fn read_bounded_chunk(
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
    limits: MirrorLimits,
) -> std::io::Result<MirrorChunk> {
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    if start_offset >= file_len {
        return Ok(MirrorChunk {
            events: Vec::new(),
            scanned: 0,
            new_offset: start_offset,
        });
    }

    file.seek(SeekFrom::Start(start_offset))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut events = Vec::new();
    let mut scanned: u64 = 0;
    let mut new_offset = start_offset;
    let mut bytes_this_pass: usize = 0;

    loop {
        if scanned > 0
            && (bytes_this_pass >= limits.max_bytes_per_pass
                || events.len() >= limits.max_events_per_pass)
        {
            break;
        }

        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break; // EOF
        }
        if !line.ends_with(b"\n") {
            break; // partial trailing line: leave for the next pass
        }

        new_offset += read as u64;
        bytes_this_pass += read;
        let line_start = new_offset - read as u64;

        let raw = String::from_utf8_lossy(&line[..line.len() - 1]);
        if raw.is_empty() {
            continue; // blank line: bytes consumed, but not counted as scanned
        }

        match source {
            LineTailSource::ClaudeCode => {
                if let Some(ev) = parse::parse_cc_line(&raw) {
                    events.push(ev);
                }
            }
            LineTailSource::Codex => {
                let sid = codex_session_id.unwrap_or("");
                if let Some(ev) = parse::parse_codex_line(&raw, sid, line_start) {
                    events.push(ev);
                }
            }
        }
        scanned += 1;
    }

    Ok(MirrorChunk {
        events,
        scanned,
        new_offset,
    })
}

/// Read, parse, and write one bounded chunk starting at `start_offset`.
async fn mirror_file_with_limits(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
    limits: MirrorLimits,
) -> Result<MirrorStats, RuntimeError> {
    let chunk =
        read_bounded_chunk(path, start_offset, source, codex_session_id, limits).map_err(|e| {
            RuntimeError::Internal(format!(
                "mirror_file: failed to read {:?} at offset {start_offset}: {e}",
                path
            ))
        })?;

    if chunk.scanned == 0 {
        return Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: chunk.new_offset,
        });
    }

    if chunk.events.is_empty() {
        // Apply cursor update even when there are no parseable events so we
        // don't re-read the same bytes on the next call. A failure here must
        // propagate — silently swallowing it would let the cursor and the
        // already-consumed bytes drift apart.
        write_cursor_only(runtime, path, &None, chunk.new_offset).await?;
        return Ok(MirrorStats {
            inserted: 0,
            scanned: chunk.scanned,
            new_offset: chunk.new_offset,
        });
    }

    write_events_and_cursor(
        runtime,
        path,
        MirrorSource::from(source).as_str(),
        &chunk.events,
        chunk.scanned,
        chunk.new_offset,
    )
    .await
}

/// Read the whole ChatGPT export `conversations.json` at `path`, parse every
/// conversation's mapping tree via [`parse::parse_chatgpt_export`], and upsert
/// every message-bearing event idempotently into the session mirror tables in
/// a single transaction.
///
/// Unlike `mirror_file` (append-only line-tail), a ChatGPT export is a single
/// static JSON array with no stable "new bytes" boundary to tail, so this
/// function always re-reads and re-parses the whole file. `start_offset` is
/// used only as a cheap re-poll guard: if the file has not grown past it,
/// nothing is read or parsed. `new_offset` is set to the whole file's byte
/// length only after a successful parse and commit — any IO, parse, or DB
/// error leaves the persisted cursor untouched, so a partially-downloaded
/// export is retried whole on the next tick, never half-consumed.
pub async fn mirror_chatgpt_export_file(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
) -> Result<MirrorStats, RuntimeError> {
    let file_len = std::fs::metadata(path).map(|m| m.len()).map_err(|e| {
        RuntimeError::Internal(format!(
            "mirror_chatgpt_export_file: failed to stat {path:?}: {e}"
        ))
    })?;

    if file_len <= start_offset {
        return Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: start_offset,
        });
    }

    let content = std::fs::read_to_string(path).map_err(|e| {
        RuntimeError::Internal(format!(
            "mirror_chatgpt_export_file: failed to read {path:?}: {e}"
        ))
    })?;

    let events = parse::parse_chatgpt_export(&content).ok_or_else(|| {
        RuntimeError::Internal(format!(
            "mirror_chatgpt_export_file: {path:?} is not a valid ChatGPT export (expected a top-level JSON array)"
        ))
    })?;

    let scanned = events.len() as u64;

    write_events_and_cursor(
        runtime,
        path,
        MirrorSource::ChatGptExport.as_str(),
        &events,
        scanned,
        file_len,
    )
    .await
}

/// Upsert `events` and the mirror cursor for `path` in one transaction.
///
/// Shared by `mirror_file`'s eventful line-tail path and
/// `mirror_chatgpt_export_file`'s whole-file path, so the session/message row
/// construction and cursor semantics (create-only sessions, `INSERT OR
/// IGNORE` message dedup, monotonic `last_seen_at`, cursor advances only on
/// success) live in exactly one place.
async fn write_events_and_cursor(
    runtime: &KhiveRuntime,
    path: &Path,
    source_value: &'static str,
    events: &[parse::ParsedEvent],
    scanned: u64,
    new_offset: u64,
) -> Result<MirrorStats, RuntimeError> {
    let now_us = Utc::now().timestamp_micros();
    let sql = runtime.sql();

    let mut tx = sql
        .begin_tx(SqlTxOptions::default())
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror: begin_tx: {e}")))?;

    let mut inserted: u64 = 0;
    let mut last_session_id: Option<String> = None;

    for ev in events {
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
                source_value
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
        .map_err(|e| RuntimeError::Internal(format!("mirror: session create: {e}")))?;

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
            .map_err(|e| RuntimeError::Internal(format!("mirror: message insert: {e}")))?;

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
            .map_err(|e| RuntimeError::Internal(format!("mirror: session touch: {e}")))?;
        }

        inserted += affected;
        last_session_id = Some(ev.session_id.clone());
    }

    // ── refresh message_count for each distinct session ───────────────────────
    //
    // In practice one file maps to one session_id, but we refresh every
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
            .map_err(|e| RuntimeError::Internal(format!("mirror: count refresh: {e}")))?;
        }
    }

    upsert_cursor_in_tx(
        &mut *tx,
        path,
        last_session_id.as_deref(),
        new_offset,
        now_us,
    )
    .await?;

    // ── commit ────────────────────────────────────────────────────────────────
    tx.commit()
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror: commit: {e}")))?;

    Ok(MirrorStats {
        inserted,
        scanned,
        new_offset,
    })
}

/// Upsert the `session_mirror_cursor` row for `path` inside an open transaction.
async fn upsert_cursor_in_tx(
    tx: &mut dyn SqlTransaction,
    path: &Path,
    session_id: Option<&str>,
    new_offset: u64,
    now_us: i64,
) -> Result<(), RuntimeError> {
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
            session_id
                .map(|s| SqlValue::Text(s.to_string()))
                .unwrap_or(SqlValue::Null),
            SqlValue::Integer(new_offset as i64),
            SqlValue::Integer(now_us),
        ],
        label: Some("session_mirror_cursor_upsert".into()),
    })
    .await
    .map_err(|e| RuntimeError::Internal(format!("mirror: cursor upsert: {e}")))?;
    Ok(())
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
        let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
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
        let stats2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .expect("mirror_file second call");
        assert_eq!(stats2.inserted, 0, "second pass must insert 0 rows");
        assert_eq!(count_rows(&rt, "session_messages").await, 3);

        // Offset-aware: calling from the advanced offset finds nothing new.
        let stats3 = mirror_file(
            &rt,
            &path,
            stats.new_offset,
            LineTailSource::ClaudeCode,
            None,
        )
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
    async fn mirror_file_respects_low_test_cap_and_advances_over_multiple_passes() {
        // Regression for PACKSESSION-AUD-003: `mirror_file` used to allocate
        // and read the entire file delta in one shot via `read_from_offset`
        // (`Vec::with_capacity(file_len - offset)` + `read_to_end`), which
        // could OOM or stall the daemon on a very large accumulated delta.
        // With a tiny test-only byte cap, a multi-line file must now be
        // consumed across multiple bounded passes — each committing its own
        // chunk and cursor advance — never reading the whole file at once.
        let (rt, _dir) = setup().await;

        let lines: Vec<String> = (0..6)
            .map(|i| user_line(&format!("uuid-cap-{i}"), "sess-CAP", &format!("line{i}")))
            .collect();

        let mut file = NamedTempFile::new().expect("tmpfile");
        for line in &lines {
            writeln!(file, "{line}").unwrap();
        }
        let path = file.path().to_path_buf();
        let file_len = std::fs::metadata(&path).unwrap().len();

        // All 6 fixture lines are byte-identical in length, so capping at
        // exactly two lines' worth of bytes forces a 2-line-per-pass split
        // without needing a giant fixture.
        let cap_bytes = (lines[0].len() + 1) + (lines[1].len() + 1);
        let limits = MirrorLimits {
            max_bytes_per_pass: cap_bytes,
            max_events_per_pass: 1024,
        };

        let stats1 =
            mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("first bounded pass");
        assert_eq!(
            stats1.inserted, 2,
            "first pass must stop at the byte cap, not read the whole file"
        );
        assert_eq!(stats1.scanned, 2);
        assert!(
            stats1.new_offset < file_len,
            "new_offset {new} must be less than file_len {file_len} for a bounded pass",
            new = stats1.new_offset
        );
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(stats1.new_offset as i64),
            "cursor must be committed after the first bounded pass"
        );

        let stats2 = mirror_file_with_limits(
            &rt,
            &path,
            stats1.new_offset,
            LineTailSource::ClaudeCode,
            None,
            limits,
        )
        .await
        .expect("second bounded pass");
        assert_eq!(stats2.inserted, 2);
        assert!(stats2.new_offset > stats1.new_offset);
        assert!(stats2.new_offset < file_len);

        let stats3 = mirror_file_with_limits(
            &rt,
            &path,
            stats2.new_offset,
            LineTailSource::ClaudeCode,
            None,
            limits,
        )
        .await
        .expect("third bounded pass");
        assert_eq!(stats3.inserted, 2);
        assert_eq!(stats3.new_offset, file_len, "final pass must reach EOF");

        // All 6 rows landed across 3 bounded passes, and the cursor reflects
        // the full file — no pass allocated or inserted the entire file at
        // once.
        assert_eq!(count_rows(&rt, "session_messages").await, 6);
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(file_len as i64)
        );

        // A pass with no remaining bytes is a clean no-op.
        let stats4 = mirror_file_with_limits(
            &rt,
            &path,
            stats3.new_offset,
            LineTailSource::ClaudeCode,
            None,
            limits,
        )
        .await
        .expect("fourth pass at EOF");
        assert_eq!(stats4.inserted, 0);
        assert_eq!(stats4.scanned, 0);
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

        let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
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
        let stats2 = mirror_file(
            &rt,
            &path,
            stats.new_offset,
            LineTailSource::ClaudeCode,
            None,
        )
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
        let s1 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s1.inserted, 1);

        // Append same uuid again.
        writeln!(file, "{line}").unwrap();

        // Second call from offset 0 should see both lines but insert 0 new rows.
        let s2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s2.inserted, 0, "duplicate uuid must not be re-inserted");
        assert_eq!(count_rows(&rt, "session_messages").await, 1);

        // Incremental: call from first call's new_offset; the second line is the dup.
        let s3 = mirror_file(&rt, &path, s1.new_offset, LineTailSource::ClaudeCode, None)
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

        let s1 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s1.inserted, 1);
        let seen_after_first = last_seen_at(&rt, "sess-NTS")
            .await
            .expect("session row exists");

        // Replay from offset 0: re-scans the same line, inserts 0, and must
        // leave last_seen_at byte-identical even though now_us has advanced.
        let s2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
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

        let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
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
        let result = mirror_file(&rt, &bad_path, 0, LineTailSource::ClaudeCode, None).await;
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
        let stats = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
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
        let s1 = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
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
        let s2 = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
            .await
            .unwrap();
        assert_eq!(s2.inserted, 0, "second pass must be idempotent");
        assert_eq!(count_rows(&rt, "session_messages").await, 1);

        // Incremental pass from advanced offset: no new data.
        let s3 = mirror_file(
            &rt,
            &path,
            s1.new_offset,
            LineTailSource::Codex,
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

        mirror_file(&rt, cc_file.path(), 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();

        mirror_file(
            &rt,
            cdx_file.path(),
            0,
            LineTailSource::Codex,
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

    // ── ChatGPT export whole-file ingest tests ────────────────────────────────
    //
    // All fixtures below are hand-authored synthetic JSON, not real export
    // content. Node ids are set equal to their own `message.id` so that
    // `parent_uuid` (which threads through the mapping node id, per
    // `parse::build_chatgpt_event`) resolves to the expected message uuid.

    use serde_json::json;

    fn write_export_file(content: &str) -> (NamedTempFile, std::path::PathBuf) {
        let mut file = NamedTempFile::new().expect("tmpfile");
        write!(file, "{content}").unwrap();
        let path = file.path().to_path_buf();
        (file, path)
    }

    fn chatgpt_happy_export_json() -> String {
        let conv = json!({
            "id": "conv-happy",
            "title": "Synthetic Happy",
            "create_time": 1_751_462_400.0,
            "current_node": "msg-happy-assistant",
            "mapping": {
                "root-happy": {
                    "id": "root-happy",
                    "message": null,
                    "parent": null,
                    "children": ["msg-happy-user"]
                },
                "msg-happy-user": {
                    "id": "msg-happy-user",
                    "parent": "root-happy",
                    "children": ["msg-happy-assistant"],
                    "message": {
                        "id": "msg-happy-user",
                        "author": {"role": "user"},
                        "create_time": 1_751_462_400.0,
                        "content": {"content_type": "text", "parts": ["Hello synthetic"]}
                    }
                },
                "msg-happy-assistant": {
                    "id": "msg-happy-assistant",
                    "parent": "msg-happy-user",
                    "children": [],
                    "message": {
                        "id": "msg-happy-assistant",
                        "author": {"role": "assistant"},
                        "create_time": 1_751_462_401.0,
                        "content": {"content_type": "text", "parts": ["Hi synthetic"]}
                    }
                }
            }
        });
        serde_json::to_string(&json!([conv])).unwrap()
    }

    #[tokio::test]
    async fn test_chatgpt_happy_conversations_json() {
        let (rt, _dir) = setup().await;
        let (_file, path) = write_export_file(&chatgpt_happy_export_json());
        let file_len = std::fs::metadata(&path).unwrap().len();

        let stats = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("happy path ingest");
        assert_eq!(stats.inserted, 2, "2 message-bearing nodes");
        assert_eq!(stats.scanned, 2, "2 events parsed");
        assert_eq!(stats.new_offset, file_len, "whole-file cursor-at-length");

        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT source, slug, cwd, git_branch FROM sessions WHERE id='conv-happy'"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("session row must exist");
        match row.get("source") {
            Some(SqlValue::Text(s)) => assert_eq!(s, "chatgpt_export"),
            other => panic!("unexpected source: {other:?}"),
        }
        match row.get("slug") {
            Some(SqlValue::Text(s)) => assert_eq!(s, "Synthetic Happy"),
            other => panic!("unexpected slug: {other:?}"),
        }
        assert!(
            matches!(row.get("cwd"), Some(SqlValue::Null) | None),
            "chatgpt export never carries a cwd"
        );
        assert!(
            matches!(row.get("git_branch"), Some(SqlValue::Null) | None),
            "chatgpt export never carries a git branch"
        );

        let mut r2 = sql.reader().await.expect("reader");
        let rows = r2
            .query_all(SqlStatement {
                sql: "SELECT seq, role FROM session_messages \
                      WHERE session_id='conv-happy' ORDER BY seq"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok");
        let roles: Vec<(i64, String)> = rows
            .iter()
            .map(|row| {
                let seq = match row.get("seq") {
                    Some(SqlValue::Integer(n)) => *n,
                    other => panic!("unexpected seq: {other:?}"),
                };
                let role = match row.get("role") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected role: {other:?}"),
                };
                (seq, role)
            })
            .collect();
        assert_eq!(
            roles,
            vec![(0, "user".to_string()), (1, "assistant".to_string())]
        );
    }

    fn chatgpt_idempotency_export_json() -> String {
        let conv = json!({
            "id": "conv-idem",
            "title": "Synthetic Idempotency",
            "current_node": "msg-idem-assistant",
            "mapping": {
                "root-idem": {
                    "id": "root-idem",
                    "message": null,
                    "parent": null,
                    "children": ["msg-idem-user"]
                },
                "msg-idem-user": {
                    "id": "msg-idem-user",
                    "parent": "root-idem",
                    "children": ["msg-idem-assistant"],
                    "message": {
                        "id": "msg-idem-user",
                        "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": ["Same question again"]}
                    }
                },
                "msg-idem-assistant": {
                    "id": "msg-idem-assistant",
                    "parent": "msg-idem-user",
                    "children": [],
                    "message": {
                        "id": "msg-idem-assistant",
                        "author": {"role": "assistant"},
                        "content": {"content_type": "text", "parts": ["Same answer again"]}
                    }
                }
            }
        });
        serde_json::to_string(&json!([conv])).unwrap()
    }

    #[tokio::test]
    async fn test_chatgpt_reingest_idempotency_conversations_json() {
        let (rt, _dir) = setup().await;
        let (_file, path) = write_export_file(&chatgpt_idempotency_export_json());

        let s1 = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("first ingest");
        assert_eq!(s1.inserted, 2);

        let seen_after_first = last_seen_at(&rt, "conv-idem")
            .await
            .expect("session row exists");

        // Re-ingest from offset 0 (the service always re-reads the whole file
        // for this source): same event uuids, INSERT OR IGNORE must dedup.
        let s2 = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("second ingest");
        assert_eq!(s2.inserted, 0, "re-ingest must insert 0 new rows");

        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let count = r
            .query_row(SqlStatement {
                sql: "SELECT COUNT(*) FROM session_messages WHERE session_id='conv-idem'".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("count row");
        match count.columns.first().map(|c| &c.value) {
            Some(SqlValue::Integer(n)) => assert_eq!(*n, 2, "message count stays at 2"),
            other => panic!("unexpected count: {other:?}"),
        }

        let seen_after_replay = last_seen_at(&rt, "conv-idem")
            .await
            .expect("session row still exists");
        assert_eq!(
            seen_after_first, seen_after_replay,
            "pure replay must not advance last_seen_at"
        );
    }

    fn chatgpt_branch_sidechain_export_json() -> String {
        let conv = json!({
            "id": "conv-branch",
            "title": "Synthetic Branch",
            "current_node": "msg-branch-main",
            "mapping": {
                "root-branch": {
                    "id": "root-branch",
                    "message": null,
                    "parent": null,
                    "children": ["msg-branch-user"]
                },
                "msg-branch-user": {
                    "id": "msg-branch-user",
                    "parent": "root-branch",
                    "children": ["msg-branch-main", "msg-branch-alt"],
                    "message": {
                        "id": "msg-branch-user",
                        "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": ["Branch question"]}
                    }
                },
                "msg-branch-main": {
                    "id": "msg-branch-main",
                    "parent": "msg-branch-user",
                    "children": [],
                    "message": {
                        "id": "msg-branch-main",
                        "author": {"role": "assistant"},
                        "content": {"content_type": "text", "parts": ["Main answer"]}
                    }
                },
                "msg-branch-alt": {
                    "id": "msg-branch-alt",
                    "parent": "msg-branch-user",
                    "children": [],
                    "message": {
                        "id": "msg-branch-alt",
                        "author": {"role": "assistant"},
                        "content": {"content_type": "text", "parts": ["Alternate answer"]}
                    }
                }
            }
        });
        serde_json::to_string(&json!([conv])).unwrap()
    }

    #[tokio::test]
    async fn test_chatgpt_branch_sidechain_conversations_json() {
        let (rt, _dir) = setup().await;
        let (_file, path) = write_export_file(&chatgpt_branch_sidechain_export_json());

        let stats = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("branch ingest");
        assert_eq!(stats.inserted, 3, "user + main + alt all stored");

        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT id, is_sidechain, text FROM session_messages \
                      WHERE session_id='conv-branch' ORDER BY id"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok");
        assert_eq!(rows.len(), 3);

        for row in &rows {
            let id = match row.get("id") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected id: {other:?}"),
            };
            let is_sidechain = match row.get("is_sidechain") {
                Some(SqlValue::Integer(n)) => *n,
                other => panic!("unexpected is_sidechain: {other:?}"),
            };
            let text = match row.get("text") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected text: {other:?}"),
            };
            match id.as_str() {
                "msg-branch-user" | "msg-branch-main" => {
                    assert_eq!(is_sidechain, 0, "{id} is on the current-node path")
                }
                "msg-branch-alt" => {
                    assert_eq!(is_sidechain, 1, "alt branch is off the current-node path");
                    assert_eq!(
                        text, "Alternate answer",
                        "sidechain content must be preserved, not dropped"
                    );
                }
                other => panic!("unexpected message id: {other}"),
            }
        }
    }

    #[tokio::test]
    async fn test_chatgpt_malformed_conversations_json_cursor_does_not_advance() {
        let (rt, _dir) = setup().await;

        // Seed the path with a valid (if empty) export and record its cursor.
        let (mut file, path) = write_export_file("[]");
        let seeded_stats = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("seeding with an empty array is a valid parse");
        assert_eq!(seeded_stats.inserted, 0);
        let seeded_offset = seeded_stats.new_offset;

        let seeded_sessions = count_rows(&rt, "sessions").await;
        let seeded_messages = count_rows(&rt, "session_messages").await;

        // Overwrite with a longer, malformed (valid-JSON-but-not-an-array) body.
        let malformed = r#"{"oops": "not a chatgpt export array"}"#;
        file.as_file_mut().set_len(0).expect("truncate");
        std::io::Seek::seek(file.as_file_mut(), std::io::SeekFrom::Start(0)).unwrap();
        write!(file, "{malformed}").unwrap();

        let result = mirror_chatgpt_export_file(&rt, &path, seeded_offset).await;
        assert!(
            matches!(result, Err(RuntimeError::Internal(_))),
            "malformed export must return Internal error, got {result:?}"
        );

        let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
        assert_eq!(
            stored_offset,
            Some(seeded_offset as i64),
            "cursor must remain at the pre-error value"
        );
        assert_eq!(
            count_rows(&rt, "sessions").await,
            seeded_sessions,
            "no new session rows on parse failure"
        );
        assert_eq!(
            count_rows(&rt, "session_messages").await,
            seeded_messages,
            "no new message rows on parse failure"
        );
    }

    #[tokio::test]
    async fn test_chatgpt_secret_bearing_conversations_json_is_masked() {
        // Assembled from fragments at runtime so no credential-shaped literal
        // is committed to the repo; matches the AWS-key shape already covered
        // by `khive_runtime::secret_gate`'s own detector tests.
        let secret_fragment_a = "AKIA";
        let secret_fragment_b = "FAKEKEY1234567890";
        let secret = format!("{secret_fragment_a}{secret_fragment_b}");
        let user_text = format!("here is my key: {secret}");

        let conv = json!({
            "id": "conv-secret",
            "title": "Synthetic Secret",
            "current_node": "msg-secret-user",
            "mapping": {
                "root-secret": {
                    "id": "root-secret",
                    "message": null,
                    "parent": null,
                    "children": ["msg-secret-user"]
                },
                "msg-secret-user": {
                    "id": "msg-secret-user",
                    "parent": "root-secret",
                    "children": [],
                    "message": {
                        "id": "msg-secret-user",
                        "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": [user_text]}
                    }
                }
            }
        });
        let content = serde_json::to_string(&json!([conv])).unwrap();
        let (_file, path) = write_export_file(&content);

        let (rt, _dir) = setup().await;
        let stats = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("secret-bearing content must still ingest, only masked");
        assert_eq!(stats.inserted, 1);

        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT text, raw FROM session_messages WHERE session_id='conv-secret'".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("message row must exist");
        let (stored_text, stored_raw) = match (row.get("text"), row.get("raw")) {
            (Some(SqlValue::Text(t)), Some(SqlValue::Text(r))) => (t.clone(), r.clone()),
            other => panic!("unexpected text/raw shape: {other:?}"),
        };

        assert!(
            !stored_text.contains(&secret),
            "stored text must not contain the raw secret"
        );
        assert!(
            !stored_raw.contains(&secret),
            "stored raw must not contain the raw secret"
        );
        assert!(
            stored_text.contains("***MASKED***"),
            "stored text must carry the secret_gate redaction marker"
        );
        assert!(
            stored_raw.contains("***MASKED***"),
            "stored raw must carry the secret_gate redaction marker"
        );
    }

    /// SS6 invariants #4 ("an ingest error never advances the cursor") and #5
    /// ("one transaction per file pass") both rest on the same underlying
    /// contract that `write_events_and_cursor` depends on: a `begin_tx()`
    /// transaction that hits an error before `commit()` is called must leave
    /// no visible trace of ANY write it made in that pass — including the
    /// cursor upsert, which runs last, right before `commit()`.
    ///
    /// The real ingest loop can't be driven into a mid-loop DB error through
    /// crafted event data: the `sessions` insert uses `ON CONFLICT(id) DO
    /// NOTHING` and the `session_messages` insert uses `INSERT OR IGNORE`,
    /// both of which swallow constraint violations by design (that's what
    /// makes re-ingest idempotent). So this test exercises the same
    /// `begin_tx`/`execute`/drop-without-commit path directly — the exact
    /// machinery `mirror_chatgpt_export_file`'s `?`-propagated errors rely
    /// on — and forces a genuine, non-suppressed SQL error (a `prepare()`
    /// failure on a nonexistent table) after a session write AND a cursor
    /// advance have already succeeded within the same open transaction.
    #[tokio::test]
    async fn test_mid_transaction_db_error_leaves_no_partial_state_and_cursor_unadvanced() {
        let (rt, _dir) = setup().await;
        let sql = rt.sql();
        let path = std::path::Path::new("/synthetic/mid-tx-probe.json");

        let mut tx = sql
            .begin_tx(SqlTxOptions::default())
            .await
            .expect("begin_tx");

        // First write succeeds — mirrors event 1's session row in a
        // multi-event file pass.
        tx.execute(SqlStatement {
            sql: "INSERT INTO sessions \
                  (id, provider_session_id, source, message_count, first_seen_at, last_seen_at, namespace) \
                  VALUES('mid-tx-session', 'mid-tx-session', 'chatgpt_export', 0, 1, 1, 'local')"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("first write in transaction must succeed");

        // Cursor advance succeeds too — mirrors `upsert_cursor_in_tx` running
        // near the end of `write_events_and_cursor`, before `commit()`.
        upsert_cursor_in_tx(&mut *tx, path, Some("mid-tx-session"), 999, 1)
            .await
            .expect("cursor upsert in transaction must succeed");

        // Third write fails with a genuine (non-suppressed) SQL error —
        // mirrors a mid-loop DB failure on a later event in the same file.
        let err = tx
            .execute(SqlStatement {
                sql: "INSERT INTO no_such_table_mid_tx_probe(a) VALUES(1)".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(err.is_err(), "forced third write must fail");

        // No `commit()` is ever called — `tx` is dropped here, exactly as
        // `write_events_and_cursor` drops its `tx` when an earlier `?`
        // returns `Err` before reaching its own `tx.commit()` call.
        drop(tx);

        assert_eq!(
            count_rows(&rt, "sessions").await,
            0,
            "session write must not survive a later error in the same transaction"
        );
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            None,
            "cursor must not advance when a later write in the same transaction fails"
        );
    }
}
