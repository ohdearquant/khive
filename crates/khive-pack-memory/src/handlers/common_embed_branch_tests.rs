use std::sync::Arc;

use async_trait::async_trait;
use khive_runtime::{EmbedderProvider, KhiveRuntime, Namespace};
use khive_storage::usage::{self, UsageContext};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

use super::{
    embed_query_model, MemoryPack, QueryEmbeddingCache, RecallCandidateParams, RuntimeError,
    TextSnippetPolicy,
};
use crate::test_support::HashVecProvider;

#[derive(Clone, Copy)]
enum PanicKind {
    Str,
    Formatted,
    Opaque,
}

struct PanickingService(PanicKind);

#[async_trait]
impl EmbeddingService for PanickingService {
    async fn embed(
        &self,
        _texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let code = 7;
        match self.0 {
            PanicKind::Str => panic!("provider embed panic"),
            PanicKind::Formatted => panic!("provider embed panic {code}"),
            PanicKind::Opaque => std::panic::panic_any(code),
        }
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "panicking-embed"
    }
}

/// A pack-registered provider whose build succeeds and whose query embedding panics.
struct PanickingProvider {
    name: &'static str,
    kind: PanicKind,
}

#[async_trait]
impl EmbedderProvider for PanickingProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn dimensions(&self) -> usize {
        8
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(PanickingService(self.kind)))
    }
}

fn healthy(name: String) -> HashVecProvider {
    HashVecProvider {
        model_name: name,
        dims: 8,
    }
}

/// Run recall candidate collection over every registered model and return the
/// sorted names of the models whose query embedding reached the vector stage.
async fn embedded_models(rt: &KhiveRuntime) -> Result<Vec<String>, RuntimeError> {
    let token = rt.authorize(Namespace::local()).unwrap();
    let pack = MemoryPack::new(rt.clone());
    let gather = crate::config::RecallFtsGatherConfig::default();
    let candidates = pack
        .collect_recall_candidates(
            "embed branch query",
            &token,
            RecallCandidateParams {
                candidate_limit: 10,
                embedding_model: None,
                session_fence: None,
                cjk_fts_bypass: false,
                snippet_policy: TextSnippetPolicy::Omit,
                fts_gather: &gather,
                ann_overfetch_max_rounds: 1,
                ann_ready_timeout_ms: 10,
            },
        )
        .await?;
    let mut models: Vec<String> = candidates
        .vector_hits_per_model
        .into_iter()
        .map(|(model, _)| model)
        .collect();
    models.sort();
    Ok(models)
}

#[tokio::test]
async fn panicking_query_embed_is_an_internal_error_for_that_model_alone() {
    let rt = KhiveRuntime::memory().unwrap();
    rt.register_embedder(PanickingProvider {
        name: "embed-panic-str",
        kind: PanicKind::Str,
    });
    rt.register_embedder(PanickingProvider {
        name: "embed-panic-formatted",
        kind: PanicKind::Formatted,
    });
    rt.register_embedder(PanickingProvider {
        name: "embed-panic-opaque",
        kind: PanicKind::Opaque,
    });
    rt.register_embedder(healthy("embed-panic-healthy".to_owned()));
    let cache = QueryEmbeddingCache::with_default_capacity();
    let embed = |model: &str| {
        embed_query_model(
            rt.clone(),
            cache.clone(),
            model.to_owned(),
            "embed panic query".to_owned(),
        )
    };
    let (str_panic, formatted_panic, opaque_panic, ok) = tokio::join!(
        embed("embed-panic-str"),
        embed("embed-panic-formatted"),
        embed("embed-panic-opaque"),
        embed("embed-panic-healthy"),
    );
    assert!(matches!(
        &str_panic,
        Err(RuntimeError::Internal(message))
            if message == "recall embed task panicked: provider embed panic"
    ));
    assert!(matches!(
        &formatted_panic,
        Err(RuntimeError::Internal(message))
            if message == "recall embed task panicked: provider embed panic 7"
    ));
    assert!(matches!(
        &opaque_panic,
        Err(RuntimeError::Internal(message))
            if message == "recall embed task panicked: non-string panic payload"
    ));
    let (model, vector) = ok.unwrap();
    assert_eq!(model, "embed-panic-healthy");
    assert_eq!(vector.len(), 8);
    assert_eq!(cache.len(), 1, "a panicked embed must write nothing");
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn recall_embed_stage_degrades_past_a_panicking_provider_in_every_branch() {
    // One healthy model beside the faulty one takes the two-model path; two take
    // the spawned path used for three or more models.
    for healthy_count in [1usize, 2] {
        let rt = KhiveRuntime::memory().unwrap();
        let mut expected = Vec::new();
        for index in 0..healthy_count {
            let name = format!("embed-branch-healthy-{healthy_count}-{index}");
            rt.register_embedder(healthy(name.clone()));
            expected.push(name);
        }
        rt.register_embedder(PanickingProvider {
            name: "embed-branch-faulty",
            kind: PanicKind::Str,
        });
        expected.sort();
        let models = embedded_models(&rt)
            .await
            .unwrap_or_else(|error| panic!("{healthy_count} healthy + 1 faulty: {error}"));
        assert_eq!(models, expected, "{healthy_count} healthy + 1 faulty");
    }
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn every_recall_embed_branch_counts_one_embed_call_per_model() {
    for model_count in [1usize, 2, 3] {
        let rt = KhiveRuntime::memory().unwrap();
        for index in 0..model_count {
            rt.register_embedder(healthy(format!("embed-usage-{model_count}-{index}")));
        }
        let ctx = UsageContext::new();
        let models = usage::scope(ctx.clone(), embedded_models(&rt))
            .await
            .unwrap();
        assert_eq!(models.len(), model_count);
        assert_eq!(
            ctx.snapshot()["embed_calls"],
            model_count as u64,
            "{model_count}-model cache miss"
        );
    }
}

#[tokio::test]
async fn cold_embedder_initialization_event_counts_toward_the_triggering_operation() {
    let rt = KhiveRuntime::memory().unwrap();
    rt.register_embedder(healthy("embed-usage-cold".to_owned()));
    let ctx = UsageContext::new();
    usage::scope(
        ctx.clone(),
        embed_query_model(
            rt.clone(),
            QueryEmbeddingCache::with_default_capacity(),
            "embed-usage-cold".to_owned(),
            "cold usage query".to_owned(),
        ),
    )
    .await
    .unwrap();
    let snapshot = ctx.snapshot();
    assert_eq!(snapshot["embed_calls"], 1u64);
    assert_eq!(
        snapshot["event_rows"], 1u64,
        "the initialization event row belongs to the operation that started the build"
    );
}
