use std::path::Path;
use std::time::Duration;

use futures::FutureExt;
use khive_runtime::daemon::{run_daemon, DaemonRequestFrame, PROTOCOL_VERSION};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use serde_json::{json, Value};
use serial_test::serial;
use tokio::time::timeout;

use super::test_harness::{clear_daemon_env, connect_when_ready, exchange, RecoveryTestGuard};
use crate::server::KhiveMcpServer;

const ACTOR: &str = "test:memory-writer";

async fn request(
    socket: &Path,
    config_id: &str,
    actor: Option<&str>,
    visible: &[&str],
    ops: String,
) -> Vec<Value> {
    let frame = DaemonRequestFrame {
        ops,
        presentation: Some("verbose".into()),
        presentation_per_op: None,
        namespace: "local".into(),
        actor_id: actor.map(str::to_owned),
        process_ref: None,
        visible_namespaces: visible.iter().map(|ns| (*ns).to_owned()).collect(),
        config_id: config_id.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: Some("json".into()),
        format_per_op: None,
        from_wire: true,
        request_id: None,
    };
    let response = timeout(Duration::from_secs(10), exchange(socket, &frame))
        .await
        .expect("bounded daemon exchange");
    assert!(response.ok, "frame failed: {:?}", response.error);
    assert!(!response.namespace_mismatch);
    assert!(!response.config_mismatch);
    assert!(!response.version_mismatch);
    let body: Value = serde_json::from_str(response.result.as_deref().expect("frame result"))
        .expect("JSON response");
    let operations = body["results"].as_array().expect("operation receipts");
    assert!(!operations.is_empty(), "empty receipts: {body}");
    operations
        .iter()
        .map(|operation| {
            assert_eq!(operation["ok"], true, "inner operation failed: {body}");
            operation["result"].clone()
        })
        .collect()
}

struct MemoryProbe {
    memory_id: String,
    observation_id: String,
    created_before: String,
}

fn recall_items(result: &Value) -> &[Value] {
    result
        .as_array()
        .or_else(|| result.get("results").and_then(Value::as_array))
        .expect("recall array or result envelope")
}

async fn probe(
    socket: &Path,
    config_id: &str,
    actor: Option<&str>,
    visible: &[&str],
    namespace: Option<&str>,
    memory: &MemoryProbe,
) -> [usize; 3] {
    let mut operations = json!([
        {"tool": "memory.recall", "args": {
            "query": "settlementprobe", "tags": ["memory-namespace-probe"],
            "created_before": memory.created_before, "fusion_strategy": "keyword_only",
            "min_score": 0.0, "include_breakdown": true
        }},
        {"tool": "list", "args": {"kind": "memory", "tags": ["memory-namespace-probe"]}},
        {"tool": "list", "args": {
            "kind": "edge", "source_id": memory.memory_id, "relations": ["annotates"]
        }}
    ]);
    if let Some(namespace) = namespace {
        for operation in operations.as_array_mut().expect("operation array") {
            operation["args"]["namespace"] = json!(namespace);
        }
    }
    let results = request(socket, config_id, actor, visible, operations.to_string()).await;
    assert_eq!(results.len(), 3);
    let recall = recall_items(&results[0]);
    let notes = results[1]["items"].as_array().expect("memory list");
    let edges = results[2]["items"].as_array().expect("edge list");
    [
        recall
            .iter()
            .filter(|hit| hit["full_id"] == memory.memory_id)
            .count(),
        notes
            .iter()
            .filter(|note| note["id"] == memory.memory_id)
            .count(),
        edges
            .iter()
            .filter(|edge| {
                edge["source_id"] == memory.memory_id
                    && edge["target_id"] == memory.observation_id
                    && edge["relation"] == "annotates"
            })
            .count(),
    ]
}

async fn exercise(socket: &Path, config_id: &str) -> [bool; 3] {
    drop(connect_when_ready(socket).await);
    let chain = request(
        socket,
        config_id,
        Some(ACTOR),
        &[],
        r#"create(kind="observation", content="settlementprobe source") | memory.remember(content="settlementprobe durable finding", memory_type="episodic", salience=1.0, decay_factor=0.0, tags=["memory-namespace-probe"], source_id=$prev.id)"#.into(),
    )
    .await;
    assert_eq!(chain.len(), 2);
    let created_at = chrono::DateTime::parse_from_rfc3339(
        chain[1]["created_at"]
            .as_str()
            .expect("memory creation time"),
    )
    .expect("RFC3339 creation time");
    let before = created_at + chrono::Duration::seconds(1);
    assert!(
        before > created_at,
        "created_before must be exclusive and later"
    );
    let memory = MemoryProbe {
        memory_id: chain[1]["id"].as_str().expect("memory UUID").into(),
        observation_id: chain[0]["id"].as_str().expect("observation UUID").into(),
        created_before: before.to_rfc3339(),
    };
    uuid::Uuid::parse_str(&memory.memory_id).expect("full memory UUID");
    uuid::Uuid::parse_str(&memory.observation_id).expect("full observation UUID");

    let positive = probe(socket, config_id, Some(ACTOR), &[ACTOR], None, &memory).await;
    eprintln!("explicit actor visibility: {positive:?}; frame.ok and every inner ok verified");
    assert_eq!(
        positive,
        [1, 1, 1],
        "explicit visibility must find all three records"
    );

    let default = probe(socket, config_id, Some(ACTOR), &[], None, &memory).await;
    let omissions = default.map(|count| count == 0);
    eprintln!(
        "empty-visibility actor omissions: recall={}, memory={}, annotates={}; frame.ok and every inner ok verified",
        omissions[0], omissions[1], omissions[2]
    );
    assert!(
        default.iter().all(|count| *count <= 1),
        "duplicate default results"
    );
    assert_eq!(
        probe(socket, config_id, None, &[], None, &memory).await,
        [0, 0, 0],
        "anonymous default reads must not include another actor's namespace"
    );
    assert_eq!(
        probe(
            socket,
            config_id,
            Some("test:other-reader"),
            &[],
            None,
            &memory
        )
        .await,
        [0, 0, 0],
        "a different actor must not inherit the writer's default visibility"
    );
    assert_eq!(
        probe(socket, config_id, Some(ACTOR), &[], Some(ACTOR), &memory).await,
        [1, 1, 1],
        "an explicit actor namespace must remain usable"
    );
    assert_eq!(
        probe(
            socket,
            config_id,
            Some(ACTOR),
            &[ACTOR],
            Some("local"),
            &memory
        )
        .await,
        [0, 0, 0],
        "explicit local must not widen, even with actor visibility supplied"
    );
    assert_eq!(
        probe(
            socket,
            config_id,
            Some(ACTOR),
            &[ACTOR, ACTOR],
            None,
            &memory
        )
        .await,
        [1, 1, 1],
        "repeated visibility entries must not duplicate results"
    );

    let writes = request(
        socket,
        config_id,
        Some(ACTOR),
        &[],
        json!([
            {"tool": "memory.remember", "args": {"content": "semantic location control", "memory_type": "semantic"}},
            {"tool": "memory.remember", "args": {"content": "explicit location control", "memory_type": "episodic", "namespace": "local"}}
        ]).to_string(),
    ).await;
    let anonymous = request(
        socket, config_id, None, &[],
        r#"memory.remember(content="anonymousprobe location control", memory_type="episodic", salience=1.0, decay_factor=0.0, tags=["anonymous-namespace-probe"])"#.into(),
    ).await;
    let destinations = request(
        socket,
        config_id,
        Some(ACTOR),
        &[],
        json!([
            {"tool": "list", "args": {"kind": "observation", "namespace": "local"}},
            {"tool": "list", "args": {"kind": "observation", "namespace": ACTOR}},
            {"tool": "list", "args": {"kind": "memory", "namespace": "local"}},
            {"tool": "list", "args": {"kind": "memory", "namespace": ACTOR}}
        ])
        .to_string(),
    )
    .await;
    let ids = |index: usize| -> Vec<&Value> {
        destinations[index]["items"]
            .as_array()
            .expect("destination list")
            .iter()
            .map(|row| &row["id"])
            .collect()
    };
    assert_eq!(
        ids(0),
        vec![&chain[0]["id"]],
        "ordinary writes remain local"
    );
    assert!(
        ids(1).is_empty(),
        "actor visibility must not relocate observations"
    );
    let local_memories = ids(2);
    assert_eq!(local_memories.len(), 3);
    for id in [&writes[0]["id"], &writes[1]["id"], &anonymous[0]["id"]] {
        assert!(
            local_memories.contains(&id),
            "semantic, explicit-local and anonymous writes remain local"
        );
    }
    assert_eq!(
        ids(3),
        vec![&chain[1]["id"]],
        "episodic default writes use the actor namespace"
    );
    let anonymous_recall = request(
        socket, config_id, None, &[],
        r#"memory.recall(query="anonymousprobe", tags=["anonymous-namespace-probe"], fusion_strategy="keyword_only", min_score=0.0, include_breakdown=true)"#.into(),
    ).await;
    let recalled_anonymous = recall_items(&anonymous_recall[0]);
    assert_eq!(recalled_anonymous.len(), 1);
    assert_eq!(recalled_anonymous[0]["full_id"], anonymous[0]["id"]);
    eprintln!("anonymous, other actor, exact namespace, duplicate visibility and write destination controls passed");
    omissions
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn bound_actor_memory_namespace_round_trip() {
    let _environment = RecoveryTestGuard::new();
    clear_daemon_env();
    let directory = tempfile::tempdir().expect("isolated daemon storage");
    let socket = directory.path().join("daemon.sock");
    std::env::set_var("KHIVE_SOCKET", &socket);
    std::env::set_var("KHIVE_PID", directory.path().join("daemon.pid"));
    std::env::set_var("KHIVE_LOCK", directory.path().join("daemon.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        directory.path().join("recoverer.lock"),
    );
    let config = RuntimeConfig {
        db_path: Some(directory.path().join("memory.db")),
        packs: vec!["kg".into(), "memory".into()],
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    };
    let runtime = KhiveRuntime::new(config).expect("file-backed runtime without embedders");
    let server = KhiveMcpServer::new(runtime).expect("kg and memory server");
    let config_id = server.config_id().to_owned();
    let daemon = tokio::spawn(run_daemon(server));
    // Catch control failures so even the intended RED result joins the daemon before unwinding.
    let outcome = timeout(
        Duration::from_secs(30),
        std::panic::AssertUnwindSafe(exercise(&socket, &config_id)).catch_unwind(),
    )
    .await;
    daemon.abort();
    let stopped = timeout(Duration::from_secs(5), daemon)
        .await
        .expect("aborted daemon must join within five seconds");
    assert!(
        matches!(&stopped, Err(error) if error.is_cancelled()),
        "daemon exited unexpectedly: {stopped:?}"
    );
    let omissions = outcome
        .expect("bounded namespace scenario")
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert_eq!(
        omissions, [false; 3],
        "bound actor with empty wire visibility omitted [recall, memory, annotates]"
    );
}
