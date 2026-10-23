pub mod mutations;
pub mod reads;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use criterion::black_box;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
    WalCeilingSource,
};
use khive_storage::types::Edge;
use khive_storage::BatchWriteSummary;
use khive_types::EdgeRelation;
use serde_json::{json, Value};
use uuid::Uuid;

pub const ACTOR: &str = "benchmark:kg";
pub const MICROS: i64 = 1_700_000_000_000_000;
pub const REPORT_SAMPLES: usize = 21;

pub fn reports_enabled() -> bool {
    let mut bench = false;
    for arg in std::env::args_os().skip(1).take_while(|arg| arg != "--") {
        if arg == "--test"
            || arg == "--profile-time"
            || arg
                .to_str()
                .is_some_and(|arg| arg.starts_with("--profile-time="))
        {
            return false;
        }
        bench |= arg == "--bench";
    }
    bench
}

pub fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .expect("bounded executor")
}

pub struct Fixture {
    pub runtime: KhiveRuntime,
    pub registry: VerbRegistry,
    pub token: NamespaceToken,
}

impl Fixture {
    pub fn new() -> Self {
        let mut config = RuntimeConfig::no_embeddings();
        config.db_path = None;
        config.actor_id = Some(ACTOR.into());
        config.default_namespace = Namespace::local();
        config.visible_namespaces.clear();
        config.allowed_outbound_namespaces.clear();
        config.packs = vec!["kg".into()];
        config.credentials.clear();
        config.visibility_receipts = None;
        config.mounts.clear();
        config.events_split = None;
        config.brain_profile = None;
        config.brain = Default::default();
        config.wal_ceiling_bytes = 0;
        config.wal_ceiling_configured_bytes = 0;
        config.wal_ceiling_source = WalCeilingSource::Default;
        config.wal_ceiling_env_raw = None;
        config.disk_guard_environment = Default::default();
        config.disk_guard_config = None;
        config.volume_lock_dir = None;
        let runtime = KhiveRuntime::new(config).expect("isolated memory runtime");
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(!runtime.backend().pool().write_queue_active());
        assert!(runtime.registered_embedding_model_names().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(ACTOR.into()));
        builder.with_default_namespace("local");
        builder.register(KgPack::new(runtime.clone()));
        // Match kg_bench's optional-audit-sink composition. In particular, a
        // read must not grow the 10K-event fixture it is intended to measure.
        let registry = builder.build().expect("KG registry");
        assert!(registry.audit_batch_handle().is_none());
        runtime.install_edge_rules(registry.all_edge_rules());
        let token = runtime.authorize(Namespace::local()).expect("local token");
        Self {
            runtime,
            registry,
            token,
        }
    }

    pub async fn seed_entities(&self, count: usize, merge_names: bool) -> Vec<Uuid> {
        let names: Vec<_> = (0..count)
            .map(|i| {
                if merge_names && i < 2 {
                    "Generated merge concept".into()
                } else {
                    format!("Generated concept {i:04}")
                }
            })
            .collect();
        let mut ids = Vec::with_capacity(count);
        for chunk in names.chunks(1000) {
            let result = self
                .registry
                .dispatch("create", bulk_params(chunk))
                .await
                .expect("seed entities through bulk create");
            ids.extend(self.check_bulk(&result, chunk).await);
        }
        assert_eq!(ids.iter().copied().collect::<BTreeSet<_>>().len(), count);
        ids
    }

    pub async fn check_bulk(&self, result: &Value, names: &[String]) -> Vec<Uuid> {
        assert_eq!(result["attempted"], names.len());
        assert_eq!(result["created"], names.len());
        assert_eq!(result["skipped"], 0);
        assert_eq!(result["failed"], 0);
        let rows = result["results"].as_array().expect("bulk results");
        assert_eq!(rows.len(), names.len());
        let mut ids = Vec::with_capacity(names.len());
        for (index, (row, name)) in rows.iter().zip(names).enumerate() {
            assert_eq!(row["index"], index);
            assert_eq!(row["ok"], true);
            assert_eq!(row["result"]["created"], true);
            let id = Uuid::parse_str(row["result"]["id"].as_str().expect("full id")).unwrap();
            let entity = self
                .runtime
                .get_entity(&self.token, id)
                .await
                .expect("actual entity");
            assert_eq!(&entity.name, name);
            assert_eq!(entity.kind, "concept");
            assert_eq!(entity.version, 1);
            assert!(entity.deleted_at.is_none());
            ids.push(id);
        }
        assert_eq!(
            ids.iter().copied().collect::<BTreeSet<_>>().len(),
            names.len()
        );
        ids
    }

    pub async fn seed_edges(&self, pairs: &[(Uuid, Uuid)]) -> Vec<Edge> {
        let timestamp = chrono::DateTime::from_timestamp_micros(MICROS).unwrap();
        let edges: Vec<_> = pairs
            .iter()
            .enumerate()
            .map(|(i, &(source_id, target_id))| Edge {
                id: Uuid::from_u128((5660_u128 << 64) | (i as u128 + 1)).into(),
                namespace: "local".into(),
                source_id,
                target_id,
                relation: EdgeRelation::Contains,
                weight: 1.0,
                created_at: timestamp,
                updated_at: timestamp,
                deleted_at: None,
                metadata: None,
                target_backend: None,
            })
            .collect();
        let store = self.runtime.graph(&self.token).expect("graph store");
        complete(
            store.upsert_edges(edges.clone()).await.expect("seed edges"),
            edges.len(),
        );
        let actual = store
            .get_edges(&edges.iter().map(|e| e.id).collect::<Vec<_>>())
            .await
            .expect("read seeded edges");
        assert_eq!(actual.len(), edges.len());
        assert_eq!(
            actual
                .iter()
                .map(|e| (Uuid::from(e.id), e.source_id, e.target_id))
                .collect::<BTreeSet<_>>(),
            edges
                .iter()
                .map(|e| (Uuid::from(e.id), e.source_id, e.target_id))
                .collect()
        );
        edges
    }

    pub fn close(self) {
        assert!(self.registry.audit_batch_handle().is_none());
        assert!(!self.runtime.backend().pool().write_queue_active());
        let Self {
            runtime,
            registry,
            token,
        } = self;
        drop(token);
        drop(registry);
        drop(runtime);
    }
}

pub fn bulk_params(names: &[String]) -> Value {
    json!({"atomic":true, "items":names.iter().map(|name|
        json!({"kind":"concept", "name":name})).collect::<Vec<_>>()})
}

pub fn complete(summary: BatchWriteSummary, count: usize) {
    assert_eq!(summary.attempted, count as u64, "{summary:?}");
    assert_eq!(summary.affected, count as u64, "{summary:?}");
    assert_eq!(summary.failed, 0, "{summary:?}");
}

pub fn measure(
    rt: &tokio::runtime::Runtime,
    fixture: &Fixture,
    verb: &str,
    params: Value,
) -> (Duration, Value) {
    let start = Instant::now();
    let result = rt.block_on(fixture.registry.dispatch(verb, black_box(params)));
    let elapsed = start.elapsed();
    (elapsed, result.expect("successful complete dispatch"))
}

pub fn report(name: &str, samples: &[Duration], work_per_call: Option<usize>) {
    assert!(!samples.is_empty());
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[(sorted.len() * 50).div_ceil(100) - 1];
    let p95 = sorted[(sorted.len() * 95).div_ceil(100) - 1];
    println!(
        "{}",
        json!({
            "scenario":name, "actual_call_samples":sorted.len(),
            "p50_us":p50.as_secs_f64()*1e6, "p95_us":p95.as_secs_f64()*1e6,
            "work_per_call":work_per_call,
            "work_per_second_at_median":work_per_call.map(|n| n as f64 / p50.as_secs_f64()),
            "percentile_method":"nearest rank of individual call durations",
        })
    );
}
