//! End-to-end smoke test for the agent pack: spawn -> observe -> suspend ->
//! resume -> kill through the `VerbRegistry` dispatch path, against a local
//! in-memory `AgentStore` test double (ADR-142 §1). Mirrors the shape of
//! `khive-pack-blob/tests/integration.rs`.
//!
//! This crate does not depend on `khive-db`'s real `AgentStore`
//! implementation — the pack only ever sees the trait object, and this
//! test double proves that boundary holds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use khive_pack_agent::AgentPack;
use khive_runtime::{VerbRegistry, VerbRegistryBuilder};
use khive_storage::{AgentStore, StorageError};
use khive_types::{AgentRecord, AgentState, Pack, TerminalReason};

#[derive(Default)]
struct MockAgentStore {
    records: Mutex<HashMap<String, AgentRecord>>,
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
