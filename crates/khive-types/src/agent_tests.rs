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
