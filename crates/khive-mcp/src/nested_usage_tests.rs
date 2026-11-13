use super::KhiveMcpServer;
use crate::tools::request::RequestParams;
use async_trait::async_trait;
use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::pack::{AuditSuccessRowsMode, DispatchHook, VerbRegistryBuilder};
use khive_runtime::usage::{self, UsageUnit};
use khive_runtime::{EmbedderProvider, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};
use khive_storage::{Event, EventFilter, EventView, PageRequest};
use khive_types::{EventKind, EventOutcome};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const MODEL: &str = "all-minilm-l6-v2";
const DIM: usize = 384;
const PROFILE: &str = "compose-usage-profile";
const QUERY: &str =
    "kv cache paging decode attention retrieval augmented generation dense sparse benchmark corpus";
const CONTENT: &str = "kv cache paging decode attention retrieval augmented generation dense sparse benchmark corpus latency gradient descent transformer vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";

struct CountingEmbedder(Arc<AtomicU64>);

#[async_trait]
impl EmbeddingService for CountingEmbedder {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        // The default query/passage methods delegate here once per batch.
        self.0.fetch_add(texts.len() as u64, Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|_| {
                let mut vector = vec![0.0; DIM];
                vector[0] = 1.0;
                vector
            })
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "compose-usage-counting-embedder"
    }
}

#[async_trait]
impl EmbedderProvider for CountingEmbedder {
    fn name(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        DIM
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(Self(self.0.clone())))
    }
}

#[derive(Default)]
struct NestedSnapshots(Mutex<Vec<(String, Value)>>);

#[async_trait]
impl DispatchHook for NestedSnapshots {
    async fn on_dispatch(&self, view: &EventView) {
        if !matches!(view.event.verb.as_str(), "brain.resolve" | "brain.profile") {
            return;
        }
        let Some(context) = usage::current() else {
            return;
        };
        self.0.lock().unwrap().push((
            view.event.verb.clone(),
            context.shipping_snapshot().expect("measured brain lookup"),
        ));
        usage::count(UsageUnit::GraphHops, 777);
        assert_eq!(context.snapshot()["graph_hops"], 777);
    }
}

#[derive(Clone, Copy, Debug)]
enum ProfileMode {
    AbsentBrain,
    Unbound,
    Explicit,
    Bound,
}

impl ProfileMode {
    fn nested_verbs(self) -> &'static [&'static str] {
        match self {
            Self::AbsentBrain => &[],
            Self::Unbound => &["brain.resolve"],
            Self::Explicit => &["brain.profile"],
            Self::Bound => &["brain.resolve", "brain.profile"],
        }
    }
}

struct Fixture {
    server: KhiveMcpServer,
    runtime: KhiveRuntime,
    served_texts: Arc<AtomicU64>,
    nested: Arc<NestedSnapshots>,
    domain_id: String,
}

async fn fixture(mode: ProfileMode) -> Fixture {
    let has_brain = !matches!(mode, ProfileMode::AbsentBrain);
    let runtime = KhiveRuntime::new(RuntimeConfig {
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        brain_profile: matches!(mode, ProfileMode::Explicit).then(|| PROFILE.to_string()),
        packs: if has_brain {
            vec!["kg".into(), "brain".into(), "knowledge".into()]
        } else {
            vec!["kg".into(), "knowledge".into()]
        },
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let served_texts = Arc::new(AtomicU64::new(0));
    runtime
        .try_register_embedder(CountingEmbedder(served_texts.clone()))
        .expect("register counting provider before any embedding");
    let nested = Arc::new(NestedSnapshots::default());
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    if has_brain {
        builder.register(BrainPack::new(runtime.clone()));
    }
    builder.register(KnowledgePack::new(runtime.clone()));
    builder.with_event_store(runtime.events(&token).unwrap());
    builder.with_audit_success_rows_mode(AuditSuccessRowsMode::All);
    builder.with_dispatch_hook(nested.clone());
    let registry = builder.build().unwrap();
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    if matches!(mode, ProfileMode::Explicit | ProfileMode::Bound) {
        registry
            .dispatch(
                "brain.create_profile",
                json!({"name":PROFILE,"consumer_kind":"knowledge_compose"}),
            )
            .await
            .unwrap();
        registry
            .dispatch("brain.activate", json!({"profile_id":PROFILE}))
            .await
            .unwrap();
        if matches!(mode, ProfileMode::Bound) {
            registry
                .dispatch(
                    "brain.bind",
                    json!({"profile_id":PROFILE,"namespace":"local","consumer_kind":"knowledge_compose"}),
                )
                .await
                .unwrap();
        }
    }
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms":[{"slug":"usage-atom","name":"Usage atom","content":CONTENT,"finalized":true}]}),
        )
        .await
        .unwrap();
    registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains":[{"slug":"usage-domain","name":"Usage domain","description":CONTENT,"members":["usage-atom"]}]}),
        )
        .await
        .unwrap();
    let domain = registry
        .dispatch("knowledge.get", json!({"id":"usage-domain"}))
        .await
        .unwrap();
    Fixture {
        server: KhiveMcpServer::from_registry(registry),
        runtime,
        served_texts,
        nested,
        domain_id: domain["id"].as_str().unwrap().to_owned(),
    }
}

async fn audit_rows(runtime: &KhiveRuntime) -> Vec<Event> {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let page = runtime
        .events(&token)
        .unwrap()
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert!(page.items.len() < 100, "fixture must fit one event page");
    page.items
        .into_iter()
        .filter(|event| event.kind == EventKind::Audit)
        .collect()
}

async fn assert_compose_usage(fixture: &Fixture, mode: ProfileMode, auto: bool) {
    let before: HashSet<_> = audit_rows(&fixture.runtime)
        .await
        .into_iter()
        .map(|event| event.id)
        .collect();
    fixture.served_texts.store(0, Ordering::SeqCst);
    fixture.nested.0.lock().unwrap().clear();
    let mut args = json!({"query":QUERY,"max_tokens":1500});
    if !auto {
        args["domain_ids"] = json!([fixture.domain_id.as_str()]);
    }
    let response = fixture
        .server
        .dispatch_request_local(RequestParams {
            ops: json!([{"tool":"knowledge.compose","args":args}]).to_string(),
            request_id: Some(6251),
            presentation: Some("verbose".into()),
            format: Some("json".into()),
            ..RequestParams::default()
        })
        .await
        .unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    let entry = &response["results"][0];
    assert_eq!(entry["ok"], true, "{mode:?} auto={auto}: {response}");
    let data = &entry["result"]["data"];
    assert!(
        data["atoms"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "{response}"
    );
    assert!(
        data["domains"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "{response}"
    );
    let served_texts = fixture.served_texts.load(Ordering::SeqCst);
    assert!(served_texts > 0, "compose must actually embed: {response}");
    assert_eq!(
        entry["usage"]["embed_calls"], served_texts,
        "{mode:?} auto={auto}: {response}"
    );
    assert!(
        entry["usage"].get("graph_hops").is_none(),
        "late nested hook work must be excluded"
    );

    // Nested identities deliberately omit the ingress request_id.
    let rows: Vec<_> = audit_rows(&fixture.runtime)
        .await
        .into_iter()
        .filter(|event| !before.contains(&event.id))
        .collect();
    assert_eq!(rows.len(), mode.nested_verbs().len() + 1, "{rows:?}");
    let outer = rows
        .iter()
        .find(|event| event.verb == "knowledge.compose")
        .unwrap();
    assert_eq!(outer.outcome, EventOutcome::Success);
    assert_eq!(outer.payload["resource"]["request_id"], 6251);
    assert_eq!(outer.payload["resource"]["units"], entry["usage"]);
    let snapshots = fixture.nested.0.lock().unwrap();
    assert_eq!(snapshots.len(), mode.nested_verbs().len());
    for verb in mode.nested_verbs() {
        let row = rows.iter().find(|event| event.verb == *verb).unwrap();
        let snapshot = &snapshots
            .iter()
            .find(|(name, _)| name.as_str() == *verb)
            .unwrap()
            .1;
        assert_eq!(row.outcome, EventOutcome::Success);
        assert!(row.payload["resource"].get("request_id").is_none());
        assert_eq!(&row.payload["resource"]["units"], snapshot);
        assert!(snapshot.is_object());
        assert!(
            snapshot.get("embed_calls").is_none(),
            "nested lookup must not inherit compose embeddings"
        );
        assert!(
            snapshot.get("graph_hops").is_none(),
            "nested audit must precede its late hook"
        );
    }
}

#[test]
#[serial_test::serial(config_ledger)]
fn compose_usage_survives_nested_profile_audit_freezes() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for mode in [
                ProfileMode::AbsentBrain,
                ProfileMode::Unbound,
                ProfileMode::Explicit,
                ProfileMode::Bound,
            ] {
                let fixture = fixture(mode).await;
                assert_compose_usage(&fixture, mode, true).await;
                assert_compose_usage(&fixture, mode, false).await;
                fixture.server.shutdown_audit_batch().await.unwrap();
            }
        });
}
