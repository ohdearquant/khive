//! Manual candidate-recall measurements on a fixed eligible set.
//! The non-ignored case checks fixture/oracle integrity, without latency gates.

#[path = "../../test_support/retrieval_measure.rs"]
mod measure;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    EmbedderProvider, KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, RuntimeResult,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{
    Entity, SubstrateKind, TextFilter, TextQueryMode, TextSearchRequest, VectorRecord,
};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
enum Arm {
    Text,
    Vector,
}

#[derive(Clone, Copy, Debug)]
enum Filter {
    Kind,
    Type,
    KindAndType,
}

impl Filter {
    fn kind(self) -> Option<&'static str> {
        (!matches!(self, Self::Type)).then_some("concept")
    }

    fn entity_type(self) -> Option<&'static str> {
        (!matches!(self, Self::Kind)).then_some("algorithm")
    }

    fn eligible(self, row: &Entity) -> bool {
        self.kind().is_none_or(|kind| row.kind == kind)
            && self
                .entity_type()
                .is_none_or(|kind| row.entity_type.as_deref() == Some(kind))
    }
}

fn query(arm: Arm) -> &'static str {
    match arm {
        Arm::Text => "retrievalchannel",
        Arm::Vector => "vectorprobe",
    }
}

fn query_vector() -> Vec<f32> {
    let mut vector = vec![0.0; EmbeddingModel::AllMiniLmL6V2.dimensions()];
    vector[0] = 1.0;
    vector
}

fn document_vector(row: &Entity, total: usize) -> Vec<f32> {
    let mut vector = query_vector();
    vector[1] = if row.id.as_u128() < 1_000_000 {
        1.0 + row.id.as_u128() as f32 / 1000.0
    } else {
        (row.id.as_u128() - 1_000_000) as f32 / (total as f32 * 2.0)
    };
    vector
}

struct QueryEmbedding;

#[async_trait]
impl EmbeddingService for QueryEmbedding {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| query_vector()).collect())
    }

    fn supports_model(&self, model: EmbeddingModel) -> bool {
        model == EmbeddingModel::AllMiniLmL6V2
    }

    fn name(&self) -> &'static str {
        "filtered-search-measure-query-vector"
    }
}

struct QueryProvider;

#[async_trait]
impl EmbedderProvider for QueryProvider {
    fn name(&self) -> &str {
        "all-minilm-l6-v2"
    }

    fn dimensions(&self) -> usize {
        EmbeddingModel::AllMiniLmL6V2.dimensions()
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(QueryEmbedding))
    }
}

fn runtime(path: &Path, arm: Arm) -> KhiveRuntime {
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(path.to_owned()),
        embedding_model: matches!(arm, Arm::Vector).then_some(EmbeddingModel::AllMiniLmL6V2),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    if matches!(arm, Arm::Vector) {
        runtime.register_embedder(QueryProvider);
    }
    runtime
}

async fn shutdown(runtime: KhiveRuntime) {
    let join = runtime.backend().pool().take_writer_task_join();
    drop(runtime);
    if let Some(join) = join {
        join.await.unwrap();
    }
}

fn rows(total: usize, eligible: usize, filter: Filter) -> Vec<Entity> {
    assert!(total > eligible && eligible > 0);
    let foreign = (0..total - eligible).map(|ordinal| {
        let kind = match filter {
            Filter::Kind => "person",
            Filter::Type => "concept",
            Filter::KindAndType => {
                if ordinal.is_multiple_of(2) {
                    "person"
                } else {
                    "concept"
                }
            }
        };
        let mut row = Entity::new(
            "local",
            kind,
            "retrievalchannel retrievalchannel retrievalchannel",
        );
        row.id = Uuid::from_u128(1_000_000 + ordinal as u128);
        row.description = Some("retrievalchannel retrievalchannel ordinary padding".into());
        row
    });
    let eligible_rows = (0..eligible).map(|ordinal| {
        let mut row = Entity::new("local", "concept", format!("eligible {ordinal}"));
        row.id = Uuid::from_u128(1 + ordinal as u128);
        row.entity_type = Some("algorithm".into());
        row.description = Some(format!(
            "retrievalchannel {}",
            "padding ".repeat(ordinal + 1)
        ));
        row
    });
    foreign.chain(eligible_rows).collect()
}

async fn seed(runtime: &KhiveRuntime, arm: Arm, rows: &[Entity]) {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entities = runtime.entities(&token).unwrap();
    for batch in rows.chunks(512) {
        assert_eq!(
            entities
                .upsert_entities(batch.to_vec())
                .await
                .unwrap()
                .failed,
            0
        );
    }
    measure::seed_documents(
        &runtime.text(&token).unwrap(),
        rows.iter().map(|row| {
            measure::document(
                row.id,
                "local",
                SubstrateKind::Entity,
                &row.kind,
                row.name.clone(),
                row.description.clone().unwrap(),
            )
        }),
    )
    .await;
    if matches!(arm, Arm::Vector) {
        let store = runtime.vectors(&token).unwrap();
        for batch in rows.chunks(512) {
            let vectors = batch
                .iter()
                .map(|row| VectorRecord {
                    subject_id: row.id,
                    kind: SubstrateKind::Entity,
                    namespace: "local".into(),
                    field: "entity.body".into(),
                    embedding_model: Some(EmbeddingModel::AllMiniLmL6V2.to_string()),
                    vectors: vec![document_vector(row, rows.len())],
                    text_fingerprint: None,
                    updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
                })
                .collect();
            assert_eq!(store.insert_batch(vectors).await.unwrap().failed, 0);
        }
    }
}

fn parameters(arm: Arm, filter: Filter, limit: u32) -> Value {
    let mut params = json!({"kind":"entity", "query":query(arm), "limit":limit});
    if let Some(kind) = filter.kind() {
        params["entity_kind"] = json!(kind);
    }
    if let Some(kind) = filter.entity_type() {
        params["entity_type"] = json!(kind);
    }
    params
}

fn dispatch_fixture(runtime: &KhiveRuntime) -> (KgPack, VerbRegistry, NamespaceToken) {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    (
        KgPack::new(runtime.clone()),
        builder.build().unwrap(),
        token,
    )
}

async fn arm_ids(runtime: &KhiveRuntime, arm: Arm, kind: Option<&str>, cap: u32) -> Vec<Uuid> {
    let token = runtime.authorize(Namespace::local()).unwrap();
    match arm {
        Arm::Text => runtime
            .text(&token)
            .unwrap()
            .search(TextSearchRequest {
                query: query(arm).into(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec!["local".into()],
                    record_kinds: kind.map(|kind| vec![kind.into()]).unwrap_or_default(),
                    ..TextFilter::default()
                }),
                top_k: cap,
                snippet_chars: 200,
            })
            .await
            .unwrap()
            .into_iter()
            .map(|hit| hit.subject_id)
            .collect(),
        Arm::Vector => runtime
            .vector_search(
                &token,
                Some(query_vector()),
                None,
                cap,
                Some(SubstrateKind::Entity),
            )
            .await
            .unwrap()
            .into_iter()
            .map(|hit| hit.subject_id)
            .collect(),
    }
}

async fn oracle(
    runtime: &KhiveRuntime,
    arm: Arm,
    filter: Filter,
    rows: &[Entity],
    count: usize,
) -> Vec<Uuid> {
    let eligible: HashSet<_> = rows
        .iter()
        .filter(|row| filter.eligible(row))
        .map(|row| row.id)
        .collect();
    assert_eq!(eligible.len(), count, "fixed eligible fixture cardinality");
    let all = match arm {
        Arm::Text => arm_ids(runtime, arm, None, rows.len().try_into().unwrap()).await,
        Arm::Vector => {
            let token = runtime.authorize(Namespace::local()).unwrap();
            assert_eq!(
                runtime.vectors(&token).unwrap().count().await.unwrap(),
                rows.len() as u64
            );
            // sqlite-vec caps a KNN request at 4096 neighbours. The oracle
            // instead scans every seeded f32 vector before eligibility/top-k.
            let query = query_vector();
            let mut ranked: Vec<_> = rows
                .iter()
                .map(|row| {
                    let vector = document_vector(row, rows.len());
                    let dot: f64 = vector
                        .iter()
                        .zip(&query)
                        .map(|(a, b)| f64::from(*a) * f64::from(*b))
                        .sum();
                    let norm: f64 = vector.iter().map(|value| f64::from(*value).powi(2)).sum();
                    (row.id, dot / norm.sqrt())
                })
                .collect();
            ranked.sort_by(|(a, sa), (b, sb)| sb.total_cmp(sa).then(a.cmp(b)));
            ranked.into_iter().map(|(id, _)| id).collect()
        }
    };
    assert_eq!(
        all.len(),
        rows.len(),
        "oracle ranking must contain every seeded candidate"
    );
    let eligible_oracle: Vec<_> = all.into_iter().filter(|id| eligible.contains(id)).collect();
    assert_eq!(
        eligible_oracle.len(),
        count,
        "oracle size must equal the eligible corpus"
    );
    eligible_oracle
}

async fn sample(
    runtime: &KhiveRuntime,
    arm: Arm,
    filter: Filter,
    limit: u32,
    expected: &[Uuid],
    rows: &[Entity],
) -> Value {
    let (pack, registry, token) = dispatch_fixture(runtime);
    let start = Instant::now();
    let response = pack
        .dispatch("search", parameters(arm, filter, limit), &registry, &token)
        .await
        .unwrap();
    let elapsed_ns = start.elapsed().as_nanos();
    let ids: Vec<_> = response
        .as_array()
        .unwrap()
        .iter()
        .map(|row| Uuid::parse_str(row["id"].as_str().unwrap()).unwrap())
        .collect();
    let eligible: HashSet<_> = rows
        .iter()
        .filter(|row| filter.eligible(row))
        .map(|row| row.id)
        .collect();
    assert!(
        ids.iter().all(|id| eligible.contains(id)),
        "public search cannot return an ineligible row"
    );
    assert!(ids.len() <= limit as usize);
    let returned: HashSet<_> = ids.iter().copied().collect();
    assert_eq!(
        returned.len(),
        ids.len(),
        "public ranking must not duplicate IDs"
    );
    let cap = limit * 4;
    let text_candidates = runtime
        .text(&token)
        .unwrap()
        .search(TextSearchRequest {
            query: query(arm).into(),
            mode: TextQueryMode::Plain,
            top_k: cap,
            snippet_chars: 200,
            filter: Some(TextFilter {
                namespaces: vec!["local".into()],
                record_kinds: filter
                    .kind()
                    .map(|kind| vec![kind.into()])
                    .unwrap_or_default(),
                ..TextFilter::default()
            }),
        })
        .await
        .unwrap()
        .len();
    let vector_candidates = if matches!(arm, Arm::Vector) {
        arm_ids(runtime, Arm::Vector, None, cap).await.len()
    } else {
        0
    };
    let oracle_top: HashSet<_> = expected.iter().take(limit as usize).copied().collect();
    let overlap = returned.intersection(&oracle_top).count();
    json!({"arm":format!("{arm:?}"), "filter":format!("{filter:?}"), "query":query(arm), "limit":limit,
        "total_rows":rows.len(), "eligible_rows":eligible.len(), "eligible_share":eligible.len() as f64 / rows.len() as f64,
        "fill_rate":ids.len() as f64 / limit as f64, "recall_at_k":overlap as f64 / oracle_top.len() as f64,
        "returned_ids":ids, "oracle_ids":expected.iter().take(limit as usize).collect::<Vec<_>>(),
        "text_candidates":text_candidates, "vector_candidates":vector_candidates, "candidate_cap_per_arm":cap,
        "candidate_count_source":"separate same-parameter arm probes on unchanged corpus, outside timing; pinned runtime multiplier4",
        "latency_ns":elapsed_ns.to_string(), "embedding":"synthetic fixed query vector; no model weights",
        "vector_dimensions":if matches!(arm, Arm::Vector) { EmbeddingModel::AllMiniLmL6V2.dimensions() } else { 0 },
        "measurement_kind":"public KG search on one enabled scoring arm; embedding initialization included when first request",
        "background_write_qualification":"existing KG search telemetry is scheduled asynchronously; timed interval excludes seeding but preserves that product behavior",
        "performance_verdict":"exploratory; no latency/fill/recall threshold"})
}

#[tokio::test]
async fn filtered_search_oracle_and_arm_counts_are_non_vacuous() {
    for arm in [Arm::Text, Arm::Vector] {
        for filter in [Filter::Kind, Filter::Type, Filter::KindAndType] {
            let dir = tempfile::tempdir().unwrap();
            let runtime = runtime(&dir.path().join("filtered.db"), arm);
            let rows = rows(40, 20, filter);
            seed(&runtime, arm, &rows).await;
            let expected = oracle(&runtime, arm, filter, &rows, 20).await;
            if matches!(arm, Arm::Vector) {
                let eligible: HashSet<_> = rows
                    .iter()
                    .filter(|row| filter.eligible(row))
                    .map(|row| row.id)
                    .collect();
                let actual: Vec<_> = arm_ids(&runtime, arm, None, 40)
                    .await
                    .into_iter()
                    .filter(|id| eligible.contains(id))
                    .collect();
                assert_eq!(
                    actual, expected,
                    "small real KNN must agree with the independent eligible oracle"
                );
            }
            let result = sample(&runtime, arm, filter, 10, &expected, &rows).await;
            assert!(
                !result["returned_ids"].as_array().unwrap().is_empty(),
                "small public search must return a known eligible match"
            );
            let count = if matches!(arm, Arm::Text) {
                &result["text_candidates"]
            } else {
                &result["vector_candidates"]
            };
            assert!(count.as_u64().unwrap() > 0);
            if matches!(arm, Arm::Vector) {
                assert_eq!(result["text_candidates"], 0);
            }
            println!("FILTERED_SEARCH_SANITY {result}");
            shutdown(runtime).await;
        }
    }
}

#[tokio::test]
#[ignore = "manual measurement: corpus of up to 100,000 rows; run on an otherwise idle host"]
async fn measure_filtered_search_fill_recall_and_arm_candidates() {
    let mut output = measure::RawRows::new("filtered-search");
    for total in [200, 1000, 10_000, 100_000] {
        for arm in [Arm::Text, Arm::Vector] {
            for filter in [Filter::Kind, Filter::Type, Filter::KindAndType] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("filtered.db");
                let seeded = runtime(&path, arm);
                let rows = rows(total, 100, filter);
                seed(&seeded, arm, &rows).await;
                let expected = oracle(&seeded, arm, filter, &rows, 100).await;
                shutdown(seeded).await;
                for limit in [1, 10, 50, 100] {
                    for first_after_reopen in [true, false] {
                        for index in 0..5 {
                            let runtime = runtime(&path, arm);
                            if !first_after_reopen {
                                let (pack, registry, token) = dispatch_fixture(&runtime);
                                for _ in 0..5 {
                                    pack.dispatch(
                                        "search",
                                        parameters(arm, filter, limit),
                                        &registry,
                                        &token,
                                    )
                                    .await
                                    .unwrap();
                                }
                            }
                            let mut row =
                                sample(&runtime, arm, filter, limit, &expected, &rows).await;
                            row["cache"] = measure::cache_state(first_after_reopen);
                            row["sample"] = json!(index);
                            println!("FILTERED_SEARCH {row}");
                            output.push(&row);
                            shutdown(runtime).await;
                        }
                    }
                }
            }
        }
    }
    output.finish();
}
