use super::*;
use crate::pool::PoolConfig;

fn setup_memory_store() -> SqlAgentStore {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());

    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(AGENTS_DDL).unwrap();
    }

    SqlAgentStore::new(pool, false)
}

fn make_record(agent_id: &str, owner_actor: &str) -> AgentRecord {
    AgentRecord {
        agent_id: agent_id.to_string(),
        state: AgentState::Spawned,
        terminal_reason: None,
        provider: "test-provider".to_string(),
        provider_session_id: None,
        checkpoint_session_id: None,
        checkpoint_cursor: None,
        owner_actor: owner_actor.to_string(),
        owner_peer_class: "native".to_string(),
        owner_write_namespace: "local".to_string(),
        owner_visible_namespaces: vec!["local".to_string()],
        spawn_fingerprint: "fingerprint-1".to_string(),
        spawned_at: 1_000,
        state_changed_at: 1_000,
        idempotency_key: None,
    }
}

#[tokio::test]
async fn test_insert_and_get() {
    let store = setup_memory_store();
    let record = make_record("agent-1", "actor-a");

    store.insert(&record).await.unwrap();

    let fetched = store.get("agent-1").await.unwrap().unwrap();
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.state, AgentState::Spawned);
    assert_eq!(fetched.owner_visible_namespaces, vec!["local".to_string()]);
}

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

/// The point of the feature (ADR-142 §1 spawn row): idempotency replay is
/// keyed on the PAIR (owner_actor, idempotency_key), never the key alone.
/// Two different actors reusing the same key string must never observe or
/// interfere with each other's records.
#[tokio::test]
async fn test_idempotency_keyed_on_actor_and_key_pair() {
    let store = setup_memory_store();

    let mut record_a = make_record("agent-actor-a", "actor-a");
    record_a.idempotency_key = Some("shared-key".to_string());
    store.insert(&record_a).await.unwrap();

    let mut record_b = make_record("agent-actor-b", "actor-b");
    record_b.idempotency_key = Some("shared-key".to_string());
    store.insert(&record_b).await.unwrap();

    let found_a = store
        .find_by_idempotency("actor-a", "shared-key")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found_a.agent_id, "agent-actor-a");

    let found_b = store
        .find_by_idempotency("actor-b", "shared-key")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found_b.agent_id, "agent-actor-b");

    assert!(store
        .find_by_idempotency("actor-c", "shared-key")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_insert_rejects_duplicate_actor_key_pair() {
    let store = setup_memory_store();

    let mut record = make_record("agent-x", "actor-a");
    record.idempotency_key = Some("dup-key".to_string());
    store.insert(&record).await.unwrap();

    let mut record_conflict = make_record("agent-y", "actor-a");
    record_conflict.idempotency_key = Some("dup-key".to_string());

    let err = store.insert(&record_conflict).await.unwrap_err();
    assert!(matches!(err, StorageError::Driver { .. }));
}

/// The other property this store exists to hold: at most one non-terminal
/// record per (provider, provider_session_id). A spawn naming a pair already
/// held by a non-terminal record must be rejected.
#[tokio::test]
async fn test_rejects_second_non_terminal_for_same_provider_session() {
    let store = setup_memory_store();

    let mut record = make_record("agent-live-1", "actor-a");
    record.provider_session_id = Some("session-1".to_string());
    store.insert(&record).await.unwrap();

    let mut conflicting = make_record("agent-live-2", "actor-b");
    conflicting.provider_session_id = Some("session-1".to_string());

    let err = store.insert(&conflicting).await.unwrap_err();
    assert!(matches!(err, StorageError::Driver { .. }));

    let holder = store
        .find_non_terminal_by_provider_session("test-provider", "session-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(holder.agent_id, "agent-live-1");
}

/// A suspended record is still non-terminal and still holds its pair — a
/// second spawn against the same (provider, provider_session_id) must still
/// be rejected while the holder is merely suspended, not just while running.
#[tokio::test]
async fn test_suspended_record_still_holds_provider_session_pair() {
    let store = setup_memory_store();

    let mut record = make_record("agent-susp-1", "actor-a");
    record.provider_session_id = Some("session-2".to_string());
    store.insert(&record).await.unwrap();
    store
        .update_state("agent-susp-1", AgentState::Suspended, None, 2_000)
        .await
        .unwrap();

    let mut conflicting = make_record("agent-susp-2", "actor-b");
    conflicting.provider_session_id = Some("session-2".to_string());
    let err = store.insert(&conflicting).await.unwrap_err();
    assert!(matches!(err, StorageError::Driver { .. }));
}

/// Once the holder reaches terminal, the pair frees up: a new spawn against
/// the same (provider, provider_session_id) succeeds.
#[tokio::test]
async fn test_provider_session_pair_frees_after_terminal() {
    let store = setup_memory_store();

    let mut record = make_record("agent-term-1", "actor-a");
    record.provider_session_id = Some("session-3".to_string());
    store.insert(&record).await.unwrap();
    store
        .update_state(
            "agent-term-1",
            AgentState::Terminal,
            Some(TerminalReason::Completed),
            2_000,
        )
        .await
        .unwrap();

    let mut second = make_record("agent-term-2", "actor-b");
    second.provider_session_id = Some("session-3".to_string());
    store.insert(&second).await.unwrap();

    let holder = store
        .find_non_terminal_by_provider_session("test-provider", "session-3")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(holder.agent_id, "agent-term-2");
}

#[tokio::test]
async fn test_terminate_all_non_terminal_boot_scan() {
    let store = setup_memory_store();

    store
        .insert(&make_record("agent-a", "actor-a"))
        .await
        .unwrap();
    store
        .insert(&make_record("agent-b", "actor-b"))
        .await
        .unwrap();
    let mut terminal_record = make_record("agent-c", "actor-c");
    terminal_record.state = AgentState::Terminal;
    terminal_record.terminal_reason = Some(TerminalReason::Completed);
    store.insert(&terminal_record).await.unwrap();

    let affected = store.terminate_all_non_terminal(9_999).await.unwrap();
    assert_eq!(affected, 2);

    let a = store.get("agent-a").await.unwrap().unwrap();
    assert_eq!(a.state, AgentState::Terminal);
    assert_eq!(a.terminal_reason, Some(TerminalReason::HostRestart));
    assert_eq!(a.state_changed_at, 9_999);

    let b = store.get("agent-b").await.unwrap().unwrap();
    assert_eq!(b.state, AgentState::Terminal);
    assert_eq!(b.terminal_reason, Some(TerminalReason::HostRestart));

    // The already-terminal record is untouched.
    let c = store.get("agent-c").await.unwrap().unwrap();
    assert_eq!(c.terminal_reason, Some(TerminalReason::Completed));
    assert_eq!(c.state_changed_at, 1_000);
}
