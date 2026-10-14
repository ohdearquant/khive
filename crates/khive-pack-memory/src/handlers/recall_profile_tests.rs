//! Capture the real stderr profiler in an isolated process: its environment flag
//! is cached, and parent tests must not mutate it or redirect global stderr.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_types::{HandlerDef, Pack, VerbCategory, Visibility};
use serde_json::{json, Value};
use tokio::sync::{Barrier, Notify};

use crate::test_support::{with_receipt_credentials, HashVecProvider};
use crate::MemoryPack;

const CHILD: &str = "KHIVE_RECALL_PROFILE_TEST_CHILD";
const CONTENT: &str = "profile fixture searchable memory";

// Same null-snapshot profile seam as the integration suite, with a rendezvous
// before candidate gathering. Both recalls initialize their IDs before either
// text/vector arm runs, deterministically exposing shared thread-local state.
struct RendezvousBrainPack {
    barrier: Option<Arc<Barrier>>,
    entered: Arc<Notify>,
}

impl Pack for RendezvousBrainPack {
    const NAME: &'static str = "brain";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "brain.profile",
        description: "test profile rendezvous",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for RendezvousBrainPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        assert_eq!(verb, "brain.profile");
        self.entered.notify_one();
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        Ok(json!({
            "id": params["profile_id"],
            "state_snapshot": null,
            "lifecycle": "active"
        }))
    }
}

async fn fixture(
    barrier: Option<Arc<Barrier>>,
) -> (MemoryPack, NamespaceToken, VerbRegistry, Arc<Notify>) {
    let rt = with_receipt_credentials(KhiveRuntime::memory().unwrap());
    for model in ["profile-model-a", "profile-model-b"] {
        rt.register_embedder(HashVecProvider {
            model_name: model.into(),
            dims: 8,
        });
    }
    let pack = MemoryPack::new(rt.clone());
    let token = rt.authorize(Namespace::local()).unwrap();
    let entered = Arc::new(Notify::new());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt));
    builder.register(RendezvousBrainPack {
        barrier,
        entered: entered.clone(),
    });
    let registry = builder.build().unwrap();
    registry
        .dispatch(
            "memory.remember",
            json!({"content": CONTENT, "memory_type": "semantic", "salience": 0.9}),
        )
        .await
        .unwrap();
    (pack, token, registry, entered)
}

fn params() -> Value {
    json!({"query": CONTENT, "profile_id": "profile-fixture", "score_floor": 0.0})
}

async fn exercise(mode: &str) {
    if mode == "concurrent" || mode == "disabled" {
        let (pack, token, registry, _) = fixture(Some(Arc::new(Barrier::new(2)))).await;
        let (left, right) = tokio::join!(
            pack.handle_recall(&token, params(), &registry, None),
            pack.handle_recall(&token, params(), &registry, None),
        );
        for response in [left.unwrap(), right.unwrap()] {
            assert!(!response.as_array().unwrap().is_empty());
        }
        return;
    }

    assert_eq!(mode, "exits");
    let (pack, token, registry, _) = fixture(None).await;
    let mut empty = params();
    empty["query"] = json!("unmatchedquasarfixture");
    empty["fusion_strategy"] = json!("keyword_only");
    assert_eq!(
        pack.handle_recall(&token, empty, &registry, None)
            .await
            .unwrap(),
        json!([])
    );

    let mut verbose = params();
    verbose["include_breakdown"] = json!(true);
    let response = pack
        .handle_recall(&token, verbose, &registry, None)
        .await
        .unwrap();
    assert!(!response["results"].as_array().unwrap().is_empty());
    assert_eq!(
        response["candidates"]["vector_candidates_per_model"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "exercise the verbose multi-model early return"
    );

    // Deserialization fails before the original profiler initialization point.
    assert!(pack
        .handle_recall(&token, json!({"query": 7}), &registry, None)
        .await
        .is_err());
    let mut invalid = params();
    invalid["query"] = json!("");
    assert!(pack
        .handle_recall(&token, invalid, &registry, None)
        .await
        .is_err());

    let (pack, token, registry, entered) = fixture(Some(Arc::new(Barrier::new(2)))).await;
    let mut recall = Box::pin(pack.handle_recall(&token, params(), &registry, None));
    tokio::select! {
        _ = entered.notified() => {},
        result = &mut recall => panic!("profile must remain held: {result:?}"),
    }
    drop(recall);
}

#[test]
fn profile_fixture_child() {
    let Ok(mode) = std::env::var(CHILD) else {
        return;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise(&mode));
}

fn run_child(mode: &str, enabled: bool) -> Vec<Value> {
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("profile-output.txt");
    let output = std::fs::File::create(&output_path).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "handlers::recall::profile_tests::profile_fixture_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, mode)
        .env_remove("KHIVE_RECALL_PROFILE")
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output));
    if enabled {
        command.env("KHIVE_RECALL_PROFILE", "1");
    }
    let mut child = command.spawn().unwrap();
    let watchdog = Instant::now() + Duration::from_secs(45);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= watchdog {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("profiler fixture watchdog expired; no semantic outcome");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(output_path).unwrap();
    assert!(status.success(), "profiler child failed:\n{output}");
    assert!(output.contains("1 passed; 0 failed"), "{output}");
    output
        .lines()
        .filter_map(|line| {
            // libtest can prefix the first stderr line with the test name.
            let start = line.find("{\"c\":")?;
            Some(serde_json::from_str(&line[start..]).expect("valid profiler JSON"))
        })
        .collect()
}

fn stages_by_call(records: &[Value]) -> BTreeMap<u64, Vec<&str>> {
    let mut calls = BTreeMap::new();
    for record in records {
        assert!(record["us"].is_u64());
        calls
            .entry(record["c"].as_u64().unwrap())
            .or_insert_with(Vec::new)
            .push(record["s"].as_str().unwrap());
    }
    calls
}

#[test]
fn concurrent_recall_stages_keep_their_own_call_id() {
    let records = run_child("concurrent", true);
    let calls = stages_by_call(&records);
    assert_eq!(calls.keys().copied().collect::<Vec<_>>(), [0, 1]);
    for (call_id, stages) in &calls {
        for required in [
            "setup",
            "profile_resolve",
            "fts",
            "embed",
            "ann",
            "candidates",
            "serialize",
            "total",
        ] {
            assert_eq!(
                stages.iter().filter(|&&stage| stage == required).count(),
                1,
                "missing, duplicated or misattributed {required}: {calls:?}"
            );
        }
        let vector_record = records
            .iter()
            .find(|record| record["c"].as_u64() == Some(*call_id) && record["s"] == "ann")
            .expect("the vector stage was counted above");
        assert!(matches!(
            vector_record["route"].as_str(),
            Some("ann" | "sqlite_vec")
        ));
        assert_eq!(stages.last(), Some(&"total"));
    }
}

#[test]
fn early_returns_errors_and_cancellation_emit_one_total() {
    let records = run_child("exits", true);
    let calls = stages_by_call(&records);
    assert_eq!(calls.keys().copied().collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
    for stages in calls.values() {
        assert_eq!(stages.iter().filter(|&&s| s == "total").count(), 1);
        assert_eq!(stages.last(), Some(&"total"));
    }
    assert!(calls[&0].contains(&"fts"));
    assert!(!calls[&0].contains(&"scoring"));
    assert!(calls[&1].contains(&"scoring"));
    assert!(!calls[&1].contains(&"serialize"));
    assert_eq!(calls[&2], ["total"]);
    assert_eq!(calls[&3], ["total"]);
    assert_eq!(calls[&4], ["setup", "total"]);
}

#[test]
fn disabled_profiler_emits_no_stage_records() {
    assert!(run_child("disabled", false).is_empty());
}
