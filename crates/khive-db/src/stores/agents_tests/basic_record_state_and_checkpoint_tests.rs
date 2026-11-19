#[tokio::test]
async fn test_get_missing_returns_none() {
    let store = setup_memory_store();
    assert!(store.get("does-not-exist").await.unwrap().is_none());
}

#[tokio::test]
async fn test_update_state_and_terminal_reason() {
    let store = setup_memory_store();
    let record = make_record("agent-2", "actor-a");
    store.insert(&record).await.unwrap();

    store
        .update_state("agent-2", AgentState::Running, None, 2_000)
        .await
        .unwrap();
    let fetched = store.get("agent-2").await.unwrap().unwrap();
    assert_eq!(fetched.state, AgentState::Running);
    assert_eq!(fetched.state_changed_at, 2_000);

    store
        .update_state(
            "agent-2",
            AgentState::Terminal,
            Some(TerminalReason::Completed),
            3_000,
        )
        .await
        .unwrap();
    let fetched = store.get("agent-2").await.unwrap().unwrap();
    assert_eq!(fetched.state, AgentState::Terminal);
    assert_eq!(fetched.terminal_reason, Some(TerminalReason::Completed));

    // Every spelling `as_str` writes must read back as the same variant.
    let non_terminal = [
        AgentState::Spawned,
        AgentState::Running,
        AgentState::Suspended,
    ];
    for state in non_terminal {
        store
            .update_state("agent-2", state, None, 4_000)
            .await
            .unwrap();
        let fetched = store.get("agent-2").await.unwrap().unwrap();
        assert_eq!(fetched.state, state);
        assert_eq!(fetched.terminal_reason, None);
    }

    let reasons = [
        TerminalReason::Completed,
        TerminalReason::Failed,
        TerminalReason::Killed,
        TerminalReason::Abandoned,
        TerminalReason::HostRestart,
    ];
    for reason in reasons {
        store
            .update_state("agent-2", AgentState::Terminal, Some(reason), 5_000)
            .await
            .unwrap();
        let fetched = store.get("agent-2").await.unwrap().unwrap();
        assert_eq!(fetched.state, AgentState::Terminal);
        assert_eq!(fetched.terminal_reason, Some(reason));
    }
}

#[tokio::test]
async fn test_set_checkpoint() {
    let store = setup_memory_store();
    let record = make_record("agent-3", "actor-a");
    store.insert(&record).await.unwrap();

    store
        .set_checkpoint("agent-3", "session-abc", 42)
        .await
        .unwrap();

    let fetched = store.get("agent-3").await.unwrap().unwrap();
    assert_eq!(
        fetched.checkpoint_session_id,
        Some("session-abc".to_string())
    );
    assert_eq!(fetched.checkpoint_cursor, Some(42));
}
