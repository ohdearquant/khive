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

/// #1847: the writer task owns the transaction around the agent ledger's
/// provider-session pre-check plus insert. Sending the legacy BEGIN wrapper
/// through the queue would deterministically fail as a nested transaction.
#[tokio::test]
async fn agent_insert_uses_writer_task_owned_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("agent-writer-task-transaction.db")),
            write_queue_enabled: Some(true),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(AGENTS_DDL).unwrap();
    }
    let before = pool.writer_acquisition_snapshot();
    let store = SqlAgentStore::new(Arc::clone(&pool), true);

    store
        .insert(&make_record("agent-queued", "actor-a"))
        .await
        .expect("agent insert must use the writer task's existing transaction");

    let after = pool.writer_acquisition_snapshot();
    assert_eq!(
        after.writer_task_acquisitions,
        before.writer_task_acquisitions + 1,
        "agent insert must acquire the shared writer task exactly once"
    );
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

/// #1847: the agent-process ledger was added after the original strict-route
/// census. It must fail closed like every other store when strict routing is
/// requested without a writer-task handle.
#[test]
fn agent_write_strict_routing_fails_closed_without_writer_task() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("agent-strict-routing.db")),
            write_queue_enabled: Some(false),
            write_routing_strict: true,
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(AGENTS_DDL).unwrap();
    }
    let store = SqlAgentStore::new(pool, true);

    let error = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(store.insert(&make_record("agent-strict", "actor-a")))
        .expect_err("strict routing must refuse the standalone writer fallback");
    assert!(
        matches!(
            &error,
            StorageError::Pool { operation, .. } if operation == "agent_insert"
        ),
        "strict routing must return the typed operation error, got: {error:?}"
    );
    assert!(error.to_string().contains("strict"), "got: {error}");
}

fn cas_pool(path: &std::path::Path, queued: bool) -> Arc<ConnectionPool> {
    let config = PoolConfig {
        path: Some(path.to_path_buf()),
        volume_lock_dir: Some(path.parent().unwrap().join("volume-locks")),
        wal_ceiling: Default::default(),
        disk_guard_config: Some(crate::EffectiveDiskGuardConfig {
            reserve_bytes: 0,
            ..Default::default()
        }),
        busy_timeout: std::time::Duration::from_secs(5),
        checkout_timeout: std::time::Duration::from_secs(5),
        write_queue_enabled: Some(queued),
        write_queue_capacity: 16,
        write_routing_strict: queued,
        write_admission_deadline_ms: 2_000,
        ..PoolConfig::for_test()
    };
    assert_eq!(config.path.as_deref(), Some(path));
    Arc::new(ConnectionPool::new(config).unwrap())
}

#[tokio::test]
async fn state_cas_matches_id_and_expected_state_without_changing_other_fields() {
    let dir = tempfile::tempdir().unwrap();
    let pool = cas_pool(&dir.path().join("agent-cas.db"), false);
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(AGENTS_DDL).unwrap();
    }
    let store = SqlAgentStore::new(pool, true);
    let mut record = make_record("agent-cas", "actor-a");
    record.checkpoint_session_id = Some("checkpoint".into());
    store.insert(&record).await.unwrap();
    store
        .insert(&make_record("other-agent", "actor-b"))
        .await
        .unwrap();
    assert!(!store
        .transition_state(
            "missing",
            AgentState::Spawned,
            AgentState::Running,
            None,
            2_000
        )
        .await
        .unwrap());
    assert!(!store
        .transition_state(
            "agent-cas",
            AgentState::Running,
            AgentState::Suspended,
            None,
            2_000
        )
        .await
        .unwrap());
    assert_eq!(
        serde_json::to_value(store.get("agent-cas").await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(&record).unwrap()
    );
    assert!(store
        .transition_state(
            "agent-cas",
            AgentState::Spawned,
            AgentState::Running,
            None,
            2_000
        )
        .await
        .unwrap());
    record.state = AgentState::Running;
    record.state_changed_at = 2_000;
    assert_eq!(
        serde_json::to_value(store.get("agent-cas").await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(record).unwrap()
    );
    assert_eq!(
        serde_json::to_value(store.get("other-agent").await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(make_record("other-agent", "actor-b")).unwrap()
    );
}

#[tokio::test]
async fn independent_pools_reject_stale_suspend_and_resume_after_terminal_commit() {
    let dir = tempfile::tempdir().unwrap();
    for (index, initial, requested) in [
        (0, AgentState::Running, AgentState::Suspended),
        (1, AgentState::Suspended, AgentState::Running),
    ] {
        let path = dir.path().join(format!("agent-race-{index}.db"));
        let stale_pool = cas_pool(&path, true);
        {
            let writer = stale_pool.writer().unwrap();
            writer.conn().execute_batch(AGENTS_DDL).unwrap();
        }
        let terminal_pool = cas_pool(&path, true);
        assert!(!Arc::ptr_eq(&stale_pool, &terminal_pool));
        let stale_store = SqlAgentStore::new(stale_pool, true);
        let terminal_store = SqlAgentStore::new(terminal_pool, true);
        let mut record = make_record("racing-agent", "actor-a");
        record.state = initial;
        stale_store.insert(&record).await.unwrap();
        let read_done = tokio::sync::Barrier::new(2);
        let terminal_done = tokio::sync::Barrier::new(2);
        let stale = async {
            let observed = stale_store.get("racing-agent").await.unwrap().unwrap();
            assert_eq!(observed.state, initial);
            read_done.wait().await;
            terminal_done.wait().await;
            assert!(!stale_store
                .transition_state("racing-agent", observed.state, requested, None, 4_000)
                .await
                .unwrap());
        };
        let terminal = async {
            read_done.wait().await;
            assert!(terminal_store
                .transition_state(
                    "racing-agent",
                    initial,
                    AgentState::Terminal,
                    Some(TerminalReason::Killed),
                    3_000
                )
                .await
                .unwrap());
            terminal_done.wait().await;
        };
        tokio::join!(stale, terminal);
        record.state = AgentState::Terminal;
        record.terminal_reason = Some(TerminalReason::Killed);
        record.state_changed_at = 3_000;
        for store in [&stale_store, &terminal_store] {
            assert_eq!(
                serde_json::to_value(store.get("racing-agent").await.unwrap().unwrap()).unwrap(),
                serde_json::to_value(&record).unwrap()
            );
        }
    }
}
