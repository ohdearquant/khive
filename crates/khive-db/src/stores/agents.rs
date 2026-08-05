//! SQL-backed agent-process store (ADR-142 §1 "Process model").

use std::sync::Arc;

use async_trait::async_trait;

use khive_storage::error::StorageError;
use khive_storage::{AgentState, AgentStore, StorageCapability, TerminalReason};
use khive_types::AgentRecord;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;
use crate::writer_task::WriterTaskHandle;

fn state_as_sql(state: AgentState) -> &'static str {
    match state {
        AgentState::Spawned => "spawned",
        AgentState::Running => "running",
        AgentState::Suspended => "suspended",
        AgentState::Terminal => "terminal",
    }
}

fn state_from_sql(s: &str) -> Result<AgentState, rusqlite::Error> {
    match s {
        "spawned" => Ok(AgentState::Spawned),
        "running" => Ok(AgentState::Running),
        "suspended" => Ok(AgentState::Suspended),
        "terminal" => Ok(AgentState::Terminal),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown AgentState: {other}").into(),
        )),
    }
}

fn terminal_reason_as_sql(reason: TerminalReason) -> &'static str {
    match reason {
        TerminalReason::Completed => "completed",
        TerminalReason::Failed => "failed",
        TerminalReason::Killed => "killed",
        TerminalReason::Abandoned => "abandoned",
        TerminalReason::HostRestart => "host_restart",
    }
}

fn terminal_reason_from_sql(s: &str) -> Result<TerminalReason, rusqlite::Error> {
    match s {
        "completed" => Ok(TerminalReason::Completed),
        "failed" => Ok(TerminalReason::Failed),
        "killed" => Ok(TerminalReason::Killed),
        "abandoned" => Ok(TerminalReason::Abandoned),
        "host_restart" => Ok(TerminalReason::HostRestart),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown TerminalReason: {other}").into(),
        )),
    }
}

// =============================================================================
// SqlAgentStore
// =============================================================================

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    // ADR-142 defines no `StorageCapability::Agents` variant. `Sql` is the
    // closest existing generic-driver capability; `op` still disambiguates
    // which agent-store method failed.
    StorageError::driver(StorageCapability::Sql, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Sql, op, e)
}

const AGENT_COLUMNS: &str = "agent_id, state, terminal_reason, provider, provider_session_id, \
     checkpoint_session_id, checkpoint_cursor, owner_actor, owner_peer_class, \
     owner_write_namespace, owner_visible_namespaces, spawn_fingerprint, spawned_at, \
     state_changed_at, idempotency_key";

fn read_agent_record(row: &rusqlite::Row<'_>) -> Result<AgentRecord, rusqlite::Error> {
    let agent_id: String = row.get(0)?;
    let state_str: String = row.get(1)?;
    let terminal_reason_str: Option<String> = row.get(2)?;
    let provider: String = row.get(3)?;
    let provider_session_id: Option<String> = row.get(4)?;
    let checkpoint_session_id: Option<String> = row.get(5)?;
    let checkpoint_cursor: Option<i64> = row.get(6)?;
    let owner_actor: String = row.get(7)?;
    let owner_peer_class: String = row.get(8)?;
    let owner_write_namespace: String = row.get(9)?;
    let owner_visible_namespaces_json: String = row.get(10)?;
    let spawn_fingerprint: String = row.get(11)?;
    let spawned_at: i64 = row.get(12)?;
    let state_changed_at: i64 = row.get(13)?;
    let idempotency_key: Option<String> = row.get(14)?;

    let state = state_from_sql(&state_str)?;
    let terminal_reason = terminal_reason_str
        .as_deref()
        .map(terminal_reason_from_sql)
        .transpose()?;
    let owner_visible_namespaces: Vec<String> =
        serde_json::from_str(&owner_visible_namespaces_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(e))
        })?;

    Ok(AgentRecord {
        agent_id,
        state,
        terminal_reason,
        provider,
        provider_session_id,
        checkpoint_session_id,
        checkpoint_cursor,
        owner_actor,
        owner_peer_class,
        owner_write_namespace,
        owner_visible_namespaces,
        spawn_fingerprint,
        spawned_at,
        state_changed_at,
        idempotency_key,
    })
}

/// A concurrent spawn naming a `(provider, provider_session_id)` pair a
/// non-terminal record already holds. Read inside the same transaction as
/// the insert attempt so the check-then-act is atomic under this pool's
/// single-writer serialization; the schema's partial unique index
/// (`idx_agents_live_provider_session`, `sql/017-agents-ddl.sql`) is the
/// backstop that makes the property hold even if a future write path
/// bypasses this pre-check.
fn find_non_terminal_by_provider_session_tx(
    conn: &rusqlite::Connection,
    provider: &str,
    provider_session_id: &str,
) -> Result<Option<AgentRecord>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {AGENT_COLUMNS} FROM agents \
         WHERE provider = ?1 AND provider_session_id = ?2 AND state != 'terminal'"
    ))?;
    let mut rows = stmt.query(rusqlite::params![provider, provider_session_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(read_agent_record(row)?)),
        None => Ok(None),
    }
}

fn insert_agent_dml(
    conn: &rusqlite::Connection,
    record: &AgentRecord,
) -> Result<(), rusqlite::Error> {
    if let Some(session_id) = &record.provider_session_id {
        if record.state != AgentState::Terminal {
            if let Some(holder) =
                find_non_terminal_by_provider_session_tx(conn, &record.provider, session_id)?
            {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                    Some(format!(
                        "non-terminal record {} already holds provider session ({}, {})",
                        holder.agent_id, record.provider, session_id
                    )),
                ));
            }
        }
    }

    let owner_visible_namespaces_json = serde_json::to_string(&record.owner_visible_namespaces)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

    conn.execute(
        &format!(
            "INSERT INTO agents ({AGENT_COLUMNS}) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"
        ),
        rusqlite::params![
            record.agent_id,
            state_as_sql(record.state),
            record.terminal_reason.map(terminal_reason_as_sql),
            record.provider,
            record.provider_session_id,
            record.checkpoint_session_id,
            record.checkpoint_cursor,
            record.owner_actor,
            record.owner_peer_class,
            record.owner_write_namespace,
            owner_visible_namespaces_json,
            record.spawn_fingerprint,
            record.spawned_at,
            record.state_changed_at,
            record.idempotency_key,
        ],
    )?;
    Ok(())
}

/// An `AgentStore` backed by SQLite (ADR-142 §1). Unlike the other stores in
/// this crate, agent-process records are not namespace-scoped — the shared
/// contract's `AgentRecord` carries no `namespace` field, matching ADR-142's
/// description of the table as a single runtime-owned process ledger.
pub struct SqlAgentStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    writer_task: Option<WriterTaskHandle>,
}

impl SqlAgentStore {
    pub fn new(pool: Arc<ConnectionPool>, is_file_backed: bool) -> Self {
        let writer_task = pool.writer_task_handle().ok().flatten();
        Self {
            pool,
            is_file_backed,
            writer_task,
        }
    }

    fn open_standalone_writer(&self) -> Result<rusqlite::Connection, StorageError> {
        self.pool
            .open_standalone_writer()
            .map_err(|e| map_sqlite_err(e, "open_agent_writer"))
    }

    fn open_standalone_reader(&self) -> Result<rusqlite::Connection, StorageError> {
        self.pool
            .open_standalone_reader()
            .map_err(|e| map_sqlite_err(e, "open_agent_reader"))
    }

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

        if self.is_file_backed {
            let conn = self.open_standalone_writer()?;
            tokio::task::spawn_blocking(move || f(&conn).map_err(|e| map_err(e, op)))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Sql, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Sql, op, e))?
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
                .map_err(|e| StorageError::driver(StorageCapability::Sql, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Sql, op, e))?
        }
    }
}

#[async_trait]
impl AgentStore for SqlAgentStore {
    async fn insert(&self, record: &AgentRecord) -> Result<(), StorageError> {
        let record = record.clone();
        self.with_writer("agent_insert", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            if let Err(e) = insert_agent_dml(conn, &record) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            conn.execute_batch("COMMIT")?;
            Ok(())
        })
        .await
    }

    async fn get(&self, agent_id: &str) -> Result<Option<AgentRecord>, StorageError> {
        let agent_id = agent_id.to_string();
        self.with_reader("agent_get", move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {AGENT_COLUMNS} FROM agents WHERE agent_id = ?1"
            ))?;
            let mut rows = stmt.query(rusqlite::params![agent_id])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_agent_record(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn update_state(
        &self,
        agent_id: &str,
        state: AgentState,
        terminal_reason: Option<TerminalReason>,
        state_changed_at: i64,
    ) -> Result<(), StorageError> {
        let agent_id = agent_id.to_string();
        self.with_writer("agent_update_state", move |conn| {
            let affected = conn.execute(
                "UPDATE agents SET state = ?1, terminal_reason = ?2, state_changed_at = ?3 \
                 WHERE agent_id = ?4",
                rusqlite::params![
                    state_as_sql(state),
                    terminal_reason.map(terminal_reason_as_sql),
                    state_changed_at,
                    agent_id,
                ],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
    }

    async fn set_checkpoint(
        &self,
        agent_id: &str,
        checkpoint_session_id: &str,
        checkpoint_cursor: i64,
    ) -> Result<(), StorageError> {
        let agent_id = agent_id.to_string();
        let checkpoint_session_id = checkpoint_session_id.to_string();
        self.with_writer("agent_set_checkpoint", move |conn| {
            let affected = conn.execute(
                "UPDATE agents SET checkpoint_session_id = ?1, checkpoint_cursor = ?2 \
                 WHERE agent_id = ?3",
                rusqlite::params![checkpoint_session_id, checkpoint_cursor, agent_id],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
    }

    async fn find_by_idempotency(
        &self,
        owner_actor: &str,
        idempotency_key: &str,
    ) -> Result<Option<AgentRecord>, StorageError> {
        let owner_actor = owner_actor.to_string();
        let idempotency_key = idempotency_key.to_string();
        self.with_reader("agent_find_by_idempotency", move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {AGENT_COLUMNS} FROM agents \
                 WHERE owner_actor = ?1 AND idempotency_key = ?2"
            ))?;
            let mut rows = stmt.query(rusqlite::params![owner_actor, idempotency_key])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_agent_record(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn find_non_terminal_by_provider_session(
        &self,
        provider: &str,
        provider_session_id: &str,
    ) -> Result<Option<AgentRecord>, StorageError> {
        let provider = provider.to_string();
        let provider_session_id = provider_session_id.to_string();
        self.with_reader("agent_find_non_terminal_by_provider_session", move |conn| {
            find_non_terminal_by_provider_session_tx(conn, &provider, &provider_session_id)
        })
        .await
    }

    async fn terminate_all_non_terminal(&self, state_changed_at: i64) -> Result<u64, StorageError> {
        self.with_writer("agent_terminate_all_non_terminal", move |conn| {
            let affected = conn.execute(
                "UPDATE agents SET state = 'terminal', terminal_reason = 'host_restart', \
                 state_changed_at = ?1 WHERE state != 'terminal'",
                rusqlite::params![state_changed_at],
            )?;
            Ok(affected as u64)
        })
        .await
    }
}

// =============================================================================
// DDL
// =============================================================================

const AGENTS_DDL: &str = include_str!("../../sql/017-agents-ddl.sql");

pub(crate) fn ensure_agents_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(AGENTS_DDL)
}

#[cfg(test)]
#[path = "agents_tests.rs"]
mod tests;
