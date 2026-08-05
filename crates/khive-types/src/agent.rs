//! Agent process lifecycle types (ADR-142 §1, "Persistent process record").

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// One of the four lifecycle states an agent process record can occupy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum AgentState {
    Spawned,
    Running,
    Suspended,
    Terminal,
}

/// Why a record reached `Terminal`. Set exactly once, at the transition into `Terminal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum TerminalReason {
    Completed,
    Failed,
    Killed,
    Abandoned,
    HostRestart,
}

/// The runtime-owned agent process record (ADR-142 §1, "Persistent process record").
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct AgentRecord {
    pub agent_id: String,
    pub state: AgentState,
    pub terminal_reason: Option<TerminalReason>,
    pub provider: String,
    pub provider_session_id: Option<String>,
    pub checkpoint_session_id: Option<String>,
    pub checkpoint_cursor: Option<i64>,
    pub owner_actor: String,
    pub owner_peer_class: String,
    pub owner_write_namespace: String,
    pub owner_visible_namespaces: Vec<String>,
    pub spawn_fingerprint: String,
    pub spawned_at: i64,
    pub state_changed_at: i64,
    pub idempotency_key: Option<String>,
}
