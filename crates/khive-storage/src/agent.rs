//! Agent process store capability (ADR-142 §1, "Process model").

use async_trait::async_trait;
use khive_types::{AgentRecord, AgentState, TerminalReason};

use crate::error::StorageError;

/// Durable storage for runtime-owned agent process records.
#[async_trait]
pub trait AgentStore: Send + Sync {
    async fn insert(&self, record: &AgentRecord) -> Result<(), StorageError>;
    async fn get(&self, agent_id: &str) -> Result<Option<AgentRecord>, StorageError>;
    async fn update_state(
        &self,
        agent_id: &str,
        state: AgentState,
        terminal_reason: Option<TerminalReason>,
        state_changed_at: i64,
    ) -> Result<(), StorageError>;
    async fn set_checkpoint(
        &self,
        agent_id: &str,
        checkpoint_session_id: &str,
        checkpoint_cursor: i64,
    ) -> Result<(), StorageError>;
    async fn find_by_idempotency(
        &self,
        owner_actor: &str,
        idempotency_key: &str,
    ) -> Result<Option<AgentRecord>, StorageError>;
    async fn find_non_terminal_by_provider_session(
        &self,
        provider: &str,
        provider_session_id: &str,
    ) -> Result<Option<AgentRecord>, StorageError>;
    async fn terminate_all_non_terminal(&self, state_changed_at: i64) -> Result<u64, StorageError>;
}
