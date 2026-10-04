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

impl AgentState {
    /// Canonical snake_case name, as stored and as serialized on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Terminal => "terminal",
        }
    }
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

impl TerminalReason {
    /// Canonical snake_case name, as stored and as serialized on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Abandoned => "abandoned",
            Self::HostRestart => "host_restart",
        }
    }
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

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    #[test]
    fn agent_state_as_str_matches_serde_wire_spelling() {
        let cases = [
            (AgentState::Spawned, "spawned"),
            (AgentState::Running, "running"),
            (AgentState::Suspended, "suspended"),
            (AgentState::Terminal, "terminal"),
        ];
        for (state, spelling) in cases {
            let wire = serde_json::to_value(state).unwrap();
            assert_eq!(wire.as_str(), Some(state.as_str()), "{state:?}");
            assert_eq!(state.as_str(), spelling);
        }
    }

    #[test]
    fn terminal_reason_as_str_matches_serde_wire_spelling() {
        let cases = [
            (TerminalReason::Completed, "completed"),
            (TerminalReason::Failed, "failed"),
            (TerminalReason::Killed, "killed"),
            (TerminalReason::Abandoned, "abandoned"),
            (TerminalReason::HostRestart, "host_restart"),
        ];
        for (reason, spelling) in cases {
            let wire = serde_json::to_value(reason).unwrap();
            assert_eq!(wire.as_str(), Some(reason.as_str()), "{reason:?}");
            assert_eq!(reason.as_str(), spelling);
        }
    }
}
