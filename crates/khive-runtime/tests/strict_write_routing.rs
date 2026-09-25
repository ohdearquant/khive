use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use khive_db::StorageBackend;
use khive_runtime::{
    ContentMergeStrategy, EdgePatch, EntityDedupMergePolicy, KhiveRuntime, Namespace,
    NamespaceToken, RuntimeConfig, RuntimeError,
};
use khive_storage::{EdgeRelation, LinkId, StorageError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

const PHASE_ENV: &str = "KHIVE_STRICT_WRITE_TEST_PHASE";
const DATABASE_ENV: &str = "KHIVE_STRICT_WRITE_TEST_DATABASE";

#[derive(Serialize, Deserialize)]
struct SeedIds {
    into_entity: Uuid,
    from_entity: Uuid,
    into_note: Uuid,
    from_note: Uuid,
    rewired_edge: Uuid,
    symmetric_edge: Uuid,
}

fn runtime(path: &Path) -> (KhiveRuntime, NamespaceToken) {
    let backend = Arc::new(StorageBackend::sqlite_for_test(path).unwrap());
    backend.prepare_core_schema().unwrap();
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: Some(path.to_owned()),
            events_split: None,
            actor_id: Some("test:strict-runtime-writes".into()),
            packs: vec!["kg".into()],
            ..RuntimeConfig::no_embeddings()
        },
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    (runtime, token)
}

async fn seed(runtime: &KhiveRuntime, token: &NamespaceToken) -> SeedIds {
    let into = runtime
        .create_entity(token, "concept", None, "Kept", None, None, vec![])
        .await
        .unwrap();
    let from = runtime
        .create_entity(token, "concept", None, "Merged", None, None, vec![])
        .await
        .unwrap();
    let left = runtime
        .create_entity(token, "concept", None, "Left", None, None, vec![])
        .await
        .unwrap();
    let right = runtime
        .create_entity(token, "concept", None, "Right", None, None, vec![])
        .await
        .unwrap();
    let into_note = runtime
        .create_note(token, "observation", None, "First note", None, None, vec![])
        .await
        .unwrap();
    let from_note = runtime
        .create_note(
            token,
            "observation",
            None,
            "Second note",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let rewired = runtime
        .link(token, from.id, left.id, EdgeRelation::Extends, 0.25, None)
        .await
        .unwrap();
    let symmetric = runtime
        .link(
            token,
            left.id,
            right.id,
            EdgeRelation::CompetesWith,
            0.5,
            None,
        )
        .await
        .unwrap();
    SeedIds {
        into_entity: into.id,
        from_entity: from.id,
        into_note: into_note.id,
        from_note: from_note.id,
        rewired_edge: rewired.id.into(),
        symmetric_edge: symmetric.id.into(),
    }
}

async fn snapshot(runtime: &KhiveRuntime, token: &NamespaceToken, ids: &SeedIds) -> Value {
    json!({
        "into_entity": runtime.get_entity_including_deleted(token, ids.into_entity).await.unwrap(),
        "from_entity": runtime.get_entity_including_deleted(token, ids.from_entity).await.unwrap(),
        "into_note": runtime.get_note_including_deleted(token, ids.into_note).await.unwrap(),
        "from_note": runtime.get_note_including_deleted(token, ids.from_note).await.unwrap(),
        "rewired_edge": runtime.graph(token).unwrap().get_edge(LinkId::from(ids.rewired_edge)).await.unwrap(),
        "symmetric_edge": runtime.graph(token).unwrap().get_edge(LinkId::from(ids.symmetric_edge)).await.unwrap(),
    })
}

fn assert_missing_handle(error: RuntimeError, expected_operation: &str) {
    match error {
        RuntimeError::Storage(StorageError::Pool { operation, message }) => {
            assert_eq!(operation, expected_operation);
            assert!(message.contains("strict"));
            assert!(message.contains("writer-task handle"));
        }
        other => panic!("expected typed missing-handle refusal, got {other:?}"),
    }
}

async fn exercise(runtime: &KhiveRuntime, token: &NamespaceToken, ids: &SeedIds, refuses: bool) {
    let before = snapshot(runtime, token, ids).await;
    let entity_result = runtime
        .merge_entity(
            token,
            ids.into_entity,
            ids.from_entity,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await;
    let note_result = runtime
        .merge_note(
            token,
            ids.into_note,
            ids.from_note,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await;
    let edge_result = runtime
        .update_edge(
            token,
            ids.symmetric_edge,
            EdgePatch {
                weight: Some(0.75),
                ..Default::default()
            },
        )
        .await;
    if refuses {
        assert_missing_handle(entity_result.unwrap_err(), "merge_entity");
        assert_missing_handle(note_result.unwrap_err(), "merge_note");
        assert_missing_handle(edge_result.unwrap_err(), "update_edge");
        assert_eq!(snapshot(runtime, token, ids).await, before);
    } else {
        assert_eq!(entity_result.unwrap().kept_id, ids.into_entity);
        assert_eq!(note_result.unwrap().kept_id, ids.into_note);
        assert_eq!(edge_result.unwrap().weight, 0.75);
        let after = snapshot(runtime, token, ids).await;
        assert_eq!(after["from_entity"]["merged_into"], json!(ids.into_entity));
        assert!(after["from_entity"]["deleted_at"].is_number());
        assert!(after["from_note"]["deleted_at"].is_number());
        assert!(after["into_note"]["content"]
            .as_str()
            .unwrap()
            .contains("Second note"));
        assert_eq!(after["rewired_edge"]["source_id"], json!(ids.into_entity));
        assert_eq!(after["symmetric_edge"]["weight"], json!(0.75));
    }
}

fn run_case(test_name: &str, strict: bool, queue_enabled: bool) {
    if let Ok(phase) = std::env::var(PHASE_ENV) {
        let database = std::path::PathBuf::from(std::env::var_os(DATABASE_ENV).unwrap());
        let seed_path = database.with_extension("seed.json");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (runtime, token) = runtime(&database);
            if phase == "seed" {
                let ids = seed(&runtime, &token).await;
                std::fs::write(&seed_path, serde_json::to_vec(&ids).unwrap()).unwrap();
            } else {
                assert_eq!(phase, "exercise");
                let pool = runtime.backend().pool_arc();
                assert_eq!(pool.config().write_routing_strict, strict);
                assert_eq!(pool.config().write_queue_enabled, Some(queue_enabled));
                assert_eq!(pool.writer_task_handle().unwrap().is_some(), queue_enabled);
                let ids = serde_json::from_slice(&std::fs::read(seed_path).unwrap()).unwrap();
                exercise(&runtime, &token, &ids, strict && !queue_enabled).await;
            }
        });
        return;
    }

    // Pool routing is environment-configured. Separate child processes seed in
    // compatibility mode and exercise the reopened database without mutating
    // the test runner's environment or sharing its process-global writer state.
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("routing.sqlite3");
    for phase in ["seed", "exercise"] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        let output = command
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .current_dir(temp.path())
            .env("KHIVE_TEST_HARNESS", "1")
            .env(PHASE_ENV, phase)
            .env(DATABASE_ENV, &database)
            .env(
                "KHIVE_WRITE_ROUTING",
                if phase == "exercise" && strict {
                    "strict"
                } else {
                    "compat"
                },
            )
            .env(
                "KHIVE_WRITE_QUEUE",
                if phase == "exercise" && queue_enabled {
                    "1"
                } else {
                    "0"
                },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{phase} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn strict_missing_handle_refuses_all_runtime_transactions_without_mutation() {
    run_case(
        "strict_missing_handle_refuses_all_runtime_transactions_without_mutation",
        true,
        false,
    );
}

#[test]
fn strict_enabled_queue_preserves_runtime_transaction_results() {
    run_case(
        "strict_enabled_queue_preserves_runtime_transaction_results",
        true,
        true,
    );
}

#[test]
fn compatibility_queue_opt_out_preserves_runtime_transaction_results() {
    run_case(
        "compatibility_queue_opt_out_preserves_runtime_transaction_results",
        false,
        false,
    );
}
