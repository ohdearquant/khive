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

#[tokio::test]
async fn external_caller_preserves_scopes_and_batches_eligibility_before_ranking_and_cut() {
    let trace = Mutex::new(Trace::default());
    let namespace = String::from("visible-a");
    let other = String::from("visible-b");
    let all = [namespace.as_str(), other.as_str()];
    let primary = [namespace.as_str()];
    let filter = String::from("kind=note;tag=visible");
    let query = String::from("query text");
    let mode = TextQueryMode::AnyTerm;
    let vector = [0.25, 0.75];
    let vector_arm = FakeArm::new(
        &trace,
        completion(
            vec![
                row(1, 0.99, "vector-1"),
                row(2, 0.9, "vector-2"),
                row(3, 0.2, "vector-3"),
            ],
            ArmRoute::Primary,
            "primary-complete",
        ),
    );
    let text_arm = FakeArm::new(
        &trace,
        completion(
            vec![row(4, 0.1, "text-4")],
            ArmRoute::Primary,
            "text-complete",
        ),
    );
    let vector_adapter: &dyn ExecutorArm<
        Id = u8,
        Label = Label,
        Evidence = Evidence,
        Filter = str,
        Error = OpaqueError,
    > = &vector_arm;
    let text_adapter: &dyn ExecutorArm<
        Id = u8,
        Label = Label,
        Evidence = Evidence,
        Filter = str,
        Error = OpaqueError,
    > = &text_arm;
    let arms = [
        ArmSpec {
            key: "vector",
            adapter: vector_adapter,
            scope: SearchScope {
                namespaces: &primary,
                filter: &filter,
            },
            input: Some(ArmInput::Vector {
                engine: "named-engine",
                model_id: "canonical-model",
                space_id: Some("verified-space"),
                input: VectorInput::Embedding(&vector),
            }),
            candidate_limit: 3,
            failure: FailurePolicy::Required,
        },
        ArmSpec {
            key: "text",
            adapter: text_adapter,
            scope: SearchScope {
                namespaces: &all,
                filter: &filter,
            },
            input: Some(ArmInput::Text {
                query: &query,
                mode: &mode,
                snippet_chars: 37,
            }),
            candidate_limit: 1,
            failure: FailurePolicy::Required,
        },
    ];
    let hydrator = Hydrator {
        trace: &trace,
        flow: Flow::EligibilityAndSalience,
    };
    let external_hydrator: &dyn CandidateHydrator<
        Id = u8,
        Label = Label,
        Evidence = Evidence,
        Filter = str,
        Item = Item,
        State = BatchState,
        Error = OpaqueError,
    > = &hydrator;
    let mut req = request(&arms, &all, &filter, 4, 1);
    req.vector_slots = &[0];
    // The returned future borrows an outer environment; it is not 'static.
    let fusion_marker = String::from("caller-owned-fusion");
    let marker = fusion_marker.as_str();
    let out: Outcome = success(
        execute_hybrid(req, external_hydrator, || {
            trace.lock().unwrap().phases.push("resolve");
            Ok(move |streams: Vec<Vec<Row>>| async move {
                assert_eq!(marker, "caller-owned-fusion");
                let ids: Vec<Vec<_>> = streams
                    .iter()
                    .map(|arm| arm.iter().map(|r| r.0).collect())
                    .collect();
                assert_eq!(
                    ids,
                    vec![vec![2, 3], vec![4]],
                    "eligibility precedes fusion"
                );
                Ok(streams.into_iter().flatten().collect())
            })
        })
        .await,
    );
    assert_eq!(out.hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![4]);
    assert_eq!(out.hits[0].score, DeterministicScore::from_f64(0.4));
    assert_eq!(out.hits[0].label.0, "text-4");
    assert_eq!(out.hits[0].item.0, "loaded-4");
    assert_eq!(
        out.arms
            .iter()
            .map(|arm| (arm.requested, arm.returned))
            .collect::<Vec<_>>(),
        [(3, 3), (1, 1)]
    );
    let trace = trace.lock().unwrap();
    assert_eq!(trace.phases, ["resolve", "outcomes", "prepare", "hydrate"]);
    assert_eq!(
        trace.prepared_ids,
        vec![vec![1, 2, 3, 4]],
        "one batch, full raw union"
    );
    assert_eq!(
        trace.hydration_scopes,
        vec![(vec![namespace.clone(), other.clone()], filter.clone()); 2]
    );
    let seen_vector = trace
        .requests
        .iter()
        .find(|r| matches!(r.input, SeenInput::Vector(..)))
        .unwrap();
    assert_eq!(seen_vector.namespaces, ["visible-a"]);
    assert_eq!(seen_vector.filter, filter);
    assert_eq!(seen_vector.limit, 3);
    assert_eq!(
        seen_vector.input,
        SeenInput::Vector(
            "named-engine".into(),
            "canonical-model".into(),
            Some("verified-space".into()),
            None,
            vector.to_vec()
        )
    );
    let seen_text = trace
        .requests
        .iter()
        .find(|r| matches!(r.input, SeenInput::Text(..)))
        .unwrap();
    assert_eq!(seen_text.namespaces, ["visible-a", "visible-b"]);
    assert_eq!(seen_text.filter, filter);
    assert_eq!(seen_text.limit, 1);
    assert_eq!(seen_text.input, SeenInput::Text(query, mode, 37));
}

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

#[tokio::test]
async fn external_preparer_preserves_original_slots_and_owned_embedding_lifetime() {
    let trace = Mutex::new(Trace::default());
    let unused = FakeArm::new(
        &trace,
        completion(vec![], ArmRoute::Primary, "must-not-run"),
    );
    let selected = FakeArm::new(
        &trace,
        completion(
            vec![row(7, 0.7, "b")],
            ArmRoute::ExactFallback,
            "index-absent",
        ),
    );
    let ns = ["ns"];
    let query = String::from("borrowed query");
    let arms = [
        ArmSpec {
            key: "skipped",
            adapter: &unused,
            scope: SearchScope {
                namespaces: &ns,
                filter: "kind=memory",
            },
            input: None,
            candidate_limit: 7,
            failure: FailurePolicy::Required,
        },
        ArmSpec {
            key: "selected",
            adapter: &selected,
            scope: SearchScope {
                namespaces: &ns,
                filter: "kind=memory",
            },
            input: Some(ArmInput::Vector {
                engine: "second-engine",
                model_id: "model-b",
                space_id: Some("space-b"),
                input: VectorInput::Text(&query),
            }),
            candidate_limit: 1,
            failure: FailurePolicy::Required,
        },
    ];
    let preparer = Preparer(&trace);
    let external_preparer: &dyn VectorGroupPreparer<
        Filter = str,
        Evidence = Evidence,
        Error = OpaqueError,
    > = &preparer;
    let mut req = request(&arms, &ns, "kind=memory", 1, 1);
    req.vector_slots = &[0, 1];
    req.vector_preparer = Some(external_preparer);
    let out = success(
        execute_memory_hybrid(
            req,
            &Hydrator {
                trace: &trace,
                flow: Flow::Pass,
            },
            || Ok(concat),
        )
        .await,
    );
    assert_eq!(out.hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![7]);
    assert_eq!(
        out.vector_preparation.expect("aggregate evidence").0,
        "session-proof-retained"
    );
    assert!(matches!(out.arms[0].state, ArmState::Skipped));
    assert_eq!(
        out.arms[1]
            .preparation
            .as_ref()
            .expect("per-model evidence")
            .0,
        "model-prepared"
    );
    assert!(
        matches!(&out.arms[1].state, ArmState::Completed { route: ArmRoute::ExactFallback, evidence } if evidence.0 == "index-absent")
    );
    let trace = trace.lock().unwrap();
    assert_eq!(trace.requests.len(), 1);
    assert_eq!(
        trace.requests[0].input,
        SeenInput::Vector(
            "second-engine".into(),
            "model-b".into(),
            Some("space-b".into()),
            None,
            vec![0.5, 0.25]
        )
    );
}

#[tokio::test]
async fn unknown_custom_returns_the_original_typed_error_before_zero_limit_work() {
    let trace = Mutex::new(Trace::default());
    let ns = ["ns"];
    let mode = TextQueryMode::Plain;
    let arm = FakeArm::new(&trace, completion(vec![], ArmRoute::Primary, "empty"));
    let arms = [ArmSpec {
        key: "text",
        adapter: &arm,
        scope: SearchScope {
            namespaces: &ns,
            filter: "all",
        },
        input: Some(ArmInput::Text {
            query: "q",
            mode: &mode,
            snippet_chars: 0,
        }),
        candidate_limit: 0,
        failure: FailurePolicy::Required,
    }];
    let strategy = FusionStrategy::Custom {
        name: "unregistered".into(),
        params: serde_json::json!({"opaque": 5}),
    };
    let error = fault(41);
    let original_witness = (&*error.witness) as *const Cell<u8> as usize;
    let result = execute_hybrid(request(&arms, &ns, "all", 0, 0), &Hydrator { trace: &trace, flow: Flow::Pass }, || {
        trace.lock().unwrap().phases.push("resolve");
        assert!(matches!(&strategy, FusionStrategy::Custom { name, params } if name == "unregistered" && params["opaque"] == 5));
        Err::<fn(Vec<Vec<Row>>) -> Ready<Result<Vec<Row>, OpaqueError>>, _>(error)
    }).await;
    match result {
        Err(ExecutorError::FusionResolution(error)) => {
            assert_eq!(error.code, 41);
            assert_eq!(
                (&*error.witness) as *const Cell<u8> as usize,
                original_witness
            );
        }
        _ => panic!("expected the unresolved Custom error"),
    }
    let trace = trace.lock().unwrap();
    assert!(trace.requests.is_empty());
    assert_eq!(trace.phases, ["resolve"]);
}

fn text_spec<'a>(
    key: &'a str,
    adapter: &'a FakeArm<'a>,
    namespaces: &'a [&'a str],
    mode: &'a TextQueryMode,
    limit: u32,
    failure: FailurePolicy,
) -> ArmSpec<'a, FakeArm<'a>> {
    ArmSpec {
        key,
        adapter,
        scope: SearchScope {
            namespaces,
            filter: "all",
        },
        input: Some(ArmInput::Text {
            query: "q",
            mode,
            snippet_chars: 0,
        }),
        candidate_limit: limit,
        failure,
    }
}

#[tokio::test]
async fn empty_unavailable_failed_and_exact_routes_remain_distinct_evidence() {
    let trace = Mutex::new(Trace::default());
    let ns = ["ns"];
    let mode = TextQueryMode::Plain;
    let empty = FakeArm::new(
        &trace,
        completion(vec![], ArmRoute::Primary, "healthy-empty"),
    );
    let unavailable = FakeArm::new(
        &trace,
        Err(ArmFailure::Unavailable(Evidence("not-built".into()))),
    );
    let failed = FakeArm::new(&trace, Err(ArmFailure::Ordinary(fault(12))));
    let exact = FakeArm::new(
        &trace,
        completion(
            vec![row(8, 0.8, "exact")],
            ArmRoute::ExactFallback,
            "index-unavailable",
        ),
    );
    let arms = [
        text_spec("empty", &empty, &ns, &mode, 1, FailurePolicy::Required),
        text_spec(
            "unavailable",
            &unavailable,
            &ns,
            &mode,
            1,
            FailurePolicy::TolerateOrdinary,
        ),
        text_spec(
            "failed",
            &failed,
            &ns,
            &mode,
            1,
            FailurePolicy::TolerateOrdinary,
        ),
        text_spec("exact", &exact, &ns, &mode, 1, FailurePolicy::Required),
    ];
    let out = success(
        execute_hybrid(
            request(&arms, &ns, "all", 4, 4),
            &Hydrator {
                trace: &trace,
                flow: Flow::Pass,
            },
            || {
                Ok(|streams: Vec<Vec<Row>>| {
                    assert_eq!(
                        streams.iter().map(Vec::len).collect::<Vec<_>>(),
                        [0, 0, 0, 1]
                    );
                    concat(streams)
                })
            },
        )
        .await,
    );
    assert_eq!(out.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [8]);
    assert_eq!(
        out.arms
            .iter()
            .map(|report| (report.key.as_str(), report.requested, report.returned))
            .collect::<Vec<_>>(),
        [
            ("empty", 1, 0),
            ("unavailable", 1, 0),
            ("failed", 1, 0),
            ("exact", 1, 1)
        ]
    );
    let empty_report: &ArmReport<Evidence, OpaqueError> = &out.arms[0];
    assert!(
        matches!(&empty_report.state, ArmState::Completed { route: ArmRoute::Primary, evidence } if evidence.0 == "healthy-empty")
    );
    assert!(
        matches!(&out.arms[1].state, ArmState::Unavailable(evidence) if evidence.0 == "not-built")
    );
    assert!(
        matches!(&out.arms[2].state, ArmState::Failed(error) if error.code == 12 && error.witness.get() == 77)
    );
    assert!(
        matches!(&out.arms[3].state, ArmState::Completed { route: ArmRoute::ExactFallback, evidence } if evidence.0 == "index-unavailable")
    );
    assert_eq!(trace.lock().unwrap().requests.len(), 4);
}

#[tokio::test]
async fn required_unavailability_and_tolerated_fatal_errors_do_not_become_empty_success() {
    for fatal in [false, true] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let failure = if fatal {
            ArmFailure::Fatal(fault(19))
        } else {
            ArmFailure::Unavailable(Evidence("required-index".into()))
        };
        let arm = FakeArm::new(&trace, Err(failure));
        let policy = if fatal {
            FailurePolicy::TolerateOrdinary
        } else {
            FailurePolicy::Required
        };
        let arms = [text_spec("required", &arm, &ns, &mode, 1, policy)];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 1, 1),
            &Hydrator {
                trace: &trace,
                flow: Flow::Pass,
            },
            || Ok(concat),
        )
        .await;
        match result {
            Err(ExecutorError::Unavailable { slot: 0, evidence }) if !fatal => {
                assert_eq!(evidence.0, "required-index")
            }
            Err(ExecutorError::Arm { slot: 0, error }) if fatal => assert_eq!(error.code, 19),
            _ => panic!("required absence or fatal error was swallowed"),
        }
        assert!(trace.lock().unwrap().phases.is_empty());
    }
}

#[tokio::test]
async fn selected_zero_window_still_runs_and_returns_its_typed_failure() {
    let trace = Mutex::new(Trace::default());
    let ns = ["ns"];
    let mode = TextQueryMode::Plain;
    let arm = FakeArm::new(&trace, Err(ArmFailure::Ordinary(fault(23))));
    let arms = [text_spec(
        "zero",
        &arm,
        &ns,
        &mode,
        0,
        FailurePolicy::Required,
    )];
    let result = execute_hybrid(
        request(&arms, &ns, "all", 0, 0),
        &Hydrator {
            trace: &trace,
            flow: Flow::Pass,
        },
        || Ok(concat),
    )
    .await;
    assert!(matches!(result, Err(ExecutorError::Arm { slot: 0, error }) if error.code == 23));
    let trace = trace.lock().unwrap();
    assert_eq!(trace.requests.len(), 1);
    assert_eq!(trace.requests[0].limit, 0);
    assert!(trace.phases.is_empty());
}

#[tokio::test]
async fn adapter_candidate_cap_is_enforced_before_hydration_or_fusion() {
    let trace = Mutex::new(Trace::default());
    let ns = ["ns"];
    let mode = TextQueryMode::Plain;
    let arm = FakeArm::new(
        &trace,
        completion(
            vec![row(1, 0.9, "a"), row(2, 0.8, "b")],
            ArmRoute::Primary,
            "too-many",
        ),
    );
    let arms = [text_spec(
        "over-cap",
        &arm,
        &ns,
        &mode,
        1,
        FailurePolicy::Required,
    )];
    let result = execute_hybrid(
        request(&arms, &ns, "all", 1, 1),
        &Hydrator {
            trace: &trace,
            flow: Flow::Pass,
        },
        || {
            Ok(|_: Vec<Vec<Row>>| {
                trace.lock().unwrap().phases.push("unexpected-fusion");
                concat(vec![])
            })
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(ExecutorError::Protocol { slot: Some(0), .. })
    ));
    assert!(trace.lock().unwrap().phases.is_empty());
}

#[tokio::test]
async fn outcome_validation_cannot_rewrite_candidates_scores_or_raw_accounting() {
    for flow in [
        Flow::AddOutcome,
        Flow::ChangeOutcomeScore,
        Flow::ChangeAccounting,
    ] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let arm = FakeArm::new(
            &trace,
            completion(vec![row(1, 0.9, "original")], ArmRoute::Primary, "one"),
        );
        let arms = [text_spec(
            "one",
            &arm,
            &ns,
            &mode,
            2,
            FailurePolicy::Required,
        )];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 2, 2),
            &Hydrator {
                trace: &trace,
                flow,
            },
            || Ok(concat),
        )
        .await;
        assert!(matches!(result, Err(ExecutorError::Protocol { .. })));
        assert_eq!(trace.lock().unwrap().phases, ["outcomes"]);
    }
}

#[tokio::test]
async fn preparing_and_hydrating_must_not_add_identities_or_duplicate_multiplicity() {
    for flow in [
        Flow::AddBefore,
        Flow::DuplicateBefore,
        Flow::AddAfter,
        Flow::DuplicateAfter,
    ] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let arm = FakeArm::new(
            &trace,
            completion(vec![row(1, 0.9, "original")], ArmRoute::Primary, "one"),
        );
        // Two slots of headroom isolate identity/multiplicity validation from cap validation.
        let arms = [text_spec(
            "one",
            &arm,
            &ns,
            &mode,
            2,
            FailurePolicy::Required,
        )];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 2, 2),
            &Hydrator {
                trace: &trace,
                flow,
            },
            || Ok(concat),
        )
        .await;
        assert!(matches!(result, Err(ExecutorError::Protocol { .. })));
        assert_eq!(trace.lock().unwrap().prepared_ids, [vec![1]]);
    }
}

#[tokio::test]
async fn fusion_must_not_add_identities_or_duplicate_multiplicity() {
    for id in [99, 1] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let arm = FakeArm::new(
            &trace,
            completion(vec![row(1, 0.9, "original")], ArmRoute::Primary, "one"),
        );
        let arms = [text_spec(
            "one",
            &arm,
            &ns,
            &mode,
            2,
            FailurePolicy::Required,
        )];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 2, 2),
            &Hydrator {
                trace: &trace,
                flow: Flow::Pass,
            },
            || {
                Ok(move |streams: Vec<Vec<Row>>| {
                    let mut rows: Vec<_> = streams.into_iter().flatten().collect();
                    rows.push(row(id, 0.8, "injected"));
                    ready(Ok(rows))
                })
            },
        )
        .await;
        assert!(matches!(result, Err(ExecutorError::Protocol { .. })));
        assert_eq!(trace.lock().unwrap().phases, ["outcomes", "prepare"]);
    }
}

#[tokio::test]
async fn callback_errors_remain_owned_typed_values_without_display_or_debug() {
    for flow in [
        Flow::FailOutcomes,
        Flow::FailPreparation,
        Flow::FailHydration,
    ] {
        let trace = Mutex::new(Trace::default());
        let ns = ["ns"];
        let mode = TextQueryMode::Plain;
        let arm = FakeArm::new(
            &trace,
            completion(vec![row(1, 0.9, "original")], ArmRoute::Primary, "one"),
        );
        let arms = [text_spec(
            "one",
            &arm,
            &ns,
            &mode,
            1,
            FailurePolicy::Required,
        )];
        let result = execute_hybrid(
            request(&arms, &ns, "all", 1, 1),
            &Hydrator {
                trace: &trace,
                flow,
            },
            || Ok(concat),
        )
        .await;
        match (flow, result) {
            (Flow::FailOutcomes, Err(ExecutorError::OutcomePolicy(error))) => {
                assert_eq!(error.code, 31)
            }
            (Flow::FailPreparation, Err(ExecutorError::Preparation(error))) => {
                assert_eq!(error.code, 32)
            }
            (Flow::FailHydration, Err(ExecutorError::Hydration(error))) => {
                assert_eq!(error.code, 33)
            }
            _ => panic!("typed callback error was lost or moved to the wrong phase"),
        }
    }
}

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
