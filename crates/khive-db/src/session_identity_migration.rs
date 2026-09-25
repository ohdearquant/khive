//! ADR-117a's once-only rebuild of the pack-owned session mirror tables.
//!
//! The core migration runner calls this under its `BEGIN IMMEDIATE` transaction,
//! before any pack schema plan or mirror worker can run. A fresh database gets
//! the final mirror schema in this same versioned transaction.

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};

use crate::error::SqliteError;

const AUDIT_NAME: &str = "adr117a_source_scoped_identity";
const STAGE_SQL: &str = include_str!("../sql/039a-session-source-scope-stage.sql");
const SWAP_SQL: &str = include_str!("../sql/039b-session-source-scope-swap.sql");
const FINALIZE_SQL: &str = include_str!("../sql/039c-session-source-scope-finalize.sql");

fn table_exists(conn: &Connection, name: &str) -> Result<bool, SqliteError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |row| row.get(0),
    )?)
}

/// `(not_null, primary_key_position)` for one column, if it exists.
fn column_shape(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<Option<(bool, i64)>, SqliteError> {
    Ok(conn
        .query_row(
            "SELECT \"notnull\", pk FROM pragma_table_info(?1) WHERE name=?2",
            params![table, column],
            |row| Ok((row.get::<_, i64>(0)? != 0, row.get(1)?)),
        )
        .optional()?)
}

fn has_unique_key(conn: &Connection, table: &str, expected: &[&str]) -> Result<bool, SqliteError> {
    let mut indexes = conn.prepare("SELECT name FROM pragma_index_list(?1) WHERE \"unique\"=1")?;
    let names = indexes
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for name in names {
        let mut columns = conn.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
        let columns = columns
            .query_map([name], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if columns
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_source_scoped_keys(conn: &Connection) -> Result<(), SqliteError> {
    if !has_unique_key(
        conn,
        "sessions",
        &["namespace", "source", "provider_session_id"],
    )? || !has_unique_key(
        conn,
        "session_messages",
        &["namespace", "source", "session_id", "id"],
    )? {
        return Err(SqliteError::InvalidData(
            "session mirror source-scoped schema is missing its composite unique keys".into(),
        ));
    }
    Ok(())
}

fn row_count(conn: &Connection, table: &str) -> Result<i64, SqliteError> {
    // The two possible names are internal constants, never caller input.
    let sql = match table {
        "sessions" | "session_messages" | "sessions_scope_new" | "session_messages_scope_new" => {
            format!("SELECT COUNT(*) FROM {table}")
        }
        _ => return Err(SqliteError::InvalidData("invalid mirror table name".into())),
    };
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

/// Matches the ingest path's framed hash exactly: an optional-text marker,
/// byte length and bytes, then raw-line byte length and bytes.
fn content_hash(text: Option<&str>, raw: &str) -> String {
    let mut hash = Sha256::new();
    match text {
        Some(value) => {
            hash.update([1]);
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        None => hash.update([0]),
    }
    hash.update((raw.len() as u64).to_be_bytes());
    hash.update(raw.as_bytes());
    let digest = hash.finalize();
    format!("{digest:x}")
}

/// Called only from the global versioned migration runner's transaction.
pub(crate) fn apply(tx: &Transaction<'_>) -> Result<(), SqliteError> {
    let sessions_exist = table_exists(tx, "sessions")?;
    let messages_exist = table_exists(tx, "session_messages")?;
    if !sessions_exist && !messages_exist {
        tx.execute_batch(FINALIZE_SQL)?;
        validate_source_scoped_keys(tx)?;
        return Ok(());
    }
    if sessions_exist != messages_exist {
        return Err(SqliteError::InvalidData(
            "session mirror schema has only one of sessions/session_messages; refusing lossy migration"
                .into(),
        ));
    }

    let session_id = column_shape(tx, "sessions", "id")?;
    let message_id = column_shape(tx, "session_messages", "id")?;
    let session_namespace = column_shape(tx, "sessions", "namespace")?;
    let message_namespace = column_shape(tx, "session_messages", "namespace")?;
    let mirror_rowid = column_shape(tx, "session_messages", "mirror_rowid")?;
    let provider_session_id = column_shape(tx, "sessions", "provider_session_id")?;
    let message_session_id = column_shape(tx, "session_messages", "session_id")?;
    let source = column_shape(tx, "session_messages", "source")?;
    let hash = column_shape(tx, "session_messages", "content_hash")?;

    // A pre-created final schema needs no copy. This can occur in a direct
    // embedded caller that applied the pack's current DDL before upgrading the
    // core ledger; validate its key columns instead of rebuilding it blindly.
    if session_id == Some((true, 0))
        && message_id == Some((true, 0))
        && session_namespace == Some((true, 0))
        && message_namespace == Some((true, 0))
        && mirror_rowid == Some((false, 1))
        && provider_session_id == Some((true, 0))
        && message_session_id == Some((true, 0))
        && source == Some((true, 0))
        && hash == Some((true, 0))
    {
        tx.execute_batch(FINALIZE_SQL)?;
        validate_source_scoped_keys(tx)?;
        return Ok(());
    }

    // Exact legacy shape is the only supported rebuild input. A partially
    // changed schema must fail before DROP, rather than guessing how to copy.
    if session_id != Some((false, 1))
        || message_id != Some((false, 1))
        || session_namespace != Some((false, 0))
        || message_namespace != Some((false, 0))
        || mirror_rowid.is_some()
        || source.is_some()
        || hash.is_some()
    {
        return Err(SqliteError::InvalidData(
            "session mirror schema is neither the legacy bare-id shape nor the source-scoped shape"
                .into(),
        ));
    }

    let old_sessions = row_count(tx, "sessions")?;
    let old_messages = row_count(tx, "session_messages")?;
    let orphans: i64 = tx.query_row(
        "SELECT COUNT(*) FROM session_messages m
         LEFT JOIN sessions s ON s.id=m.session_id
          AND COALESCE(s.namespace,'local')=COALESCE(m.namespace,'local')
         WHERE s.id IS NULL",
        [],
        |row| row.get(0),
    )?;

    tx.execute_batch(STAGE_SQL)?;
    tx.execute_batch(
        "INSERT INTO sessions_scope_new
           (rowid, id, provider_session_id, source, cwd, git_branch, slug,
            message_count, first_seen_at, last_seen_at, namespace)
           SELECT rowid, id, provider_session_id, source, cwd, git_branch, slug,
                  message_count, first_seen_at, last_seen_at, COALESCE(namespace,'local')
             FROM sessions;
         INSERT INTO session_messages_scope_new
           (mirror_rowid, id, session_id, seq, parent_uuid, is_sidechain, role,
            msg_type, text, raw, created_at, namespace, source, content_hash)
           SELECT m.rowid, m.id, m.session_id, m.seq, m.parent_uuid,
                  m.is_sidechain, m.role, m.msg_type, m.text, m.raw,
                  m.created_at, COALESCE(m.namespace,'local'),
                  COALESCE(s.source,'unknown'), ''
             FROM session_messages m
             LEFT JOIN sessions s ON s.id=m.session_id
              AND COALESCE(s.namespace,'local')=COALESCE(m.namespace,'local');",
    )?;

    // Hash every legacy event while its original rowid is stable. The copy,
    // backfill, audit, swaps and version ledger all commit or roll back as one.
    {
        let mut select = tx.prepare("SELECT rowid, text, raw FROM session_messages")?;
        let mut rows = select.query([])?;
        while let Some(row) = rows.next()? {
            let rowid: i64 = row.get(0)?;
            let text: Option<String> = row.get(1)?;
            let raw: String = row.get(2)?;
            tx.execute(
                "UPDATE session_messages_scope_new SET content_hash=?1 WHERE mirror_rowid=?2",
                params![content_hash(text.as_deref(), &raw), rowid],
            )?;
        }
    }

    if row_count(tx, "sessions_scope_new")? != old_sessions
        || row_count(tx, "session_messages_scope_new")? != old_messages
    {
        return Err(SqliteError::InvalidData(
            "session mirror migration row-count mismatch; refusing table swap".into(),
        ));
    }

    tx.execute_batch(SWAP_SQL)?;
    tx.execute_batch(FINALIZE_SQL)?;
    validate_source_scoped_keys(tx)?;

    // A prior bare-id collision suppressed the losing source's rows before
    // storage, so the table copy cannot recover them. Rewind persisted file
    // cursors; the next ordinary mirror tick will reparse each file under the
    // new scoped key. This covers both line tails and whole-file exports and
    // does not perform filesystem IO during the migration transaction.
    let cursors_reset = if table_exists(tx, "session_mirror_cursor")? {
        tx.execute("UPDATE session_mirror_cursor SET byte_offset=0", [])? as i64
    } else {
        0
    };
    tx.execute(
        "INSERT INTO session_mirror_migration_audit
           (name, session_rows, message_rows, orphan_rows, cursors_reset, migrated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            AUDIT_NAME,
            old_sessions,
            old_messages,
            orphans,
            cursors_reset,
            chrono::Utc::now().timestamp_micros(),
        ],
    )?;
    tracing::info!(
        session_rows = old_sessions,
        message_rows = old_messages,
        orphan_rows = orphans,
        cursors_reset,
        "session mirror identity migration retained legacy rows"
    );
    Ok(())
}
