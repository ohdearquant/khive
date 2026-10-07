//! End-to-end smoke test for the agent pack: spawn -> observe -> suspend ->
//! resume -> kill through the `VerbRegistry` dispatch path, against a local
//! in-memory `AgentStore` test double (ADR-142 §1). Mirrors the shape of
//! `khive-pack-blob/tests/integration.rs`.
//!
//! This crate does not depend on `khive-db`'s real `AgentStore`
//! implementation — the pack only ever sees the trait object, and this
//! test double proves that boundary holds.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use khive_pack_agent::AgentPack;
use khive_runtime::{VerbRegistry, VerbRegistryBuilder};
use khive_storage::{AgentStore, StorageError};
use khive_types::{AgentRecord, AgentState, Pack, TerminalReason};

enum CasMiss {
    Unchanged,
    State(AgentState, Option<TerminalReason>),
    Gone,
}

#[derive(Default)]
struct MockAgentStore {
    records: Mutex<HashMap<String, AgentRecord>>,
    cas_misses: Mutex<VecDeque<CasMiss>>,
    cas_expected: Mutex<Vec<AgentState>>,
}

#[async_trait]
impl AgentStore for MockAgentStore {
    async fn insert(&self, record: &AgentRecord) -> Result<(), StorageError> {
        self.records
            .lock()
            .unwrap()
            .insert(record.agent_id.clone(), record.clone());
        Ok(())
    }

    async fn get(&self, agent_id: &str) -> Result<Option<AgentRecord>, StorageError> {
        Ok(self.records.lock().unwrap().get(agent_id).cloned())
    }

    async fn update_state(
        &self,
        agent_id: &str,
        state: AgentState,
        terminal_reason: Option<TerminalReason>,
        state_changed_at: i64,
    ) -> Result<(), StorageError> {
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(agent_id).expect("agent_id exists");
        record.state = state;
        record.terminal_reason = terminal_reason;
        record.state_changed_at = state_changed_at;
        Ok(())
    }

    async fn transition_state(
        &self,
        agent_id: &str,
        expected: AgentState,
        state: AgentState,
        terminal_reason: Option<TerminalReason>,
        state_changed_at: i64,
    ) -> Result<bool, StorageError> {
        let mut records = self.records.lock().unwrap();
        self.cas_expected.lock().unwrap().push(expected);
        if let Some(miss) = self.cas_misses.lock().unwrap().pop_front() {
            match miss {
                // Models a competing state change followed by a return to the
                // previous state before the handler's reread (an ABA race).
                CasMiss::Unchanged => {}
                CasMiss::State(state, reason) => {
                    let record = records.get_mut(agent_id).unwrap();
                    record.state = state;
                    record.terminal_reason = reason;
                    record.state_changed_at = 777;
                    record.checkpoint_session_id = Some("raced-checkpoint".into());
                }
                CasMiss::Gone => {
                    records.remove(agent_id);
                }
            }
            return Ok(false);
        }
        let Some(record) = records.get_mut(agent_id) else {
            return Ok(false);
        };
        if record.state != expected {
            return Ok(false);
        }
        record.state = state;
        record.terminal_reason = terminal_reason;
        record.state_changed_at = state_changed_at;
        Ok(true)
    }

    async fn set_checkpoint(
        &self,
        agent_id: &str,
        checkpoint_session_id: &str,
        checkpoint_cursor: i64,
    ) -> Result<(), StorageError> {
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(agent_id).expect("agent_id exists");
        record.checkpoint_session_id = Some(checkpoint_session_id.to_string());
        record.checkpoint_cursor = Some(checkpoint_cursor);
        Ok(())
    }

    async fn find_by_idempotency(
        &self,
        owner_actor: &str,
        idempotency_key: &str,
    ) -> Result<Option<AgentRecord>, StorageError> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .values()
            .find(|r| {
                r.owner_actor == owner_actor
                    && r.idempotency_key.as_deref() == Some(idempotency_key)
            })
            .cloned())
    }

    async fn find_non_terminal_by_provider_session(
        &self,
        provider: &str,
        provider_session_id: &str,
    ) -> Result<Option<AgentRecord>, StorageError> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .values()
            .find(|r| {
                r.provider == provider
                    && r.provider_session_id.as_deref() == Some(provider_session_id)
                    && r.state != AgentState::Terminal
            })
            .cloned())
    }

    async fn terminate_all_non_terminal(&self, state_changed_at: i64) -> Result<u64, StorageError> {
        let mut records = self.records.lock().unwrap();
        let mut moved = 0u64;
        for record in records.values_mut() {
            if record.state != AgentState::Terminal {
                record.state = AgentState::Terminal;
                record.terminal_reason = Some(TerminalReason::HostRestart);
                record.state_changed_at = state_changed_at;
                moved += 1;
            }
        }
        Ok(moved)
    }
}

fn build_registry() -> (VerbRegistry, Arc<MockAgentStore>) {
    let store = Arc::new(MockAgentStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AgentPack::new(store.clone() as Arc<dyn AgentStore>));
    let registry = builder.build().expect("registry builds");
    (registry, store)
}

#[test]
fn agent_pack_name_and_requires_are_stable() {
    assert_eq!(AgentPack::NAME, "agent");
    assert!(AgentPack::REQUIRES.is_empty());
    assert!(AgentPack::NOTE_KINDS.is_empty());
    assert!(AgentPack::ENTITY_KINDS.is_empty());
}

async fn seed(store: &MockAgentStore) -> String {
    let id = "stored-agent".to_string();
    store
        .insert(&AgentRecord {
            agent_id: id.clone(),
            state: AgentState::Spawned,
            terminal_reason: None,
            provider: "local".into(),
            provider_session_id: None,
            checkpoint_session_id: None,
            checkpoint_cursor: None,
            owner_actor: "operator".into(),
            owner_peer_class: "native".into(),
            owner_write_namespace: "local".into(),
            owner_visible_namespaces: vec!["local".into()],
            spawn_fingerprint: "fixture".into(),
            spawned_at: 1,
            state_changed_at: 1,
            idempotency_key: None,
        })
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn stored_observe_suspend_resume_kill_round_trips() {
    let (registry, store) = build_registry();
    let agent_id = seed(&store).await;

    let observed = registry
        .dispatch("agent.observe", serde_json::json!({ "id": agent_id }))
        .await
        .expect("agent.observe dispatches");
    assert_eq!(observed["state"], "spawned");
    assert_eq!(observed["provider"], "local");
    assert!(observed["terminal_reason"].is_null());

    // agent.suspend from `spawned` is not a legal transition — the state
    // machine only allows it from `running` (ADR-142 §1). Driving `spawned`
    // to `running` is an automatic transition the provider dispatcher makes,
    // not something any verb in this pack's surface triggers, so it is out
    // of reach of this pack-level test.
    let bad_suspend = registry
        .dispatch("agent.suspend", serde_json::json!({ "id": agent_id }))
        .await;
    assert!(bad_suspend.is_err());

    // agent.kill is legal from `spawned`, `running`, or `suspended`.
    let kill = registry
        .dispatch("agent.kill", serde_json::json!({ "id": agent_id }))
        .await
        .expect("agent.kill dispatches");
    assert_eq!(kill["state"], "terminal");
    assert_eq!(kill["terminal_reason"], "killed");

    // agent.kill on an already-terminal record is a no-op, never an error.
    let kill_again = registry
        .dispatch("agent.kill", serde_json::json!({ "id": agent_id }))
        .await
        .expect("agent.kill on terminal is a no-op, not an error");
    assert_eq!(kill_again["state"], "terminal");
    assert_eq!(kill_again["terminal_reason"], "killed");

    // agent.resume on a terminal record is an illegal-transition error.
    let resume_terminal = registry
        .dispatch("agent.resume", serde_json::json!({ "id": agent_id }))
        .await;
    assert!(resume_terminal.is_err());
}

#[tokio::test]
async fn spawn_validation_failure_is_a_per_operation_error() {
    let (registry, _store) = build_registry();

    let missing_task = registry
        .dispatch("agent.spawn", serde_json::json!({ "provider": "local" }))
        .await;
    assert!(missing_task.is_err());
}

#[tokio::test]
async fn observe_unknown_agent_id_is_a_per_operation_error() {
    let (registry, _store) = build_registry();

    let err = registry
        .dispatch(
            "agent.observe",
            serde_json::json!({ "id": "00000000-0000-0000-0000-000000000000" }),
        )
        .await;
    assert!(err.is_err());
}

#[tokio::test]
async fn suspend_and_resume_round_trip_from_running() {
    let (registry, store) = build_registry();

    let agent_id = seed(&store).await;

    // Drive `spawned` -> `running` directly on the store, standing in for
    // the automatic transition this pack's verb surface does not itself
    // trigger (ADR-142 §1's transition table, row 2).
    store
        .update_state(&agent_id, AgentState::Running, None, 1)
        .await
        .expect("mock update_state");

    let suspend = registry
        .dispatch("agent.suspend", serde_json::json!({ "id": agent_id }))
        .await
        .expect("agent.suspend from running dispatches");
    assert_eq!(suspend["state"], "suspended");

    // agent.suspend on an already-suspended record is a no-op.
    let suspend_again = registry
        .dispatch("agent.suspend", serde_json::json!({ "id": agent_id }))
        .await
        .expect("agent.suspend on suspended is a no-op, not an error");
    assert_eq!(suspend_again["state"], "suspended");

    let resume = registry
        .dispatch("agent.resume", serde_json::json!({ "id": agent_id }))
        .await
        .expect("agent.resume from suspended dispatches");
    assert_eq!(resume["state"], "running");

    // agent.resume on an already-running record is a no-op.
    let resume_again = registry
        .dispatch("agent.resume", serde_json::json!({ "id": agent_id }))
        .await
        .expect("agent.resume on running is a no-op, not an error");
    assert_eq!(resume_again["state"], "running");
}

#[tokio::test]
async fn unavailable_providers_never_write_and_do_not_poison_observe() {
    let (registry, store) = build_registry();
    for provider in ["local", "x", "https://provider.invalid", "sk-secret"] {
        let error = registry
            .dispatch(
                "agent.spawn",
                serde_json::json!({"provider": provider, "task": "t", "idempotency_key": "same"}),
            )
            .await
            .unwrap_err();
        let khive_runtime::RuntimeError::Khive(error) = error else {
            panic!("typed refusal")
        };
        assert_eq!(
            serde_json::to_value(error).unwrap()["details"]["reason"],
            "provider_unavailable"
        );
        assert!(store.records.lock().unwrap().is_empty());
    }
    let id = seed(&store).await;
    assert_eq!(
        registry
            .dispatch("agent.observe", serde_json::json!({"id": id}))
            .await
            .unwrap()["agent_id"],
        id
    );
}

async fn cas_fixture(initial: AgentState) -> (VerbRegistry, Arc<MockAgentStore>, String) {
    let (registry, store) = build_registry();
    let id = seed(&store).await;
    store.update_state(&id, initial, None, 1).await.unwrap();
    (registry, store, id)
}

#[tokio::test]
async fn lifecycle_cas_miss_rereads_into_normal_noop() {
    for (verb, initial, current, reason) in [
        (
            "agent.suspend",
            AgentState::Running,
            AgentState::Suspended,
            None,
        ),
        (
            "agent.resume",
            AgentState::Suspended,
            AgentState::Running,
            None,
        ),
        (
            "agent.kill",
            AgentState::Running,
            AgentState::Terminal,
            Some(TerminalReason::Completed),
        ),
    ] {
        let (registry, store, id) = cas_fixture(initial).await;
        store
            .cas_misses
            .lock()
            .unwrap()
            .push_back(CasMiss::State(current, reason));
        let response = registry
            .dispatch(verb, serde_json::json!({"id": id}))
            .await
            .unwrap();
        let expected = match verb {
            "agent.suspend" => {
                serde_json::json!({"agent_id": id, "state": "suspended", "checkpoint_session_id": "raced-checkpoint"})
            }
            "agent.resume" => serde_json::json!({"agent_id": id, "state": "running"}),
            _ => {
                serde_json::json!({"agent_id": id, "state": "terminal", "terminal_reason": "completed"})
            }
        };
        assert_eq!(response, expected);
        assert_eq!(*store.cas_expected.lock().unwrap(), vec![initial]);
        let record = store.get(&id).await.unwrap().unwrap();
        assert_eq!(record.state, current);
        assert_eq!(record.terminal_reason, reason);
        assert_eq!(record.state_changed_at, 777);
    }
}

#[tokio::test]
async fn lifecycle_cas_miss_retries_a_still_legal_change() {
    for (verb, initial, target) in [
        ("agent.suspend", AgentState::Running, AgentState::Suspended),
        ("agent.resume", AgentState::Suspended, AgentState::Running),
        ("agent.kill", AgentState::Running, AgentState::Terminal),
    ] {
        let (registry, store, id) = cas_fixture(initial).await;
        let retry_from = if verb == "agent.kill" {
            AgentState::Suspended
        } else {
            initial
        };
        let miss = if verb == "agent.kill" {
            CasMiss::State(retry_from, None)
        } else {
            CasMiss::Unchanged
        };
        store.cas_misses.lock().unwrap().push_back(miss);
        let response = registry
            .dispatch(verb, serde_json::json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(response["agent_id"], id);
        assert_eq!(response["state"], target.as_str());
        assert_eq!(
            *store.cas_expected.lock().unwrap(),
            vec![initial, retry_from]
        );
        let record = store.get(&id).await.unwrap().unwrap();
        assert_eq!(record.state, target);
        assert_eq!(
            record.terminal_reason,
            if target == AgentState::Terminal {
                Some(TerminalReason::Killed)
            } else {
                None
            }
        );
    }
}

#[tokio::test]
async fn lifecycle_cas_misses_preserve_refusals_and_cap_attempts() {
    for (verb, initial) in [
        ("agent.suspend", AgentState::Running),
        ("agent.resume", AgentState::Suspended),
        ("agent.kill", AgentState::Running),
    ] {
        let (registry, store, id) = cas_fixture(initial).await;
        let before = serde_json::to_value(store.get(&id).await.unwrap().unwrap()).unwrap();
        store.cas_misses.lock().unwrap().extend([
            CasMiss::Unchanged,
            CasMiss::Unchanged,
            CasMiss::Unchanged,
        ]);
        let error = registry
            .dispatch(verb, serde_json::json!({"id": id}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, khive_runtime::RuntimeError::InvalidInput(message) if message == format!("{verb}: state changed concurrently; retry"))
        );
        assert_eq!(*store.cas_expected.lock().unwrap(), vec![initial; 3]);
        assert_eq!(
            serde_json::to_value(store.get(&id).await.unwrap().unwrap()).unwrap(),
            before
        );

        let (registry, store, id) = cas_fixture(initial).await;
        store.cas_misses.lock().unwrap().push_back(CasMiss::Gone);
        let error = registry
            .dispatch(verb, serde_json::json!({"id": id}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, khive_runtime::RuntimeError::NotFound(message) if message == format!("{verb}: unknown agent_id {id:?}"))
        );
        assert_eq!(*store.cas_expected.lock().unwrap(), vec![initial]);
        assert!(store.get(&id).await.unwrap().is_none());

        if verb != "agent.kill" {
            let (registry, store, id) = cas_fixture(initial).await;
            store.cas_misses.lock().unwrap().push_back(CasMiss::State(
                AgentState::Terminal,
                Some(TerminalReason::Killed),
            ));
            let error = registry
                .dispatch(verb, serde_json::json!({"id": id}))
                .await
                .unwrap_err();
            assert!(
                matches!(error, khive_runtime::RuntimeError::InvalidInput(message) if message == format!("{verb}: illegal transition from terminal for agent_id {id:?}"))
            );
            assert_eq!(*store.cas_expected.lock().unwrap(), vec![initial]);
            let record = store.get(&id).await.unwrap().unwrap();
            assert_eq!(record.state, AgentState::Terminal);
            assert_eq!(record.terminal_reason, Some(TerminalReason::Killed));
            assert_eq!(record.state_changed_at, 777);
        }
    }
}

#[tokio::test]
async fn lifecycle_last_cas_miss_still_rereads_before_retry_refusal() {
    let (registry, store, id) = cas_fixture(AgentState::Running).await;
    store.cas_misses.lock().unwrap().extend([
        CasMiss::Unchanged,
        CasMiss::Unchanged,
        CasMiss::State(AgentState::Suspended, None),
    ]);
    let response = registry
        .dispatch("agent.suspend", serde_json::json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(
        response,
        serde_json::json!({"agent_id": id, "state": "suspended", "checkpoint_session_id": "raced-checkpoint"})
    );
    assert_eq!(
        *store.cas_expected.lock().unwrap(),
        vec![AgentState::Running; 3]
    );
    assert_eq!(store.get(&id).await.unwrap().unwrap().state_changed_at, 777);
}
