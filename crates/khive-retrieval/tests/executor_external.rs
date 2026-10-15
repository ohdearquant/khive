#![cfg(feature = "executor")]

use std::cell::Cell;
use std::future::{ready, Ready};
use std::num::NonZeroUsize;
use std::sync::Mutex;

use async_trait::async_trait;
use khive_retrieval::hybrid::executor::{
    execute_hybrid, execute_memory_hybrid, ArmCompletion, ArmFailure, ArmInput, ArmReport,
    ArmRequest, ArmRoute, ArmSpec, ArmState, CandidateHydrator, CollectedArms, ExecutorArm,
    ExecutorError, ExecutorOutcome, ExecutorRequest, FailurePolicy, HydratedHit, PreparedArms,
    PreparedVector, Scored, SearchScope, VectorGroupPreparer, VectorInput, VectorPreparation,
};
use khive_retrieval::{
    fuse_search_results, FusionStrategy, HybridConfig, HybridSearcher, KeywordSearch, Query,
    VectorSearch,
};
use khive_score::DeterministicScore;
use khive_storage::TextQueryMode;

// These deliberately lack Clone, Debug, Display and serde implementations. The
// executor must transport the values without imposing presentation/storage policy.
// Cell also makes the owned error Send but not Sync.
struct Label(String);
struct Evidence(String);
struct OpaqueError {
    code: u16,
    witness: Box<Cell<u8>>,
}
struct Item(String);

type Row = Scored<u8, Label>;
type ArmResult = Result<ArmCompletion<u8, Label, Evidence>, ArmFailure<OpaqueError, Evidence>>;
type Outcome = ExecutorOutcome<u8, Label, Evidence, Item, OpaqueError>;
type Error = ExecutorError<OpaqueError, Evidence>;

fn fault(code: u16) -> OpaqueError {
    OpaqueError {
        code,
        witness: Box::new(Cell::new(77)),
    }
}

fn row(id: u8, score: f64, label: &str) -> Row {
    (id, DeterministicScore::from_f64(score), Label(label.into()))
}

fn completion(hits: Vec<Row>, route: ArmRoute, reason: &str) -> ArmResult {
    Ok(ArmCompletion {
        hits,
        route,
        evidence: Evidence(reason.into()),
    })
}

fn success<T>(result: Result<T, Error>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("unexpected executor refusal"),
    }
}

fn concat(arms: Vec<Vec<Row>>) -> Ready<Result<Vec<Row>, OpaqueError>> {
    ready(Ok(arms.into_iter().flatten().collect()))
}

#[derive(Debug, PartialEq)]
enum SeenInput {
    Vector(String, String, Option<String>, Option<String>, Vec<f32>),
    Text(String, TextQueryMode, usize),
}

#[derive(Debug, PartialEq)]
struct SeenRequest {
    namespaces: Vec<String>,
    filter: String,
    input: SeenInput,
    limit: u32,
}

fn observe(request: &ArmRequest<'_, str>) -> SeenRequest {
    let input = match request.input {
        ArmInput::Vector {
            engine,
            model_id,
            space_id,
            input,
        } => {
            let (text, vector) = match input {
                VectorInput::Text(text) => (Some((*text).to_owned()), Vec::new()),
                VectorInput::Embedding(vector) => (None, vector.to_vec()),
            };
            SeenInput::Vector(
                (*engine).to_owned(),
                (*model_id).to_owned(),
                space_id.map(str::to_owned),
                text,
                vector,
            )
        }
        ArmInput::Text {
            query,
            mode,
            snippet_chars,
        } => SeenInput::Text((*query).to_owned(), (*mode).clone(), *snippet_chars),
    };
    SeenRequest {
        namespaces: request
            .scope
            .namespaces
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        filter: request.scope.filter.to_owned(),
        input,
        limit: request.candidate_limit,
    }
}

#[derive(Default)]
struct Trace {
    requests: Vec<SeenRequest>,
    phases: Vec<&'static str>,
    prepared_ids: Vec<Vec<u8>>,
    hydration_scopes: Vec<(Vec<String>, String)>,
}

struct FakeArm<'a> {
    trace: &'a Mutex<Trace>,
    output: Mutex<Option<ArmResult>>,
}

impl<'a> FakeArm<'a> {
    fn new(trace: &'a Mutex<Trace>, output: ArmResult) -> Self {
        Self {
            trace,
            output: Mutex::new(Some(output)),
        }
    }
}

#[async_trait]
impl ExecutorArm for FakeArm<'_> {
    type Id = u8;
    type Label = Label;
    type Evidence = Evidence;
    type Filter = str;
    type Error = OpaqueError;

    async fn search<'a>(&'a self, request: ArmRequest<'a, str>) -> ArmResult {
        let before = observe(&request);
        tokio::task::yield_now().await;
        assert_eq!(
            before,
            observe(&request),
            "borrowed input must survive await"
        );
        self.trace.lock().unwrap().requests.push(before);
        self.output.lock().unwrap().take().expect("arm called once")
    }
}

#[derive(Clone, Copy)]
enum Flow {
    Pass,
    EligibilityAndSalience,
    AddBefore,
    DuplicateBefore,
    AddAfter,
    DuplicateAfter,
    FailOutcomes,
    AddOutcome,
    ChangeOutcomeScore,
    ChangeAccounting,
    FailPreparation,
    FailHydration,
}

struct BatchState {
    seen: Vec<u8>,
}

struct Hydrator<'a> {
    trace: &'a Mutex<Trace>,
    flow: Flow,
}

#[async_trait]
impl CandidateHydrator for Hydrator<'_> {
    type Id = u8;
    type Label = Label;
    type Evidence = Evidence;
    type Filter = str;
    type Item = Item;
    type State = BatchState;
    type Error = OpaqueError;

    fn validate_outcomes(
        &self,
        mut collected: CollectedArms<u8, Label, Evidence, OpaqueError>,
    ) -> Result<CollectedArms<u8, Label, Evidence, OpaqueError>, OpaqueError> {
        self.trace.lock().unwrap().phases.push("outcomes");
        if matches!(self.flow, Flow::FailOutcomes) {
            return Err(fault(31));
        }
        match self.flow {
            Flow::AddOutcome => collected.arms[0].push(row(99, 0.5, "invented")),
            Flow::ChangeOutcomeScore => collected.arms[0][0].1 = DeterministicScore::ZERO,
            Flow::ChangeAccounting => collected.reports[0].returned += 1,
            _ => {}
        }
        Ok(collected)
    }

    async fn prepare_arms<'a>(
        &'a self,
        scope: SearchScope<'a, str>,
        mut arms: Vec<Vec<Row>>,
    ) -> Result<PreparedArms<u8, Label, BatchState>, OpaqueError> {
        assert!(!scope.namespaces.is_empty());
        let ids: Vec<_> = arms.iter().flatten().map(|(id, _, _)| *id).collect();
        {
            let mut trace = self.trace.lock().unwrap();
            trace.phases.push("prepare");
            trace.prepared_ids.push(ids.clone());
            trace.hydration_scopes.push((
                scope.namespaces.iter().map(|s| (*s).to_owned()).collect(),
                scope.filter.to_owned(),
            ));
        }
        tokio::task::yield_now().await;
        match self.flow {
            Flow::EligibilityAndSalience => {
                for arm in &mut arms {
                    arm.retain(|(id, _, _)| *id != 1);
                }
            }
            Flow::AddBefore => arms[0].push(row(99, 0.5, "invented")),
            Flow::DuplicateBefore => arms[0].push(row(ids[0], 0.5, "duplicate")),
            Flow::FailPreparation => return Err(fault(32)),
            _ => {}
        }
        Ok(PreparedArms {
            arms,
            state: BatchState { seen: ids },
        })
    }

    async fn hydrate_filter_rank<'a>(
        &'a self,
        scope: SearchScope<'a, str>,
        state: BatchState,
        candidates: Vec<Row>,
    ) -> Result<Vec<HydratedHit<u8, Label, Item>>, OpaqueError> {
        assert!(!scope.namespaces.is_empty());
        {
            let mut trace = self.trace.lock().unwrap();
            trace.phases.push("hydrate");
            trace.hydration_scopes.push((
                scope.namespaces.iter().map(|s| (*s).to_owned()).collect(),
                scope.filter.to_owned(),
            ));
        }
        if matches!(self.flow, Flow::FailHydration) {
            return Err(fault(33));
        }
        let mut hits: Vec<_> = candidates
            .into_iter()
            .map(|(id, score, label)| {
                assert!(state.seen.contains(&id));
                HydratedHit {
                    id,
                    score,
                    label,
                    item: Item(format!("loaded-{id}")),
                }
            })
            .collect();
        match self.flow {
            Flow::EligibilityAndSalience => {
                hits.retain(|hit| hit.id != 2);
                for hit in &mut hits {
                    hit.score = DeterministicScore::from_f64(if hit.id == 4 { 0.4 } else { 0.2 });
                }
                hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(&b.id)));
            }
            Flow::AddAfter | Flow::DuplicateAfter => hits.push(HydratedHit {
                id: if matches!(self.flow, Flow::AddAfter) {
                    99
                } else {
                    hits[0].id
                },
                score: DeterministicScore::ZERO,
                label: Label("added".into()),
                item: Item("added".into()),
            }),
            _ => {}
        }
        Ok(hits)
    }
}

fn request<'a, A>(
    arms: &'a [ArmSpec<'a, A>],
    namespaces: &'a [&'a str],
    filter: &'a str,
    max_candidates: usize,
    final_limit: usize,
) -> ExecutorRequest<'a, A>
where
    A: ExecutorArm<Id = u8, Label = Label, Evidence = Evidence, Filter = str, Error = OpaqueError>
        + ?Sized,
{
    ExecutorRequest {
        operation: "executor.external",
        scope: SearchScope { namespaces, filter },
        arms,
        max_in_flight: NonZeroUsize::new(2).unwrap(),
        vector_slots: &[],
        vector_preparer: None,
        max_candidates,
        final_limit,
    }
}

include!("executor_external/scope_and_hydration.rs");

struct Preparer<'a>(&'a Mutex<Trace>);

#[async_trait]
impl VectorGroupPreparer for Preparer<'_> {
    type Filter = str;
    type Evidence = Evidence;
    type Error = OpaqueError;

    async fn prepare<'a>(
        &'a self,
        requests: Vec<(usize, ArmRequest<'a, str>)>,
        vector_slot_count: usize,
    ) -> Result<VectorPreparation<Evidence>, OpaqueError> {
        assert_eq!(vector_slot_count, 2);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, 1, "skip does not renumber selected model");
        assert_eq!(
            observe(&requests[0].1),
            SeenRequest {
                namespaces: vec!["ns".into()],
                filter: "kind=memory".into(),
                limit: 1,
                input: SeenInput::Vector(
                    "second-engine".into(),
                    "model-b".into(),
                    Some("space-b".into()),
                    Some("borrowed query".into()),
                    vec![]
                ),
            }
        );
        self.0.lock().unwrap().phases.push("embedding");
        tokio::task::yield_now().await;
        Ok(VectorPreparation {
            arms: vec![PreparedVector {
                embedding: Some(vec![0.5, 0.25]),
                evidence: Evidence("model-prepared".into()),
            }],
            evidence: Evidence("session-proof-retained".into()),
        })
    }
}

include!("executor_external/vector_preparation.rs");

include!("executor_external/routes_and_errors.rs");

include!("executor_external/validation_and_errors.rs");

struct Legacy;

#[async_trait]
impl VectorSearch for Legacy {
    type Id = u8;

    async fn vector_search(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> khive_retrieval::Result<Vec<(u8, DeterministicScore)>> {
        assert_eq!(embedding, [0.25, 0.75]);
        assert_eq!(top_k, 5);
        Ok(vec![(1, DeterministicScore::from_f64(0.8))])
    }
}

#[async_trait]
impl KeywordSearch for Legacy {
    type Id = u8;

    async fn keyword_search(
        &self,
        text: &str,
        top_k: usize,
    ) -> khive_retrieval::Result<Vec<(u8, DeterministicScore)>> {
        assert_eq!(text, "legacy query");
        assert_eq!(top_k, 5);
        Ok(vec![(2, DeterministicScore::from_f64(0.7))])
    }
}

#[async_trait]
impl HybridSearcher for Legacy {
    async fn hybrid_search(
        &self,
        query: &Query,
        config: &HybridConfig,
    ) -> khive_retrieval::Result<Vec<(u8, DeterministicScore)>> {
        let vectors = self
            .vector_search(
                query.embedding.as_deref().expect("vector query"),
                config.candidate_pool_size,
            )
            .await?;
        let text = self
            .keyword_search(&query.text, config.candidate_pool_size)
            .await?;
        Ok(fuse_search_results(vec![vectors, text], config).expect("legacy fusion"))
    }
}

#[tokio::test]
async fn legacy_traits_and_custom_fallback_helpers_remain_external_apis() {
    let query = Query::hybrid("legacy query", vec![0.25, 0.75]);
    let custom =
        HybridConfig::new(2)
            .with_pool_size(5)
            .with_fusion_strategy(FusionStrategy::Custom {
                name: "legacy-unknown".into(),
                params: serde_json::json!({}),
            });
    let rrf = HybridConfig::new(2).with_pool_size(5);
    let legacy = Legacy;
    let custom_hits = legacy.hybrid_search(&query, &custom).await.unwrap();
    let rrf_hits = legacy.hybrid_search(&query, &rrf).await.unwrap();
    assert_eq!(custom_hits, rrf_hits);
    assert_eq!(
        custom_hits.iter().map(|hit| hit.0).collect::<Vec<_>>(),
        [1, 2]
    );
    let labelled = vec![
        vec![(1u8, DeterministicScore::from_f64(0.8), "vector".to_owned())],
        vec![(1u8, DeterministicScore::from_f64(0.7), "text".to_owned())],
    ];
    let merge = |a: String, b: String| format!("{a}+{b}");
    let old_custom =
        khive_retrieval::hybrid::fuse_labelled_scored(labelled.clone(), &custom, merge).unwrap();
    let old_rrf = khive_retrieval::hybrid::fuse_labelled_scored(labelled, &rrf, merge).unwrap();
    assert_eq!(old_custom, old_rrf);
    assert_eq!(old_custom[0].2, "vector+text");
}
