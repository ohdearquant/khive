use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::KnowledgePack;
use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{
    AllowAllGate, BackendId, EmbedderProvider, KhiveRuntime, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_types::Namespace;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;

const MODEL_KEY: &str = "all-minilm-l6-v2";
const DIM: usize = 384;
type EmbedCalls = Arc<Mutex<Vec<Vec<String>>>>;

struct FixedEmbedService {
    calls: EmbedCalls,
}

#[async_trait]
impl EmbeddingService for FixedEmbedService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        self.calls
            .lock()
            .expect("embedding calls")
            .push(texts.to_vec());
        Ok(texts
            .iter()
            .map(|_| {
                let mut vector = vec![0.0; DIM];
                vector[0] = 1.0;
                vector
            })
            .collect())
    }

    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        self.embed(texts, model).await
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "fixed-embed-service"
    }
}

struct FixedEmbedProvider {
    calls: EmbedCalls,
}

#[async_trait]
impl EmbedderProvider for FixedEmbedProvider {
    fn name(&self) -> &str {
        MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        DIM
    }

    async fn build(
        &self,
    ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(FixedEmbedService {
            calls: self.calls.clone(),
        }))
    }
}

fn runtime() -> (KhiveRuntime, Arc<VerbRegistry>, EmbedCalls) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let rt = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("runtime");
    rt.register_embedder(FixedEmbedProvider {
        calls: calls.clone(),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    (rt, Arc::new(registry), calls)
}

type AfterCapture = Pin<Box<dyn Future<Output = ()> + Send>>;

tokio::task_local! {
    static AFTER_CAPTURE: RefCell<Option<AfterCapture>>;
}

pub(super) async fn after_capture() {
    let action = AFTER_CAPTURE
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten();
    if let Some(action) = action {
        action.await;
    }
}

fn content(marker: &str) -> String {
    format!(
        "{marker} this private indexing fixture contains enough ordinary words to exercise \
         the accepted public atom write contract without external data or network access"
    )
}

fn atom(slug: &str, name: &str, marker: &str) -> serde_json::Value {
    json!({"slug": slug, "name": name, "content": content(marker)})
}

async fn seed_atom(registry: &VerbRegistry, slug: &str, name: &str, marker: &str) {
    let result = registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [atom(slug, name, marker)]}),
        )
        .await
        .expect("seed catalogue atom");
    assert_eq!(
        result["created"].as_u64(),
        Some(1),
        "valid new atom fixture"
    );
    let got = registry
        .dispatch("knowledge.get", json!({"id": slug}))
        .await
        .expect("read seeded atom");
    assert!(got["id"].as_str().is_some(), "seeded atom has an ID");
}

fn assert_counts(result: &serde_json::Value, total: u64, indexed: u64, skipped: u64) {
    assert_eq!(
        result["total"].as_u64(),
        Some(total),
        "captured catalogue total"
    );
    assert_eq!(
        result["indexed"].as_u64(),
        Some(indexed),
        "persisted catalogue vectors"
    );
    assert_eq!(
        result["skipped"].as_u64(),
        Some(skipped),
        "skipped captured atoms"
    );
    assert_eq!(result["failed"].as_u64(), Some(0));
}

#[tokio::test]
async fn catalogue_total_preserves_domain_mirror_population() {
    let (_runtime, registry, calls) = runtime();
    seed_atom(
        &registry,
        "catalogue-alpha",
        "Catalogue alpha",
        "cataloguebaselinealpha",
    )
    .await;
    seed_atom(
        &registry,
        "catalogue-beta",
        "Catalogue beta",
        "cataloguebaselinebeta",
    )
    .await;
    registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [{
                "slug": "catalogue-domain", "name": "Catalogue domain",
                "description": content("cataloguebaselinedomain"),
            }]}),
        )
        .await
        .expect("seed domain mirror");
    let domain = registry
        .dispatch("knowledge.get", json!({"id": "catalogue-domain"}))
        .await
        .expect("read domain mirror");
    assert!(domain["id"].as_str().is_some(), "domain mirror has an ID");
    calls.lock().expect("embedding calls").clear();

    let result = registry
        .dispatch("knowledge.index", json!({"batch_size": 2}))
        .await
        .expect("index unchanged catalogue");
    assert_counts(&result, 3, 3, 0);
    let calls = calls.lock().expect("embedding calls");
    assert_eq!(calls.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 1]);
    for marker in [
        "cataloguebaselinealpha",
        "cataloguebaselinebeta",
        "cataloguebaselinedomain",
    ] {
        assert_eq!(
            calls
                .iter()
                .flatten()
                .filter(|text| text.contains(marker))
                .count(),
            1,
            "each selected atom, including the domain mirror, is embedded once: {marker}"
        );
    }
}

#[tokio::test]
async fn catalogue_deleted_between_passes_is_skipped() {
    let (_runtime, registry, calls) = runtime();
    seed_atom(
        &registry,
        "catalogue-gone",
        "Catalogue gone",
        "cataloguedeletedmarker",
    )
    .await;
    seed_atom(
        &registry,
        "catalogue-kept",
        "Catalogue kept",
        "cataloguekeptmarker",
    )
    .await;
    calls.lock().expect("embedding calls").clear();
    let edit_registry = registry.clone();
    let edit_calls = calls.clone();
    let action: AfterCapture = Box::pin(async move {
        let deleted = edit_registry
            .dispatch("knowledge.delete_atoms", json!({"ids": ["catalogue-gone"]}))
            .await
            .expect("delete captured atom");
        assert_eq!(
            deleted["deleted"].as_u64(),
            Some(1),
            "valid between-pass delete"
        );
        edit_calls.lock().expect("embedding calls").clear();
    });
    let result = AFTER_CAPTURE
        .scope(
            RefCell::new(Some(action)),
            registry.dispatch("knowledge.index", json!({"batch_size": 1})),
        )
        .await
        .expect("index after captured atom deletion");
    assert_counts(&result, 2, 1, 1);
    let calls = calls.lock().expect("embedding calls");
    let texts: Vec<_> = calls.iter().flatten().collect();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("cataloguekeptmarker"));
    assert!(!texts[0].contains("cataloguedeletedmarker"));
}

#[tokio::test]
async fn catalogue_reload_uses_edits_and_excludes_new_ids() {
    let (_runtime, registry, calls) = runtime();
    seed_atom(
        &registry,
        "catalogue-edited",
        "Catalogue before",
        "cataloguebeforemarker",
    )
    .await;
    calls.lock().expect("embedding calls").clear();
    let edit_registry = registry.clone();
    let edit_calls = calls.clone();
    let action: AfterCapture = Box::pin(async move {
        let updated = edit_registry
            .dispatch(
                "knowledge.upsert_atoms",
                json!({"atoms": [atom("catalogue-edited", "Catalogue after", "catalogueaftermarker")]}),
            )
            .await
            .expect("edit captured atom");
        assert_eq!(updated["created"].as_u64(), Some(0));
        assert_eq!(
            updated["updated"].as_u64(),
            Some(1),
            "valid between-pass edit"
        );
        let got = edit_registry
            .dispatch("knowledge.get", json!({"id": "catalogue-edited"}))
            .await
            .expect("read edited atom");
        assert_eq!(got["name"], "Catalogue after");
        seed_atom(
            &edit_registry,
            "catalogue-new",
            "Catalogue new",
            "cataloguenewmarker",
        )
        .await;
        edit_calls.lock().expect("embedding calls").clear();
    });
    let result = AFTER_CAPTURE
        .scope(
            RefCell::new(Some(action)),
            registry.dispatch("knowledge.index", json!({"batch_size": 1})),
        )
        .await
        .expect("index late payload");
    assert_counts(&result, 1, 1, 0);
    {
        let calls = calls.lock().expect("embedding calls");
        let texts: Vec<_> = calls.iter().flatten().collect();
        assert_eq!(texts.len(), 1);
        assert!(
            texts[0].contains("catalogueaftermarker"),
            "embed edited payload"
        );
        assert!(!texts[0].contains("cataloguebeforemarker"));
        assert!(
            !texts[0].contains("cataloguenewmarker"),
            "new ID excluded from captured set"
        );
    }
    calls.lock().expect("embedding calls").clear();
    let next = registry
        .dispatch("knowledge.index", json!({"batch_size": 1}))
        .await
        .expect("next run includes newly created ID");
    assert_counts(&next, 2, 2, 0);
    let calls = calls.lock().expect("embedding calls");
    let texts: Vec<_> = calls.iter().flatten().collect();
    assert_eq!(texts.len(), 2);
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.contains("catalogueaftermarker"))
            .count(),
        1
    );
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.contains("cataloguenewmarker"))
            .count(),
        1
    );
}

#[tokio::test]
async fn catalogue_progress_reaches_total_when_all_captured_atoms_vanish() {
    let (runtime, registry, calls) = runtime();
    seed_atom(
        &registry,
        "catalogue-gone-a",
        "Catalogue gone A",
        "cataloguegoneamarker",
    )
    .await;
    seed_atom(
        &registry,
        "catalogue-gone-b",
        "Catalogue gone B",
        "cataloguegonebmarker",
    )
    .await;
    calls.lock().expect("embedding calls").clear();
    let edit_registry = registry.clone();
    let action: AfterCapture = Box::pin(async move {
        let deleted = edit_registry
            .dispatch(
                "knowledge.delete_atoms",
                json!({"ids": ["catalogue-gone-a", "catalogue-gone-b"]}),
            )
            .await
            .expect("delete every captured atom");
        assert_eq!(
            deleted["deleted"].as_u64(),
            Some(2),
            "valid all-skipped fixture"
        );
    });
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    let ann = super::vamana::new_shared();
    let progress = Mutex::new(Vec::new());
    let callback = |processed, total| {
        progress
            .lock()
            .expect("progress observations")
            .push((processed, total));
    };
    let result = AFTER_CAPTURE
        .scope(
            RefCell::new(Some(action)),
            super::KnowledgeHandlers::index(
                &runtime,
                &token,
                json!({"batch_size": 1}),
                &ann,
                Some(&callback),
            ),
        )
        .await
        .expect("index all-skipped catalogue with progress");
    assert_counts(&result, 2, 0, 2);
    assert!(calls.lock().expect("embedding calls").is_empty());
    let progress = progress.lock().expect("progress observations");
    assert_eq!(
        *progress,
        vec![(0, 2), (1, 2), (2, 2)],
        "processed skipped IDs reach total"
    );
    assert!(progress
        .iter()
        .all(|&(processed, total)| total == 2 && processed <= total));
    assert!(progress.windows(2).all(|pair| pair[0].0 <= pair[1].0));
}

#[tokio::test]
async fn catalogue_mixed_progress_counts_written_and_skipped_ids() {
    let (runtime, registry, calls) = runtime();
    seed_atom(
        &registry,
        "catalogue-mixed-gone",
        "Catalogue mixed gone",
        "cataloguemixedgonemarker",
    )
    .await;
    seed_atom(
        &registry,
        "catalogue-mixed-kept",
        "Catalogue mixed kept",
        "cataloguemixedkeptmarker",
    )
    .await;
    calls.lock().expect("embedding calls").clear();
    let edit_registry = registry.clone();
    let edit_calls = calls.clone();
    let action: AfterCapture = Box::pin(async move {
        let deleted = edit_registry
            .dispatch(
                "knowledge.delete_atoms",
                json!({"ids": ["catalogue-mixed-gone"]}),
            )
            .await
            .expect("delete one captured atom");
        assert_eq!(
            deleted["deleted"].as_u64(),
            Some(1),
            "valid mixed progress fixture"
        );
        edit_calls.lock().expect("embedding calls").clear();
    });
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    let ann = super::vamana::new_shared();
    let progress = Mutex::new(Vec::new());
    let callback = |processed, total| {
        progress
            .lock()
            .expect("progress observations")
            .push((processed, total));
    };
    let result = AFTER_CAPTURE
        .scope(
            RefCell::new(Some(action)),
            super::KnowledgeHandlers::index(
                &runtime,
                &token,
                json!({"batch_size": 2}),
                &ann,
                Some(&callback),
            ),
        )
        .await
        .expect("index mixed written and skipped catalogue");
    assert_counts(&result, 2, 1, 1);
    let calls = calls.lock().expect("embedding calls");
    let texts: Vec<_> = calls.iter().flatten().collect();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("cataloguemixedkeptmarker"));
    assert!(!texts[0].contains("cataloguemixedgonemarker"));
    let progress = progress.lock().expect("progress observations");
    assert_eq!(
        *progress,
        vec![(0, 2), (2, 2)],
        "written and skipped IDs both advance progress"
    );
    assert!(progress
        .iter()
        .all(|&(processed, total)| total == 2 && processed <= total));
    assert!(progress.windows(2).all(|pair| pair[0].0 <= pair[1].0));
}

#[tokio::test]
async fn catalogue_replacement_between_passes_does_not_enter_captured_set() {
    let (_runtime, registry, calls) = runtime();
    seed_atom(
        &registry,
        "catalogue-original",
        "Catalogue original",
        "catalogueoriginalmarker",
    )
    .await;
    let original = registry
        .dispatch("knowledge.get", json!({"id": "catalogue-original"}))
        .await
        .expect("read original captured ID");
    let original_id = original["id"]
        .as_str()
        .expect("original ID string")
        .to_owned();
    calls.lock().expect("embedding calls").clear();
    let edit_registry = registry.clone();
    let edit_calls = calls.clone();
    let action: AfterCapture = Box::pin(async move {
        let deleted = edit_registry
            .dispatch(
                "knowledge.delete_atoms",
                json!({"ids": ["catalogue-original"]}),
            )
            .await
            .expect("delete original captured atom");
        assert_eq!(
            deleted["deleted"].as_u64(),
            Some(1),
            "valid replacement deletion"
        );
        seed_atom(
            &edit_registry,
            "catalogue-replacement",
            "Catalogue replacement",
            "cataloguereplacementmarker",
        )
        .await;
        let replacement = edit_registry
            .dispatch("knowledge.get", json!({"id": "catalogue-replacement"}))
            .await
            .expect("read replacement ID");
        let replacement_id = replacement["id"].as_str().expect("replacement ID string");
        assert_ne!(
            replacement_id,
            original_id.as_str(),
            "replacement has a different ID"
        );
        edit_calls.lock().expect("embedding calls").clear();
    });
    let result = AFTER_CAPTURE
        .scope(
            RefCell::new(Some(action)),
            registry.dispatch("knowledge.index", json!({"batch_size": 1})),
        )
        .await
        .expect("index frozen ID set after replacement");
    assert_counts(&result, 1, 0, 1);
    assert!(
        calls.lock().expect("embedding calls").is_empty(),
        "replacement is not embedded in the captured run"
    );

    let next = registry
        .dispatch("knowledge.index", json!({"batch_size": 1}))
        .await
        .expect("fresh run captures replacement ID");
    assert_counts(&next, 1, 1, 0);
    let calls = calls.lock().expect("embedding calls");
    let texts: Vec<_> = calls.iter().flatten().collect();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("cataloguereplacementmarker"));
    assert!(!texts[0].contains("catalogueoriginalmarker"));
}

#[tokio::test]
async fn catalogue_batch_reload_probes_atoms_by_primary_key() {
    let (rt, _registry, _calls) = runtime();
    let reload = khive_runtime::sql!("knowledge_index_atom_batch");
    let plan = rt
        .backend()
        .pool_arc()
        .writer()
        .expect("writer")
        .autocommit(|connection| {
            let mut explain = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {reload}"))
                .expect("plan prepares");
            let plan = explain
                .query_map(rusqlite::params!["local", "[\"a\",\"b\"]"], |row| {
                    row.get::<_, String>(3)
                })
                .expect("plan runs")
                .collect::<rusqlite::Result<Vec<String>>>()
                .expect("plan rows");
            Ok(plan)
        })
        .expect("plan");
    // The selected IDs drive the loop and each atom is one primary-key probe, so a
    // batch costs the same whatever the namespace holds.
    let selected = plan
        .iter()
        .position(|detail| detail.contains("selected"))
        .unwrap_or_else(|| panic!("selected IDs are scanned: {plan:?}"));
    let atom = plan
        .iter()
        .position(|detail| detail.starts_with("SEARCH atom") && detail.contains("(id=?)"))
        .unwrap_or_else(|| panic!("atoms are probed by primary key: {plan:?}"));
    assert!(selected < atom, "selected IDs drive the join: {plan:?}");
}
