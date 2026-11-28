use std::num::NonZeroUsize;
use std::sync::Mutex;

use async_trait::async_trait;
use khive_score::DeterministicScore;
use khive_storage::TextQueryMode;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use super::*;
use crate::{FilterPredicate, FuseStrategy, FusionStrategy, QueryNode, RerankMethod};

struct Label {
    metadata: Value,
    signals: [f64; 2],
}
type Row = Scored<u32, Label>;
type Error = ExecutorError<&'static str, usize>;
const NS: &[&str] = &["fixture"];
const MODE: TextQueryMode = TextQueryMode::Plain;

fn row(id: u32, score: f64, pick: Value, signal: f64) -> Row {
    (
        id,
        DeterministicScore::from_f64(score),
        Label {
            metadata: json!({"pick":pick}),
            signals: [score, signal],
        },
    )
}
fn ok<T>(result: Result<T, Error>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("unexpected interpreter refusal"),
    }
}

#[derive(Debug, PartialEq)]
enum Input {
    Vector(String, String, Vec<f32>, u32),
    Keyword(String, u32),
}
struct Arm {
    rows: Mutex<Option<Vec<Row>>>,
    seen: Mutex<Vec<Input>>,
}
impl Arm {
    fn new(rows: Vec<Row>) -> Self {
        Self {
            rows: Mutex::new(Some(rows)),
            seen: Mutex::new(Vec::new()),
        }
    }
}
#[async_trait]
impl ExecutorArm for Arm {
    type Id = u32;
    type Label = Label;
    type Evidence = usize;
    type Filter = ();
    type Error = &'static str;
    async fn search<'a>(
        &'a self,
        request: ArmRequest<'a, ()>,
    ) -> Result<ArmCompletion<u32, Label, usize>, ArmFailure<&'static str, usize>> {
        assert_eq!(request.scope.namespaces, NS);
        let seen = match request.input {
            ArmInput::Vector {
                engine,
                model_id,
                input: VectorInput::Embedding(embedding),
                ..
            } => Input::Vector(
                (*engine).into(),
                (*model_id).into(),
                embedding.to_vec(),
                request.candidate_limit,
            ),
            ArmInput::Text { query, .. } => {
                Input::Keyword((*query).into(), request.candidate_limit)
            }
            _ => panic!("unprepared text reached provider"),
        };
        self.seen.lock().unwrap().push(seen);
        Ok(ArmCompletion {
            hits: self.rows.lock().unwrap().take().expect("one provider call"),
            route: ArmRoute::Primary,
            evidence: 17,
        })
    }
}
fn spec<'a>(arm: &'a Arm, key: &'a str, vector: bool, limit: u32) -> ArmSpec<'a, Arm> {
    ArmSpec {
        key,
        adapter: arm,
        scope: SearchScope {
            namespaces: NS,
            filter: &(),
        },
        input: Some(if vector {
            ArmInput::Vector {
                engine: "engine-a",
                model_id: "model-a",
                space_id: Some("space-a"),
                input: VectorInput::Embedding(&[9.0]),
            }
        } else {
            ArmInput::Text {
                query: "binding text",
                mode: &MODE,
                snippet_chars: 0,
            }
        }),
        candidate_limit: limit,
        failure: FailurePolicy::Required,
    }
}
fn request<'a>(
    arms: &'a [ArmSpec<'a, Arm>],
    vector_slots: &'a [usize],
    limit: usize,
) -> ExecutorRequest<'a, Arm> {
    ExecutorRequest {
        operation: "interpreter.fixture",
        scope: SearchScope {
            namespaces: NS,
            filter: &(),
        },
        arms,
        max_in_flight: NonZeroUsize::new(2).unwrap(),
        vector_slots,
        vector_preparer: None,
        max_candidates: arms.iter().map(|arm| arm.candidate_limit as usize).sum(),
        final_limit: limit,
    }
}

struct Hydrate;
#[async_trait]
impl CandidateHydrator for Hydrate {
    type Id = u32;
    type Label = Label;
    type Evidence = usize;
    type Filter = ();
    type Item = ();
    type State = ();
    type Error = &'static str;
    fn validate_outcomes(
        &self,
        collected: HydratorArms<Self>,
    ) -> Result<HydratorArms<Self>, &'static str> {
        Ok(collected)
    }
    async fn prepare_arms<'a>(
        &'a self,
        _: SearchScope<'a, ()>,
        arms: Vec<Vec<Row>>,
    ) -> Result<PreparedArms<u32, Label, ()>, &'static str> {
        Ok(PreparedArms { arms, state: () })
    }
    async fn hydrate_filter_rank<'a>(
        &'a self,
        _: SearchScope<'a, ()>,
        _: (),
        rows: Vec<Row>,
    ) -> Result<Vec<HydratedHit<u32, Label, ()>>, &'static str> {
        Ok(rows
            .into_iter()
            .map(|(id, score, label)| HydratedHit {
                id,
                score,
                label,
                item: (),
            })
            .collect())
    }
}

#[derive(Default)]
struct Operators {
    trace: Mutex<Vec<String>>,
    reranked: Mutex<Vec<Vec<u32>>>,
    drop_rerank_row: bool,
}
#[async_trait]
impl PlanOperators for Operators {
    type Id = u32;
    type Label = Label;
    type Error = &'static str;
    type Fusion = FusionStrategy;
    type Reranker = RerankMethod;
    fn resolve_fusion(&self, strategy: &FusionStrategy) -> Result<FusionStrategy, &'static str> {
        self.trace.lock().unwrap().push("resolve fusion".into());
        if matches!(strategy, FusionStrategy::Custom { name, .. } if name != "reverse-children") {
            return Err("unknown fusion");
        }
        Ok(strategy.clone())
    }
    fn resolve_rerank(&self, method: &RerankMethod) -> Result<RerankMethod, &'static str> {
        self.trace.lock().unwrap().push("resolve rerank".into());
        if matches!(method, RerankMethod::CrossEncoder { model } if model != "literal-model") {
            return Err("unknown reranker");
        }
        Ok(method.clone())
    }
    fn metadata_equals(
        &self,
        _: &u32,
        label: &Label,
        field: &str,
        value: &Value,
    ) -> Result<bool, &'static str> {
        self.trace.lock().unwrap().push("metadata".into());
        if field == "refuse" {
            return Err("metadata refusal");
        }
        Ok(label.metadata.get(field) == Some(value))
    }
    async fn fuse(
        &self,
        resolved: FusionStrategy,
        mut children: Vec<Vec<Row>>,
        top_k: usize,
    ) -> Result<Vec<Row>, &'static str> {
        self.trace.lock().unwrap().push("fuse".into());
        let custom = matches!(&resolved, FusionStrategy::Custom { .. });
        match resolved {
            FusionStrategy::Weighted { weights } => {
                for (rows, weight) in children
                    .iter_mut()
                    .zip(khive_fusion::normalize_weights(&weights))
                {
                    for row in rows {
                        row.1 = DeterministicScore::from_f64(row.1.to_f64() * weight);
                    }
                }
            }
            FusionStrategy::Custom { name, params } => {
                assert_eq!(name, "reverse-children");
                assert_eq!(
                    params,
                    json!({"preserve":true}),
                    "custom parameter payload must arrive unchanged"
                );
                children.reverse();
            }
            FusionStrategy::VectorOnly | FusionStrategy::KeywordOnly | FusionStrategy::Union => {}
            _ => return Err("unexpected fixture fusion"),
        }
        let mut rows: Vec<_> = children.into_iter().flatten().collect();
        if !custom {
            rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        }
        rows.truncate(top_k);
        Ok(rows)
    }
    async fn rerank(
        &self,
        resolved: RerankMethod,
        method: &RerankMethod,
        mut rows: Vec<Row>,
    ) -> Result<Vec<Row>, &'static str> {
        self.trace.lock().unwrap().push("rerank".into());
        assert_eq!(
            serde_json::to_value(&resolved).unwrap(),
            serde_json::to_value(method).unwrap()
        );
        self.reranked
            .lock()
            .unwrap()
            .push(rows.iter().map(|row| row.0).collect());
        for row in &mut rows {
            let score = match &resolved {
                RerankMethod::ScoreWeighted { weights } => row
                    .2
                    .signals
                    .iter()
                    .zip(weights)
                    .map(|(signal, weight)| signal * weight)
                    .sum(),
                RerankMethod::CrossEncoder { model } => {
                    assert_eq!(model, "literal-model");
                    if row.0 == 3 {
                        0.97
                    } else {
                        0.1
                    }
                }
            };
            row.1 = DeterministicScore::from_f64(score);
        }
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if self.drop_rerank_row {
            rows.pop();
        }
        Ok(rows)
    }
}

#[tokio::test]
async fn leaf_interpreter_uses_node_inputs_bounds_and_keeps_raw_accounting() {
    let arm = Arm::new(vec![
        row(1, 0.2, json!(false), 0.2),
        row(2, 0.8, json!(true), 0.8),
    ]);
    let vector_spec = spec(&arm, "vector", true, 2);
    let permits = Semaphore::new(1);
    let leaf = ok(execute_leaf(
        LeafExecutionContext {
            operation: "leaf.fixture",
            permits: &permits,
        },
        LeafSlot(4),
        &vector_spec,
        PreparedLeaf {
            node: QueryNode::Vector {
                embedding: vec![0.25, 0.75],
                top_k: 2,
                min_score: Some(DeterministicScore::from_f64(0.5)),
            },
            preparation: Some(23),
        },
    )
    .await);
    assert_eq!(
        *arm.seen.lock().unwrap(),
        [Input::Vector(
            "engine-a".into(),
            "model-a".into(),
            vec![0.25, 0.75],
            2
        )],
        "provider must consume the literal prepared node"
    );
    assert_eq!(
        leaf.hits().iter().map(|row| row.0).collect::<Vec<_>>(),
        [2],
        "leaf threshold must affect actual rows"
    );
    assert_eq!(
        leaf.report().returned,
        2,
        "raw accounting must precede threshold"
    );
    assert_eq!(leaf.report().preparation, Some(23));
    assert!(
        matches!(leaf.node(), QueryNode::Vector { embedding,top_k:2,.. } if embedding==&[0.25,0.75]),
        "returned node must be the consumed leaf"
    );
    let text = Arm::new(vec![]);
    let spec = spec(&text, "text", false, 0);
    let leaf = ok(execute_leaf(
        LeafExecutionContext {
            operation: "leaf.fixture",
            permits: &permits,
        },
        LeafSlot(0),
        &spec,
        PreparedLeaf {
            node: QueryNode::keyword("node text", 0),
            preparation: None,
        },
    )
    .await);
    assert_eq!(
        *text.seen.lock().unwrap(),
        [Input::Keyword("node text".into(), 0)],
        "zero-limit selected text still executes exact node"
    );
    assert!(matches!(leaf.report().state, ArmState::Completed { .. }));
}

async fn collected_leaf(
    slot: usize,
    key: &str,
    id: u32,
    empty: bool,
) -> ExecutedLeaf<u32, Label, usize, &'static str> {
    let arm = Arm::new(if empty {
        vec![]
    } else {
        vec![row(id, 0.5, json!(true), 0.5)]
    });
    let spec = spec(&arm, key, false, 1);
    let permits = Semaphore::new(1);
    ok(execute_leaf(
        LeafExecutionContext {
            operation: "collect.fixture",
            permits: &permits,
        },
        LeafSlot(slot),
        &spec,
        PreparedLeaf {
            node: if empty {
                QueryNode::Empty
            } else {
                QueryNode::keyword(key, 1)
            },
            preparation: None,
        },
    )
    .await)
}
#[tokio::test]
async fn raw_collection_preserves_declared_order_duplicate_ids_and_skipped_slots() {
    let declared = CollectionPlan::new(vec![
        CollectionSlot {
            slot: LeafSlot(3),
            key: "first".into(),
            kind: LeafKind::Keyword,
        },
        CollectionSlot {
            slot: LeafSlot(7),
            key: "skip".into(),
            kind: LeafKind::Keyword,
        },
        CollectionSlot {
            slot: LeafSlot(8),
            key: "last".into(),
            kind: LeafKind::Keyword,
        },
    ])
    .unwrap();
    let first = collected_leaf(3, "first", 9, false).await;
    let skipped = collected_leaf(7, "skip", 0, true).await;
    let last = collected_leaf(8, "last", 9, false).await;
    let out = ok(finish_collect(
        &declared,
        vec![last, skipped, first],
        Some(51),
    ));
    assert_eq!(
        out.collected
            .arms
            .iter()
            .map(|rows| rows.iter().map(|r| r.0).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [vec![9], vec![], vec![9]],
        "Collect must preserve declared raw slots and duplicate identities"
    );
    assert_eq!(
        out.collected
            .reports
            .iter()
            .map(|r| r.key.as_str())
            .collect::<Vec<_>>(),
        ["first", "skip", "last"],
        "Collect must preserve declared report order"
    );
    assert_eq!(out.collected.vector_preparation, Some(51));
    assert!(
        matches!(out.plan,QueryNode::Collect { children } if matches!(&children[0],QueryNode::Keyword{text,..} if text=="first") && matches!(&children[1],QueryNode::Empty) && matches!(&children[2],QueryNode::Keyword{text,..} if text=="last"))
    );
    let duplicate = vec![
        collected_leaf(3, "first", 9, false).await,
        collected_leaf(3, "first", 9, false).await,
    ];
    assert!(matches!(
        finish_collect(&declared, duplicate, None),
        Err(ExecutorError::Invalid("duplicate executed collection slot"))
    ));
    assert!(matches!(
        finish_collect::<u32, Label, usize, &'static str>(&declared, vec![], None),
        Err(ExecutorError::Invalid(
            "executed collection slot count differs from declaration"
        ))
    ));
}

fn composed(method: RerankMethod) -> PlanTemplate {
    PlanTemplate::Rerank {
        child: Box::new(PlanTemplate::Filter {
            child: Box::new(PlanTemplate::Fuse {
                children: vec![
                    PlanTemplate::Leaf(LeafSlot(0)),
                    PlanTemplate::Leaf(LeafSlot(1)),
                ],
                strategy: FuseStrategy::Weighted {
                    weights: vec![1.0, 2.0],
                },
                top_k: 4,
            }),
            predicate: FilterPredicate::And(vec![
                FilterPredicate::MinScore(DeterministicScore::from_f64(0.05)),
                FilterPredicate::Or(vec![
                    FilterPredicate::TopK(1),
                    FilterPredicate::MetadataEquals {
                        field: "pick".into(),
                        value: json!(true),
                    },
                ]),
            ]),
        }),
        method,
        top_k: 1,
    }
}
async fn run_composed(method: RerankMethod, expected: u32, score: f64) {
    let a = Arm::new(vec![
        row(1, 0.9, json!(false), 0.1),
        row(2, 0.8, json!("true"), 0.99),
    ]);
    let b = Arm::new(vec![
        row(3, 0.2, json!(true), 0.4),
        row(4, 0.1, json!(true), 0.95),
    ]);
    let arms = [spec(&a, "text", false, 2), spec(&b, "vector", true, 2)];
    let ops = Operators::default();
    let out = ok(execute_hybrid(
        request(&arms, &[1], 4),
        &Hydrate,
        PlanProgram {
            template: composed(method.clone()),
            operators: &ops,
        },
    )
    .await);
    assert_eq!(
        out.hits.iter().map(|h| h.id).collect::<Vec<_>>(),
        [expected],
        "actual filter and reranker must determine the literal winner"
    );
    assert_eq!(
        out.hits[0].score,
        DeterministicScore::from_f64(score),
        "actual reranker score must survive"
    );
    assert_eq!(
        *ops.reranked.lock().unwrap(),
        [vec![1, 3, 4]],
        "filter must use typed metadata and original ranks before all-row reranking"
    );
    assert_eq!(
        *ops.trace.lock().unwrap(),
        [
            "resolve rerank",
            "resolve fusion",
            "fuse",
            "metadata",
            "metadata",
            "metadata",
            "rerank"
        ]
    );
    // Independent fixed-fixture route: call providers and concrete operators directly.
    let direct_text = Arm::new(vec![
        row(1, 0.9, json!(false), 0.1),
        row(2, 0.8, json!("true"), 0.99),
    ]);
    let direct_vector = Arm::new(vec![
        row(3, 0.2, json!(true), 0.4),
        row(4, 0.1, json!(true), 0.95),
    ]);
    let text_input = ArmInput::Text {
        query: "binding text",
        mode: &MODE,
        snippet_chars: 0,
    };
    let vector_input = ArmInput::Vector {
        engine: "engine-a",
        model_id: "model-a",
        space_id: Some("space-a"),
        input: VectorInput::Embedding(&[9.0]),
    };
    let mut streams = Vec::new();
    for (provider, input) in [(&direct_text, &text_input), (&direct_vector, &vector_input)] {
        let result = provider
            .search(ArmRequest {
                scope: SearchScope {
                    namespaces: NS,
                    filter: &(),
                },
                input,
                candidate_limit: 2,
            })
            .await;
        let rows = match result {
            Ok(completion) => completion.hits,
            Err(_) => panic!("direct fixture provider refused"),
        };
        streams.push(rows);
    }
    let direct_ops = Operators::default();
    let mut direct = direct_ops
        .fuse(
            FusionStrategy::Weighted {
                weights: vec![1.0, 2.0],
            },
            streams,
            4,
        )
        .await
        .expect("direct fixed fusion");
    direct.retain(|row| [1, 3, 4].contains(&row.0));
    direct = direct_ops
        .rerank(method.clone(), &method, direct)
        .await
        .expect("direct fixed reranking");
    direct.truncate(1);
    assert_eq!(
        out.hits
            .iter()
            .map(|hit| (hit.id, hit.score))
            .collect::<Vec<_>>(),
        direct.iter().map(|row| (row.0, row.1)).collect::<Vec<_>>(),
        "interpreted results must match direct fixed-fixture composition"
    );
    assert_eq!(*a.seen.lock().unwrap(), *direct_text.seen.lock().unwrap());
    assert_eq!(*b.seen.lock().unwrap(), *direct_vector.seen.lock().unwrap());
    assert_eq!(out.plan.depth(), 4);
    assert_eq!(out.plan.leaf_count(), 2);
    assert!(matches!(out.plan, QueryNode::Rerank { top_k: 1, .. }));
}
#[tokio::test]
async fn composed_weighted_rerank_filters_before_all_rows_then_truncates() {
    run_composed(
        RerankMethod::ScoreWeighted {
            weights: vec![0.0, 1.0],
        },
        4,
        0.95,
    )
    .await;
}
#[tokio::test]
async fn composed_cross_encoder_dispatches_exact_model_and_uses_its_scores() {
    run_composed(
        RerankMethod::CrossEncoder {
            model: "literal-model".into(),
        },
        3,
        0.97,
    )
    .await;
}

#[tokio::test]
async fn resolution_precedes_invalid_request_and_missing_text_preparer() {
    let arm = Arm::new(vec![]);
    let mut spec = spec(&arm, "vector", true, 1);
    spec.input = Some(ArmInput::Vector {
        engine: "engine-a",
        model_id: "model-a",
        space_id: None,
        input: VectorInput::Text("embed me"),
    });
    let arms = [spec];
    let ops = Operators::default();
    let mut req = request(&arms, &[0], 0);
    req.max_candidates = 0;
    let out = execute_hybrid(
        req,
        &Hydrate,
        PlanProgram {
            template: PlanTemplate::Fuse {
                children: vec![PlanTemplate::Leaf(LeafSlot(0))],
                strategy: FuseStrategy::Custom {
                    name: "unregistered".into(),
                    params: Value::Null,
                },
                top_k: 0,
            },
            operators: &ops,
        },
    )
    .await;
    assert!(
        matches!(out, Err(ExecutorError::FusionResolution("unknown fusion"))),
        "resolver refusal must precede invalid candidate bound and text-preparer refusal"
    );
    let out = execute_hybrid(
        request(&arms, &[0], 0),
        &Hydrate,
        PlanProgram {
            template: PlanTemplate::Leaf(LeafSlot(0)),
            operators: &ops,
        },
    )
    .await;
    assert!(
        matches!(out, Err(ExecutorError::MissingVectorPreparer { slot: 0 })),
        "selected text vector requires a preparer before any provider call"
    );
    assert!(arm.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn scalar_entries_refuse_collect_and_duplicate_slots_before_provider_work() {
    for template in [
        PlanTemplate::Collect {
            children: vec![PlanTemplate::Leaf(LeafSlot(0))],
        },
        PlanTemplate::Filter {
            child: Box::new(PlanTemplate::Collect {
                children: vec![PlanTemplate::Leaf(LeafSlot(0))],
            }),
            predicate: FilterPredicate::TopK(1),
        },
        PlanTemplate::Fuse {
            children: vec![
                PlanTemplate::Leaf(LeafSlot(0)),
                PlanTemplate::Leaf(LeafSlot(0)),
            ],
            strategy: FuseStrategy::Union,
            top_k: 1,
        },
        PlanTemplate::Empty,
    ] {
        let arm = Arm::new(vec![]);
        let arms = [spec(&arm, "text", false, 1)];
        let ops = Operators::default();
        assert!(
            matches!(
                execute_hybrid(
                    request(&arms, &[], 1),
                    &Hydrate,
                    PlanProgram {
                        template,
                        operators: &ops
                    }
                )
                .await,
                Err(ExecutorError::Invalid(_))
            ),
            "invalid shape must refuse before providers"
        );
        assert!(arm.seen.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn reranker_cannot_drop_duplicate_identity_occurrences() {
    let arm = Arm::new(vec![
        row(7, 0.2, json!(true), 0.2),
        row(7, 0.3, json!(true), 0.3),
    ]);
    let arms = [spec(&arm, "text", false, 2)];
    let ops = Operators {
        drop_rerank_row: true,
        ..Default::default()
    };
    let out = execute_hybrid(
        request(&arms, &[], 1),
        &Hydrate,
        PlanProgram {
            template: PlanTemplate::Rerank {
                child: Box::new(PlanTemplate::Leaf(LeafSlot(0))),
                method: RerankMethod::ScoreWeighted {
                    weights: vec![0.0, 1.0],
                },
                top_k: 1,
            },
            operators: &ops,
        },
    )
    .await;
    assert!(
        matches!(
            out,
            Err(ExecutorError::Protocol {
                message: "reranking changed identity multiplicity",
                ..
            })
        ),
        "reranker must return every occurrence before truncation"
    );
}

#[tokio::test]
async fn strategy_selection_keeps_inactive_leaf_empty_without_provider_call() {
    for vector_only in [true, false] {
        let text = Arm::new(vec![row(1, 0.5, json!(true), 0.5)]);
        let vector = Arm::new(vec![row(2, 0.6, json!(true), 0.6)]);
        let arms = [
            spec(&text, "text", false, 1),
            spec(&vector, "vector", true, 1),
        ];
        let ops = Operators::default();
        let out = ok(execute_hybrid(
            request(&arms, &[1], 2),
            &Hydrate,
            PlanProgram {
                template: PlanTemplate::Fuse {
                    children: vec![
                        PlanTemplate::Leaf(LeafSlot(0)),
                        PlanTemplate::Leaf(LeafSlot(1)),
                    ],
                    strategy: if vector_only {
                        FuseStrategy::VectorOnly
                    } else {
                        FuseStrategy::KeywordOnly
                    },
                    top_k: 2,
                },
                operators: &ops,
            },
        )
        .await);
        assert_eq!(
            out.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            [if vector_only { 2 } else { 1 }],
            "strategy must suppress the inactive physical provider"
        );
        assert_eq!(text.seen.lock().unwrap().len(), usize::from(!vector_only));
        assert_eq!(vector.seen.lock().unwrap().len(), usize::from(vector_only));
        let QueryNode::Fuse { children, .. } = out.plan else {
            panic!("executed fusion missing")
        };
        assert!(matches!(
            &children[usize::from(!vector_only)],
            QueryNode::Empty
        ));
    }
}

#[tokio::test]
async fn custom_fusion_receives_actual_name_parameters_and_ordered_children() {
    let first = Arm::new(vec![row(1, 0.9, json!(true), 0.9)]);
    let second = Arm::new(vec![row(2, 0.1, json!(true), 0.1)]);
    let arms = [
        spec(&first, "first", false, 1),
        spec(&second, "second", false, 1),
    ];
    let ops = Operators::default();
    let out = ok(execute_hybrid(
        request(&arms, &[], 2),
        &Hydrate,
        PlanProgram {
            template: PlanTemplate::Fuse {
                children: vec![
                    PlanTemplate::Leaf(LeafSlot(0)),
                    PlanTemplate::Leaf(LeafSlot(1)),
                ],
                strategy: FuseStrategy::Custom {
                    name: "reverse-children".into(),
                    params: json!({"preserve":true}),
                },
                top_k: 2,
            },
            operators: &ops,
        },
    )
    .await);
    assert_eq!(
        out.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        [2, 1],
        "actual named custom operation must determine child order"
    );
    assert_eq!(
        out.hits.iter().map(|hit| hit.score).collect::<Vec<_>>(),
        [
            DeterministicScore::from_f64(0.1),
            DeterministicScore::from_f64(0.9)
        ]
    );
    assert!(
        matches!(out.plan,QueryNode::Fuse {strategy:FuseStrategy::Custom {name,params},..} if name=="reverse-children" && params==json!({"preserve":true}))
    );
}

#[tokio::test]
async fn leaf_mismatch_refuses_before_callback_and_empty_collection_is_real() {
    let arm = Arm::new(vec![]);
    let spec = spec(&arm, "text", false, 1);
    let permits = Semaphore::new(1);
    let out = execute_leaf(
        LeafExecutionContext {
            operation: "leaf.fixture",
            permits: &permits,
        },
        LeafSlot(0),
        &spec,
        PreparedLeaf {
            node: QueryNode::keyword("node", 2),
            preparation: None,
        },
    )
    .await;
    assert!(matches!(
        out,
        Err(ExecutorError::Invalid(
            "leaf limit differs from declared candidate limit"
        ))
    ));
    assert!(arm.seen.lock().unwrap().is_empty());
    let plan = CollectionPlan::new(vec![]).unwrap();
    let out = ok(finish_collect::<u32, Label, usize, &'static str>(
        &plan,
        vec![],
        None,
    ));
    assert!(out.collected.arms.is_empty());
    assert!(matches!(out.plan,QueryNode::Collect {children} if children.is_empty()));
}

#[tokio::test]
async fn filter_boolean_identities_short_circuit_and_errors_remain_typed() {
    let cases = [
        (FilterPredicate::And(vec![]), true, false),
        (FilterPredicate::Or(vec![]), false, false),
        (
            FilterPredicate::MinScore(DeterministicScore::from_f64(0.6)),
            false,
            false,
        ),
        (
            FilterPredicate::Or(vec![
                FilterPredicate::TopK(1),
                FilterPredicate::MetadataEquals {
                    field: "refuse".into(),
                    value: json!(true),
                },
            ]),
            true,
            false,
        ),
        (
            FilterPredicate::And(vec![
                FilterPredicate::TopK(0),
                FilterPredicate::MetadataEquals {
                    field: "refuse".into(),
                    value: json!(true),
                },
            ]),
            false,
            false,
        ),
        (
            FilterPredicate::MetadataEquals {
                field: "refuse".into(),
                value: json!(true),
            },
            false,
            true,
        ),
    ];
    for (predicate, kept, refuses) in cases {
        let arm = Arm::new(vec![row(1, 0.5, json!(true), 0.5)]);
        let arms = [spec(&arm, "text", false, 1)];
        let ops = Operators::default();
        let out = execute_hybrid(
            request(&arms, &[], 1),
            &Hydrate,
            PlanProgram {
                template: PlanTemplate::Filter {
                    child: Box::new(PlanTemplate::Leaf(LeafSlot(0))),
                    predicate,
                },
                operators: &ops,
            },
        )
        .await;
        if refuses {
            assert!(matches!(
                out,
                Err(ExecutorError::Metadata("metadata refusal"))
            ));
        } else {
            assert_eq!(
                ok(out).hits.len(),
                usize::from(kept),
                "predicate identities and short circuit must affect actual retained rows"
            );
        }
    }
}

#[tokio::test]
async fn raw_collection_overfetch_preserves_logical_request_and_scalar_limit() {
    let permits = Semaphore::new(1);
    let rows = || {
        vec![
            row(1, 0.9, json!(true), 0.9),
            row(2, 0.8, json!(true), 0.8),
            row(3, 0.7, json!(true), 0.7),
        ]
    };
    let strict = Arm::new(rows());
    let raw = Arm::new(rows());
    let node = || QueryNode::vector(vec![0.25, 0.75], 2);
    let strict_out = execute_leaf(
        LeafExecutionContext {
            operation: "leaf.fixture",
            permits: &permits,
        },
        LeafSlot(0),
        &spec(&strict, "vector", true, 2),
        PreparedLeaf {
            node: node(),
            preparation: None,
        },
    )
    .await;
    assert!(
        matches!(
            strict_out,
            Err(ExecutorError::Protocol {
                slot: Some(0),
                message: "arm exceeded its candidate limit"
            })
        ),
        "scalar leaf must retain its strict returned-row bound"
    );
    let raw_out = execute_collect_leaf(
        LeafExecutionContext {
            operation: "leaf.fixture",
            permits: &permits,
        },
        LeafSlot(0),
        &spec(&raw, "vector", true, 2),
        PreparedLeaf {
            node: node(),
            preparation: None,
        },
    )
    .await;
    assert!(
        raw_out.is_ok(),
        "raw collection must accept provider overfetch without truncation"
    );
    let raw_out = ok(raw_out);
    assert_eq!(
        raw_out.report().requested,
        2,
        "raw collection must retain its logical requested limit"
    );
    assert_eq!(
        raw_out.report().returned,
        3,
        "raw collection must report every provider row"
    );
    assert_eq!(
        raw_out.hits().iter().map(|row| row.0).collect::<Vec<_>>(),
        [1, 2, 3],
        "raw collection must preserve overfetch rows in provider order"
    );
    assert_eq!(raw_out.node().top_k(), 2);
    assert_eq!(
        *raw.seen.lock().unwrap(),
        *strict.seen.lock().unwrap(),
        "raw and scalar leaves must send the same actual provider request"
    );
    let declared = CollectionPlan::new(vec![CollectionSlot {
        slot: LeafSlot(0),
        key: "vector".into(),
        kind: LeafKind::Vector,
    }])
    .unwrap();
    let collected = ok(finish_collect(&declared, vec![raw_out], None));
    assert_eq!(collected.collected.arms[0].len(), 3);
    assert_eq!(collected.plan.top_k(), 2);
}
