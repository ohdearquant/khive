use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use async_trait::async_trait;
use khive_storage::{await_request_read_phase, ensure_request_read_active};
use tokio::sync::Semaphore;

use crate::{FilterPredicate, FuseStrategy, FusionStrategy, QueryNode, RerankMethod};

use super::types::*;

/// Stable identity of one declared provider slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LeafSlot(pub usize);

/// Provider family declared before work, independently of an eventual skipped leaf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeafKind {
    /// A prepared vector search.
    Vector,
    /// A lexical search.
    Keyword,
}

/// One slot declared before its provider jobs start.
#[derive(Clone, Debug)]
pub struct CollectionSlot {
    /// Stable slot identity; values need not be contiguous.
    pub slot: LeafSlot,
    /// Nonempty report key, unique within this collection.
    pub key: String,
    /// Authorized provider family.
    pub kind: LeafKind,
}

/// Immutable declaration of raw output order, without ranking or deduplication.
#[derive(Clone, Debug)]
pub struct CollectionPlan {
    slots: Vec<CollectionSlot>,
}

impl CollectionPlan {
    /// Validate unique slot identities and keys before launching the declared jobs.
    pub fn new(slots: Vec<CollectionSlot>) -> Result<Self, &'static str> {
        let mut ids = HashSet::new();
        let mut keys = HashSet::new();
        for slot in &slots {
            if !ids.insert(slot.slot) || slot.key.is_empty() || !keys.insert(slot.key.as_str()) {
                return Err("collection slots and nonempty keys must be unique");
            }
        }
        Ok(Self { slots })
    }

    /// Declarations in the order returned by [`finish_collect`].
    pub fn slots(&self) -> &[CollectionSlot] {
        &self.slots
    }
}

/// One immutable post-embedding leaf and its original preparation evidence.
pub struct PreparedLeaf<D> {
    /// Only Vector, Keyword or Empty is accepted by [`execute_leaf`].
    pub node: QueryNode,
    /// Original embedding evidence, including a withheld embedding.
    pub preparation: Option<D>,
}

/// Borrowed request context shared with the caller's existing scheduler.
pub struct LeafExecutionContext<'a> {
    /// Diagnostic operation for request-stop errors.
    pub operation: &'static str,
    /// Existing shared concurrency bound; no work is spawned by the interpreter.
    pub permits: &'a Semaphore,
}

/// Sealed result of an actual interpreted leaf; callers cannot fabricate its provenance.
pub struct ExecutedLeaf<I, L, D, E> {
    slot: LeafSlot,
    node: QueryNode,
    hits: Vec<Scored<I, L>>,
    report: ArmReport<D, E>,
}

impl<I, L, D, E> ExecutedLeaf<I, L, D, E> {
    /// The declared slot that executed.
    pub fn slot(&self) -> LeafSlot {
        self.slot
    }
    /// The exact immutable node consumed by the interpreter.
    pub fn node(&self) -> &QueryNode {
        &self.node
    }
    /// Surviving rows in provider order, after the optional leaf score threshold.
    pub fn hits(&self) -> &[Scored<I, L>] {
        &self.hits
    }
    /// Raw accounting before the optional leaf score threshold.
    pub fn report(&self) -> &ArmReport<D, E> {
        &self.report
    }
}

/// Exact executed raw collection and its original ordered streams/evidence.
pub struct CollectedPlanOutcome<I, L, D, E> {
    /// Root-only Collect tree containing the consumed leaf nodes.
    pub plan: QueryNode,
    /// Raw streams in declared order, with duplicates preserved.
    pub collected: CollectedArms<I, L, D, E>,
}

/// Interpret the node's input, limit and threshold before retaining its execution evidence.
///
/// Scope, physical model/engine, lexical mode and failure policy stay in `spec`.
/// This method inherits the caller's request context and never spawns or changes deadlines.
pub async fn execute_leaf<A: ExecutorArm + ?Sized>(
    context: LeafExecutionContext<'_>,
    slot: LeafSlot,
    spec: &ArmSpec<'_, A>,
    prepared: PreparedLeaf<A::Evidence>,
) -> Result<
    ExecutedLeaf<A::Id, A::Label, A::Evidence, A::Error>,
    ExecutorError<A::Error, A::Evidence>,
> {
    execute_leaf_with_policy(context, slot, spec, prepared, OutputBound::Strict).await
}

/// Interpret a raw collection leaf while retaining the provider's existing overfetch rows.
///
/// The node still supplies the logical requested limit and exact input. This entry point
/// permits an adapter's existing raw output to exceed that limit without truncating it;
/// reports retain the actual returned count. It is only for [`finish_collect`] consumers.
/// Scalar executors always use [`execute_leaf`], which retains the strict output bound.
pub async fn execute_collect_leaf<A: ExecutorArm + ?Sized>(
    context: LeafExecutionContext<'_>,
    slot: LeafSlot,
    spec: &ArmSpec<'_, A>,
    prepared: PreparedLeaf<A::Evidence>,
) -> Result<
    ExecutedLeaf<A::Id, A::Label, A::Evidence, A::Error>,
    ExecutorError<A::Error, A::Evidence>,
> {
    execute_leaf_with_policy(context, slot, spec, prepared, OutputBound::RawCollection).await
}

#[derive(Clone, Copy)]
enum OutputBound {
    Strict,
    RawCollection,
}

async fn execute_leaf_with_policy<A: ExecutorArm + ?Sized>(
    context: LeafExecutionContext<'_>,
    slot: LeafSlot,
    spec: &ArmSpec<'_, A>,
    prepared: PreparedLeaf<A::Evidence>,
    output_bound: OutputBound,
) -> Result<
    ExecutedLeaf<A::Id, A::Label, A::Evidence, A::Error>,
    ExecutorError<A::Error, A::Evidence>,
> {
    let operation = context.operation;
    ensure_request_read_active(operation).map_err(ExecutorError::RequestStopped)?;
    if operation.is_empty() || spec.key.is_empty() {
        return Err(ExecutorError::Invalid(
            "leaf operation and key must not be empty",
        ));
    }
    let PreparedLeaf { node, preparation } = prepared;
    let (input, minimum) = match (&node, &spec.input) {
        (QueryNode::Empty, _) => (None, None),
        (
            QueryNode::Vector {
                embedding,
                top_k,
                min_score,
            },
            Some(ArmInput::Vector {
                engine,
                model_id,
                space_id,
                ..
            }),
        ) => {
            if engine.is_empty() || model_id.is_empty() || space_id.is_some_and(str::is_empty) {
                return Err(ExecutorError::Invalid(
                    "vector bindings must not contain empty identifiers",
                ));
            }
            if *top_k != spec.candidate_limit as usize {
                return Err(ExecutorError::Invalid(
                    "leaf limit differs from declared candidate limit",
                ));
            }
            (
                Some(ArmInput::Vector {
                    engine,
                    model_id,
                    space_id: *space_id,
                    input: VectorInput::Embedding(embedding),
                }),
                *min_score,
            )
        }
        (
            QueryNode::Keyword {
                text,
                top_k,
                min_score,
            },
            Some(ArmInput::Text {
                mode,
                snippet_chars,
                ..
            }),
        ) => {
            if *top_k != spec.candidate_limit as usize {
                return Err(ExecutorError::Invalid(
                    "leaf limit differs from declared candidate limit",
                ));
            }
            (
                Some(ArmInput::Text {
                    query: text,
                    mode,
                    snippet_chars: *snippet_chars,
                }),
                *min_score,
            )
        }
        _ => {
            return Err(ExecutorError::Invalid(
                "prepared leaf does not match its provider family",
            ))
        }
    };
    let Some(input) = input.filter(|_| !spec.scope.namespaces.is_empty()) else {
        return Ok(ExecutedLeaf {
            slot,
            node: QueryNode::Empty,
            hits: Vec::new(),
            report: ArmReport {
                key: spec.key.to_owned(),
                requested: spec.candidate_limit,
                returned: 0,
                preparation,
                state: ArmState::Skipped,
            },
        });
    };
    let permit = await_request_read_phase(operation, context.permits.acquire())
        .await
        .map_err(ExecutorError::RequestStopped)?
        .map_err(|_| ExecutorError::Invalid("leaf semaphore is closed"))?;
    let result = await_request_read_phase(
        operation,
        spec.adapter.search(ArmRequest {
            scope: SearchScope {
                namespaces: spec.scope.namespaces,
                filter: spec.scope.filter,
            },
            input: &input,
            candidate_limit: node.top_k() as u32,
        }),
    )
    .await
    .map_err(ExecutorError::RequestStopped)?;
    drop(permit);
    let (mut hits, state) = match result {
        Ok(completion) => {
            if matches!(output_bound, OutputBound::Strict)
                && completion.hits.len() > spec.candidate_limit as usize
            {
                return Err(ExecutorError::Protocol {
                    slot: Some(slot.0),
                    message: "arm exceeded its candidate limit",
                });
            }
            (
                completion.hits,
                ArmState::Completed {
                    route: completion.route,
                    evidence: completion.evidence,
                },
            )
        }
        Err(ArmFailure::Ordinary(error)) => match spec.failure {
            FailurePolicy::Required => {
                return Err(ExecutorError::Arm {
                    slot: slot.0,
                    error,
                })
            }
            FailurePolicy::TolerateOrdinary => (Vec::new(), ArmState::Failed(error)),
        },
        Err(ArmFailure::Fatal(error)) => {
            return Err(ExecutorError::Arm {
                slot: slot.0,
                error,
            })
        }
        Err(ArmFailure::Unavailable(evidence)) => match spec.failure {
            FailurePolicy::Required => {
                return Err(ExecutorError::Unavailable {
                    slot: slot.0,
                    evidence,
                })
            }
            FailurePolicy::TolerateOrdinary => (Vec::new(), ArmState::Unavailable(evidence)),
        },
    };
    let returned = hits.len();
    if let Some(minimum) = minimum {
        hits.retain(|hit| hit.1 >= minimum);
    }
    Ok(ExecutedLeaf {
        slot,
        node,
        hits,
        report: ArmReport {
            key: spec.key.to_owned(),
            requested: spec.candidate_limit,
            returned,
            preparation,
            state,
        },
    })
}

/// Assemble sealed leaf results in declared order without ranking, deduplication or truncation.
pub fn finish_collect<I, L, D, E>(
    declared: &CollectionPlan,
    leaves: Vec<ExecutedLeaf<I, L, D, E>>,
    vector_preparation: Option<D>,
) -> Result<CollectedPlanOutcome<I, L, D, E>, ExecutorError<E, D>> {
    let mut by_slot = HashMap::new();
    for leaf in leaves {
        if by_slot.insert(leaf.slot, leaf).is_some() {
            return Err(ExecutorError::Invalid("duplicate executed collection slot"));
        }
    }
    if by_slot.len() != declared.slots.len() {
        return Err(ExecutorError::Invalid(
            "executed collection slot count differs from declaration",
        ));
    }
    let mut children = Vec::with_capacity(declared.slots.len());
    let mut arms = Vec::with_capacity(declared.slots.len());
    let mut reports = Vec::with_capacity(declared.slots.len());
    for expected in &declared.slots {
        let leaf = by_slot
            .remove(&expected.slot)
            .ok_or(ExecutorError::Invalid("missing declared collection slot"))?;
        if leaf.report.key != expected.key
            || !matches!(
                (&leaf.node, expected.kind),
                (QueryNode::Empty, _)
                    | (QueryNode::Vector { .. }, LeafKind::Vector)
                    | (QueryNode::Keyword { .. }, LeafKind::Keyword)
            )
        {
            return Err(ExecutorError::Invalid(
                "executed collection binding differs from declaration",
            ));
        }
        children.push(leaf.node);
        arms.push(leaf.hits);
        reports.push(leaf.report);
    }
    Ok(CollectedPlanOutcome {
        plan: QueryNode::Collect { children },
        collected: CollectedArms {
            arms,
            reports,
            vector_preparation,
        },
    })
}

/// Immutable operation structure whose leaves lower only after embedding preparation.
#[derive(Clone, Debug)]
pub enum PlanTemplate {
    /// One declared provider slot, appearing exactly once.
    Leaf(LeafSlot),
    /// No provider work and no scalar rows.
    Empty,
    /// A resolved fusion of ordered scalar child streams.
    Fuse {
        /// Ordered children.
        children: Vec<PlanTemplate>,
        /// Validated before providers.
        strategy: FuseStrategy,
        /// Maximum fused rows.
        top_k: usize,
    },
    /// Actual predicate application to the child stream.
    Filter {
        /// Scalar child.
        child: Box<PlanTemplate>,
        /// Predicates share the original child row's rank.
        predicate: FilterPredicate,
    },
    /// Actual reranking of all child rows before truncation.
    Rerank {
        /// Scalar child.
        child: Box<PlanTemplate>,
        /// Caller-resolved scoring method.
        method: RerankMethod,
        /// Limit applied only after reranking all rows.
        top_k: usize,
    },
    /// Root-only raw collection; never a scalar child.
    Collect {
        /// Ordered raw children.
        children: Vec<PlanTemplate>,
    },
}

/// Explicit typed operator bindings. No pass-through fallback is provided.
#[async_trait]
pub trait PlanOperators: Send + Sync {
    /// Stable candidate identity.
    type Id: Eq + Hash + Clone + Send + Sync;
    /// Owned metadata; neither Clone nor Sync is required.
    type Label: Send;
    /// Original caller error, never stringified.
    type Error: Send;
    /// Owned resolved fusion handle, consumed once.
    type Fusion: Send;
    /// Owned resolved reranker handle, consumed once.
    type Reranker: Send;
    /// Resolve before validation or any provider work.
    fn resolve_fusion(&self, strategy: &FusionStrategy) -> Result<Self::Fusion, Self::Error>;
    /// Resolve before validation or any provider work.
    fn resolve_rerank(&self, method: &RerankMethod) -> Result<Self::Reranker, Self::Error>;
    /// Typed JSON metadata equality; invoked in predicate declaration order.
    fn metadata_equals(
        &self,
        id: &Self::Id,
        label: &Self::Label,
        field: &str,
        value: &serde_json::Value,
    ) -> Result<bool, Self::Error>;
    /// Fuse the ordered child streams using the already-resolved strategy.
    async fn fuse(
        &self,
        resolved: Self::Fusion,
        children: Vec<Vec<Scored<Self::Id, Self::Label>>>,
        top_k: usize,
    ) -> Result<Vec<Scored<Self::Id, Self::Label>>, Self::Error>;
    /// Return a permutation of all input identities, with new order/scores permitted.
    async fn rerank(
        &self,
        resolved: Self::Reranker,
        method: &RerankMethod,
        rows: Vec<Scored<Self::Id, Self::Label>>,
    ) -> Result<Vec<Scored<Self::Id, Self::Label>>, Self::Error>;
}

/// Candidate program executed by the generic executor.
///
/// In 0.21 this deliberately replaces the published executor API's opaque fusion closure.
/// Adapters must supply the actual strategy/operator structure, not a diagnostic label.
pub struct PlanProgram<'a, O: PlanOperators + ?Sized> {
    /// Immutable operation structure established before provider work.
    pub template: PlanTemplate,
    /// Caller-owned physical operator bindings.
    pub operators: &'a O,
}

pub(super) enum ResolvedPlan<O: PlanOperators + ?Sized> {
    Leaf(LeafSlot),
    Empty,
    Fuse {
        children: Vec<ResolvedPlan<O>>,
        strategy: FuseStrategy,
        top_k: usize,
        resolved: O::Fusion,
    },
    Filter {
        child: Box<ResolvedPlan<O>>,
        predicate: FilterPredicate,
    },
    Rerank {
        child: Box<ResolvedPlan<O>>,
        method: RerankMethod,
        top_k: usize,
        resolved: O::Reranker,
    },
    Collect {
        children: Vec<ResolvedPlan<O>>,
    },
}

pub(super) fn resolve<O: PlanOperators + ?Sized, D>(
    template: PlanTemplate,
    operators: &O,
) -> Result<ResolvedPlan<O>, ExecutorError<O::Error, D>> {
    Ok(match template {
        PlanTemplate::Leaf(slot) => ResolvedPlan::Leaf(slot),
        PlanTemplate::Empty => ResolvedPlan::Empty,
        PlanTemplate::Fuse {
            children,
            strategy,
            top_k,
        } => {
            let validated = FusionStrategy::try_from(strategy.clone())
                .map_err(ExecutorError::StrategyValidation)?;
            let resolved = operators
                .resolve_fusion(&validated)
                .map_err(ExecutorError::FusionResolution)?;
            let children = children
                .into_iter()
                .map(|child| resolve(child, operators))
                .collect::<Result<_, _>>()?;
            ResolvedPlan::Fuse {
                children,
                strategy,
                top_k,
                resolved,
            }
        }
        PlanTemplate::Filter { child, predicate } => ResolvedPlan::Filter {
            child: Box::new(resolve(*child, operators)?),
            predicate,
        },
        PlanTemplate::Rerank {
            child,
            method,
            top_k,
        } => {
            let resolved = operators
                .resolve_rerank(&method)
                .map_err(ExecutorError::RerankResolution)?;
            ResolvedPlan::Rerank {
                child: Box::new(resolve(*child, operators)?),
                method,
                top_k,
                resolved,
            }
        }
        PlanTemplate::Collect { children } => ResolvedPlan::Collect {
            children: children
                .into_iter()
                .map(|child| resolve(child, operators))
                .collect::<Result<_, _>>()?,
        },
    })
}

pub(super) fn validate<O: PlanOperators + ?Sized, A: ExecutorArm + ?Sized>(
    plan: &ResolvedPlan<O>,
    request: &ExecutorRequest<'_, A>,
) -> Result<Vec<bool>, &'static str> {
    fn visit<O: PlanOperators + ?Sized>(
        node: &ResolvedPlan<O>,
        vector_slots: &[usize],
        seen: &mut [bool],
        active: &mut [bool],
        vector: bool,
        keyword: bool,
    ) -> Result<(), &'static str> {
        match node {
            ResolvedPlan::Leaf(LeafSlot(slot)) => {
                if *slot >= seen.len() || seen[*slot] {
                    return Err("plan leaves must name each arm slot exactly once");
                }
                seen[*slot] = true;
                active[*slot] = if vector_slots.contains(slot) {
                    vector
                } else {
                    keyword
                };
            }
            ResolvedPlan::Empty => {}
            ResolvedPlan::Fuse {
                children, strategy, ..
            } => {
                let vector = vector && !matches!(strategy, FuseStrategy::KeywordOnly);
                let keyword = keyword && !matches!(strategy, FuseStrategy::VectorOnly);
                for child in children {
                    visit(child, vector_slots, seen, active, vector, keyword)?;
                }
            }
            ResolvedPlan::Filter { child, .. } | ResolvedPlan::Rerank { child, .. } => {
                visit(child, vector_slots, seen, active, vector, keyword)?
            }
            ResolvedPlan::Collect { children } => {
                let _ = children;
                return Err(
                    "raw Collect requires execute_leaf and finish_collect, not a scalar entry",
                );
            }
        }
        Ok(())
    }
    let mut seen = vec![false; request.arms.len()];
    let mut active = vec![false; request.arms.len()];
    visit(
        plan,
        request.vector_slots,
        &mut seen,
        &mut active,
        true,
        true,
    )?;
    if seen.iter().any(|seen| !seen) {
        return Err("plan omitted an arm slot");
    }
    Ok(active)
}

fn retain_predicate<O: PlanOperators + ?Sized>(
    operators: &O,
    predicate: &FilterPredicate,
    rank: usize,
    row: &Scored<O::Id, O::Label>,
) -> Result<bool, O::Error> {
    match predicate {
        FilterPredicate::MinScore(minimum) => Ok(row.1 >= *minimum),
        FilterPredicate::TopK(limit) => Ok(rank < *limit),
        FilterPredicate::MetadataEquals { field, value } => {
            operators.metadata_equals(&row.0, &row.2, field, value)
        }
        FilterPredicate::And(children) => {
            for child in children {
                if !retain_predicate(operators, child, rank, row)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        FilterPredicate::Or(children) => {
            for child in children {
                if retain_predicate(operators, child, rank, row)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

type ScalarRows<O> = Vec<Scored<<O as PlanOperators>::Id, <O as PlanOperators>::Label>>;
type Evaluation<'a, O, D> = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<
                    (ScalarRows<O>, QueryNode),
                    ExecutorError<<O as PlanOperators>::Error, D>,
                >,
            > + Send
            + 'a,
    >,
>;

pub(super) fn evaluate<'a, O: PlanOperators + ?Sized + 'a, D: Send + 'a>(
    operation: &'static str,
    plan: ResolvedPlan<O>,
    operators: &'a O,
    streams: &'a mut [Option<ScalarRows<O>>],
    nodes: &'a mut [Option<QueryNode>],
    max_candidates: usize,
) -> Evaluation<'a, O, D>
where
    O::Id: 'a,
    O::Label: 'a,
    O::Error: 'a,
    O::Fusion: 'a,
    O::Reranker: 'a,
{
    Box::pin(async move {
        ensure_request_read_active(operation).map_err(ExecutorError::RequestStopped)?;
        match plan {
            ResolvedPlan::Leaf(LeafSlot(slot)) => {
                let rows = streams
                    .get_mut(slot)
                    .and_then(Option::take)
                    .ok_or(ExecutorError::Invalid("plan leaf stream already consumed"))?;
                let node =
                    nodes
                        .get_mut(slot)
                        .and_then(Option::take)
                        .ok_or(ExecutorError::Invalid(
                            "plan leaf provenance already consumed",
                        ))?;
                Ok((rows, node))
            }
            ResolvedPlan::Empty => Ok((Vec::new(), QueryNode::Empty)),
            ResolvedPlan::Fuse {
                children,
                strategy,
                top_k,
                resolved,
            } => {
                let mut child_rows = Vec::with_capacity(children.len());
                let mut child_nodes = Vec::with_capacity(children.len());
                for child in children {
                    let (rows, node) =
                        evaluate(operation, child, operators, streams, nodes, max_candidates)
                            .await?;
                    child_rows.push(rows);
                    child_nodes.push(node);
                }
                let mut available =
                    super::protocol::counts(child_rows.iter().flatten().map(|row| row.0.clone()));
                let rows = await_request_read_phase(
                    operation,
                    operators.fuse(resolved, child_rows, top_k),
                )
                .await
                .map_err(ExecutorError::RequestStopped)?
                .map_err(ExecutorError::Fusion)?;
                if rows.len() > top_k
                    || rows.len() > max_candidates
                    || rows
                        .iter()
                        .any(|row| !super::protocol::consume(&mut available, &row.0))
                {
                    return Err(ExecutorError::Protocol {
                        slot: None,
                        message: "fusion exceeded candidate bounds or identity multiplicity",
                    });
                }
                Ok((
                    rows,
                    QueryNode::Fuse {
                        children: child_nodes,
                        strategy,
                        top_k,
                    },
                ))
            }
            ResolvedPlan::Filter { child, predicate } => {
                let (rows, node) =
                    evaluate(operation, *child, operators, streams, nodes, max_candidates).await?;
                let mut kept = Vec::new();
                for (rank, row) in rows.into_iter().enumerate() {
                    ensure_request_read_active(operation).map_err(ExecutorError::RequestStopped)?;
                    let retained = retain_predicate(operators, &predicate, rank, &row);
                    ensure_request_read_active(operation).map_err(ExecutorError::RequestStopped)?;
                    if retained.map_err(ExecutorError::Metadata)? {
                        kept.push(row);
                    }
                }
                Ok((
                    kept,
                    QueryNode::Filter {
                        child: Box::new(node),
                        predicate,
                    },
                ))
            }
            ResolvedPlan::Rerank {
                child,
                method,
                top_k,
                resolved,
            } => {
                let (rows, node) =
                    evaluate(operation, *child, operators, streams, nodes, max_candidates).await?;
                let expected_len = rows.len();
                let mut available = super::protocol::counts(rows.iter().map(|row| row.0.clone()));
                let mut rows =
                    await_request_read_phase(operation, operators.rerank(resolved, &method, rows))
                        .await
                        .map_err(ExecutorError::RequestStopped)?
                        .map_err(ExecutorError::Rerank)?;
                if rows.len() != expected_len
                    || rows
                        .iter()
                        .any(|row| !super::protocol::consume(&mut available, &row.0))
                {
                    return Err(ExecutorError::Protocol {
                        slot: None,
                        message: "reranking changed identity multiplicity",
                    });
                }
                rows.truncate(top_k);
                Ok((
                    rows,
                    QueryNode::Rerank {
                        child: Box::new(node),
                        method,
                        top_k,
                    },
                ))
            }
            ResolvedPlan::Collect { .. } => {
                Err(ExecutorError::Invalid("raw Collect is not a scalar stream"))
            }
        }
    })
}
