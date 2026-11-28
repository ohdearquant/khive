use async_trait::async_trait;
use khive_retrieval::hybrid::executor::{
    execute_collect_leaf, execute_leaf, ArmCompletion, ArmFailure, ArmInput, ArmRequest, ArmRoute,
    ArmSpec, ArmState, CollectedArms, CollectionPlan, CollectionSlot, ExecutedLeaf, ExecutorArm,
    ExecutorError, FailurePolicy, LeafExecutionContext, LeafKind, LeafSlot, PreparedLeaf,
    SearchScope, VectorInput,
};
use khive_retrieval::QueryNode;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{TextQueryMode, TextSearchHit, VectorSearchHit};
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::common::{collect_model_ann_hits, PerModelAnnHits, TextSnippetPolicy};
use crate::{ann, config::RecallFtsGatherConfig, MemoryPack};

pub(super) enum RawHit {
    Text(TextSearchHit),
    Vector(VectorSearchHit),
}

pub(super) enum Evidence {
    Text,
    Vector(PerModelAnnHits),
    WithheldEmbedding,
}

pub(super) type MemoryLeaf = ExecutedLeaf<Uuid, RawHit, Evidence, RuntimeError>;
pub(super) type MemoryArms = CollectedArms<Uuid, RawHit, Evidence, RuntimeError>;

pub(super) fn declaration(slots: Vec<CollectionSlot>) -> Result<CollectionPlan, RuntimeError> {
    CollectionPlan::new(slots).map_err(|message| RuntimeError::Internal(message.into()))
}

pub(super) fn text_declaration() -> Result<CollectionPlan, RuntimeError> {
    declaration(vec![CollectionSlot {
        slot: LeafSlot(0),
        key: "text".into(),
        kind: LeafKind::Keyword,
    }])
}

pub(super) fn vector_declaration(models: &[String]) -> Result<CollectionPlan, RuntimeError> {
    declaration(
        models
            .iter()
            .enumerate()
            .map(|(index, model)| CollectionSlot {
                slot: LeafSlot(index + 1),
                key: format!("vector:{model}"),
                kind: LeafKind::Vector,
            })
            .collect(),
    )
}

pub(super) fn model_slot(plan: &CollectionPlan, model: &str) -> Result<LeafSlot, RuntimeError> {
    let key = format!("vector:{model}");
    plan.slots()
        .iter()
        .find(|slot| slot.key == key)
        .map(|slot| slot.slot)
        .ok_or_else(|| RuntimeError::Internal("memory model was not declared before work".into()))
}

pub(super) fn executor_error(error: ExecutorError<RuntimeError, Evidence>) -> RuntimeError {
    match error {
        ExecutorError::RequestStopped(error) => error.into(),
        ExecutorError::Arm { error, .. }
        | ExecutorError::VectorPreparation(error)
        | ExecutorError::OutcomePolicy(error)
        | ExecutorError::Preparation(error)
        | ExecutorError::FusionResolution(error)
        | ExecutorError::Fusion(error)
        | ExecutorError::RerankResolution(error)
        | ExecutorError::Metadata(error)
        | ExecutorError::Rerank(error)
        | ExecutorError::Hydration(error) => error,
        ExecutorError::Invalid(message) | ExecutorError::Protocol { message, .. } => {
            RuntimeError::Internal(format!("memory collection plan: {message}"))
        }
        ExecutorError::StrategyValidation(error) => RuntimeError::Internal(format!(
            "raw memory collection cannot validate fusion: {error}"
        )),
        ExecutorError::Unavailable { .. } | ExecutorError::MissingVectorPreparer { .. } => {
            RuntimeError::Internal("memory collection lost its prepared provider binding".into())
        }
    }
}

pub(super) fn model_evidence(leaf: &MemoryLeaf) -> Result<&PerModelAnnHits, RuntimeError> {
    match &leaf.report().state {
        ArmState::Completed {
            evidence: Evidence::Vector(evidence),
            ..
        } => Ok(evidence),
        _ => Err(RuntimeError::Internal(
            "memory vector result lost its completed evidence".into(),
        )),
    }
}

pub(super) type RawStreams = (Vec<TextSearchHit>, Vec<(String, Vec<VectorSearchHit>)>);

pub(super) fn raw_streams(collected: MemoryArms) -> Result<RawStreams, RuntimeError> {
    let mut text = Vec::new();
    let mut vectors = Vec::new();
    for (rows, report) in collected.arms.into_iter().zip(collected.reports) {
        match report.state {
            ArmState::Skipped => {}
            ArmState::Completed {
                evidence: Evidence::Text,
                ..
            } => {
                for (_, _, label) in rows {
                    let RawHit::Text(hit) = label else {
                        return Err(RuntimeError::Internal(
                            "text collection label changed".into(),
                        ));
                    };
                    text.push(hit);
                }
            }
            ArmState::Completed {
                evidence: Evidence::Vector(evidence),
                ..
            } => {
                let mut hits = Vec::with_capacity(rows.len());
                for (_, _, label) in rows {
                    let RawHit::Vector(hit) = label else {
                        return Err(RuntimeError::Internal(
                            "vector collection label changed".into(),
                        ));
                    };
                    hits.push(hit);
                }
                vectors.push((evidence.model_name, hits));
            }
            _ => {
                return Err(RuntimeError::Internal(
                    "memory collection lost its required result".into(),
                ));
            }
        }
    }
    Ok((text, vectors))
}

enum Provider<'a> {
    Text {
        pack: &'a MemoryPack,
        token: &'a NamespaceToken,
        cjk_fts_bypass: bool,
        fts_gather: &'a RecallFtsGatherConfig,
    },
    Vector {
        runtime: &'a KhiveRuntime,
        ann: &'a ann::SharedAnn,
        token: &'a NamespaceToken,
        namespace: &'a str,
        ordinary_limit: u32,
        ann_fetch_limit: usize,
        max_rounds: usize,
        ready_timeout_ms: u64,
        session_seq: Option<(u64, String)>,
    },
    Skipped,
}

struct MemoryArm<'a> {
    provider: Provider<'a>,
    #[cfg(test)]
    slot: LeafSlot,
    #[cfg(test)]
    observer: Observer,
}

#[async_trait]
impl ExecutorArm for MemoryArm<'_> {
    type Id = Uuid;
    type Label = RawHit;
    type Evidence = Evidence;
    type Filter = ();
    type Error = RuntimeError;

    async fn search<'a>(
        &'a self,
        request: ArmRequest<'a, ()>,
    ) -> Result<ArmCompletion<Uuid, RawHit, Evidence>, ArmFailure<RuntimeError, Evidence>> {
        #[cfg(test)]
        let observation = self.observer.begin(self.slot, &request);
        #[cfg(test)]
        self.observer
            .before_provider(self.slot)
            .await
            .map_err(ArmFailure::Fatal)?;
        let namespaces: Vec<String> = request
            .scope
            .namespaces
            .iter()
            .map(|s| (*s).into())
            .collect();
        let completion = match (&self.provider, request.input) {
            (
                Provider::Text {
                    pack,
                    token,
                    cjk_fts_bypass,
                    fts_gather,
                },
                ArmInput::Text {
                    query,
                    snippet_chars,
                    ..
                },
            ) => {
                let snippet = if *snippet_chars == 0 {
                    TextSnippetPolicy::Omit
                } else {
                    TextSnippetPolicy::Include {
                        chars: *snippet_chars,
                    }
                };
                let hits = pack
                    .collect_recall_text_hits(
                        token,
                        query,
                        &namespaces,
                        request.candidate_limit,
                        snippet,
                        *cjk_fts_bypass,
                        fts_gather,
                    )
                    .await
                    .map_err(ArmFailure::Fatal)?;
                ArmCompletion {
                    hits: hits
                        .into_iter()
                        .map(|hit| (hit.subject_id, hit.score, RawHit::Text(hit)))
                        .collect(),
                    route: ArmRoute::Primary,
                    evidence: Evidence::Text,
                }
            }
            (
                Provider::Vector {
                    runtime,
                    ann,
                    token,
                    namespace,
                    ordinary_limit,
                    ann_fetch_limit,
                    max_rounds,
                    ready_timeout_ms,
                    session_seq,
                },
                ArmInput::Vector {
                    model_id,
                    input: VectorInput::Embedding(embedding),
                    ..
                },
            ) => {
                let fenced = session_seq.is_some();
                let mut outcome = collect_model_ann_hits(
                    runtime,
                    ann,
                    token,
                    namespace,
                    &namespaces,
                    (*model_id).to_owned(),
                    embedding.to_vec(),
                    if fenced {
                        *ordinary_limit
                    } else {
                        request.candidate_limit
                    },
                    if fenced {
                        request.candidate_limit as usize
                    } else {
                        *ann_fetch_limit
                    },
                    *max_rounds,
                    *ready_timeout_ms,
                    session_seq.clone(),
                )
                .await
                .map_err(ArmFailure::Fatal)?;
                let hits = std::mem::take(&mut outcome.hits)
                    .into_iter()
                    .map(|hit| (hit.subject_id, hit.score, RawHit::Vector(hit)))
                    .collect();
                ArmCompletion {
                    hits,
                    route: if outcome.used_sqlite_vec_fallback {
                        ArmRoute::ExactFallback
                    } else {
                        ArmRoute::Primary
                    },
                    evidence: Evidence::Vector(outcome),
                }
            }
            _ => {
                return Err(ArmFailure::Fatal(RuntimeError::Internal(
                    "memory leaf/provider family mismatch".into(),
                )))
            }
        };
        #[cfg(test)]
        self.observer.complete(observation, &completion);
        Ok(completion)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn text_leaf(
    pack: &MemoryPack,
    token: &NamespaceToken,
    query: &str,
    namespaces: &[String],
    limit: u32,
    enabled: bool,
    snippet: TextSnippetPolicy,
    cjk_fts_bypass: bool,
    fts_gather: &RecallFtsGatherConfig,
) -> Result<MemoryLeaf, RuntimeError> {
    let arm = MemoryArm {
        provider: Provider::Text {
            pack,
            token,
            cjk_fts_bypass,
            fts_gather,
        },
        #[cfg(test)]
        slot: LeafSlot(0),
        #[cfg(test)]
        observer: Observer::current(),
    };
    let names: Vec<&str> = namespaces.iter().map(String::as_str).collect();
    let mode = TextQueryMode::AnyTerm;
    let spec = ArmSpec {
        key: "text",
        adapter: &arm,
        scope: SearchScope {
            namespaces: &names,
            filter: &(),
        },
        input: Some(ArmInput::Text {
            query,
            mode: &mode,
            snippet_chars: snippet.snippet_chars(),
        }),
        candidate_limit: limit,
        failure: FailurePolicy::Required,
    };
    let node = if enabled {
        QueryNode::Keyword {
            text: query.into(),
            top_k: limit as usize,
            min_score: None,
        }
    } else {
        QueryNode::Empty
    };
    execute_leaf(
        LeafExecutionContext {
            operation: "memory.recall",
            permits: &Semaphore::new(1),
        },
        LeafSlot(0),
        &spec,
        PreparedLeaf {
            node,
            preparation: None,
        },
    )
    .await
    .map_err(executor_error)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn vector_leaf(
    runtime: &KhiveRuntime,
    ann: &ann::SharedAnn,
    token: &NamespaceToken,
    namespace: &str,
    namespaces: &[String],
    model: String,
    embedding: Vec<f32>,
    candidate_limit: u32,
    ann_fetch_limit: usize,
    max_rounds: usize,
    ready_timeout_ms: u64,
    session_seq: Option<(u64, String)>,
    slot: LeafSlot,
    permits: &Semaphore,
    observer: Observer,
) -> Result<MemoryLeaf, RuntimeError> {
    let limit = if session_seq.is_some() {
        u32::try_from(ann_fetch_limit)
            .map_err(|_| RuntimeError::Internal("session candidate bound exceeds u32".into()))?
    } else {
        candidate_limit
    };
    let arm = MemoryArm {
        provider: Provider::Vector {
            runtime,
            ann,
            token,
            namespace,
            ordinary_limit: candidate_limit,
            ann_fetch_limit,
            max_rounds,
            ready_timeout_ms,
            session_seq,
        },
        #[cfg(test)]
        slot,
        #[cfg(test)]
        observer,
    };
    #[cfg(not(test))]
    let _ = observer;
    let names: Vec<&str> = namespaces.iter().map(String::as_str).collect();
    let key = format!("vector:{model}");
    let spec = ArmSpec {
        key: &key,
        adapter: &arm,
        scope: SearchScope {
            namespaces: &names,
            filter: &(),
        },
        input: Some(ArmInput::Vector {
            engine: "memory",
            model_id: &model,
            space_id: None,
            input: VectorInput::Embedding(&[]),
        }),
        candidate_limit: limit,
        failure: FailurePolicy::Required,
    };
    let leaf = execute_collect_leaf(
        LeafExecutionContext {
            operation: "memory.recall",
            permits,
        },
        slot,
        &spec,
        PreparedLeaf {
            node: QueryNode::Vector {
                embedding,
                top_k: limit as usize,
                min_score: None,
            },
            preparation: None,
        },
    )
    .await
    .map_err(executor_error)?;
    #[cfg(test)]
    arm.observer.finished(&leaf);
    Ok(leaf)
}

pub(super) async fn skipped_vector(
    slot: &CollectionSlot,
    limit: u32,
) -> Result<MemoryLeaf, RuntimeError> {
    let arm = MemoryArm {
        provider: Provider::Skipped,
        #[cfg(test)]
        slot: slot.slot,
        #[cfg(test)]
        observer: Observer::current(),
    };
    let spec = ArmSpec {
        key: &slot.key,
        adapter: &arm,
        scope: SearchScope {
            namespaces: &[],
            filter: &(),
        },
        input: None,
        candidate_limit: limit,
        failure: FailurePolicy::Required,
    };
    execute_leaf(
        LeafExecutionContext {
            operation: "memory.recall",
            permits: &Semaphore::new(1),
        },
        slot.slot,
        &spec,
        PreparedLeaf {
            node: QueryNode::Empty,
            preparation: Some(Evidence::WithheldEmbedding),
        },
    )
    .await
    .map_err(executor_error)
}

#[derive(Clone, Default)]
pub(super) struct Observer {
    #[cfg(test)]
    pub(super) log: Option<std::sync::Arc<TraceLog>>,
}

impl Observer {
    #[cfg(test)]
    fn finished(&self, leaf: &MemoryLeaf) {
        let Some(log) = &self.log else {
            return;
        };
        let mut entries = log.entries.lock().unwrap();
        if let Some(entry) = entries
            .iter_mut()
            .rev()
            .find(|entry| entry["slot"] == leaf.slot().0)
        {
            entry["requested"] = serde_json::json!(leaf.report().requested);
            entry["returned"] = serde_json::json!(leaf.report().returned);
        }
    }

    pub(super) fn current() -> Self {
        #[cfg(test)]
        return TRACE.try_with(Clone::clone).unwrap_or_default();
        #[cfg(not(test))]
        Self::default()
    }
}

#[cfg(test)]
tokio::task_local! { pub(super) static TRACE: Observer; }

#[cfg(test)]
#[derive(Default)]
pub(super) struct TraceLog {
    pub(super) text_entered: std::sync::atomic::AtomicBool,
    pub(super) entries: std::sync::Mutex<Vec<serde_json::Value>>,
    pub(super) gates:
        std::sync::Mutex<std::collections::HashMap<usize, std::sync::Arc<ProviderGate>>>,
}

/// Request-owned barriers exercise the existing model scheduler without changing
/// its production providers or treating injected errors as native failures.
#[cfg(test)]
pub(super) struct ProviderGate {
    pub(super) entered: tokio::sync::Notify,
    pub(super) release: Semaphore,
    pub(super) exited: tokio::sync::Notify,
    pub(super) error: &'static str,
}

#[cfg(test)]
struct GateExit(std::sync::Arc<ProviderGate>);

#[cfg(test)]
impl Drop for GateExit {
    fn drop(&mut self) {
        self.0.exited.notify_one();
    }
}

#[cfg(test)]
impl Observer {
    async fn before_provider(&self, slot: LeafSlot) -> Result<(), RuntimeError> {
        let gate = self
            .log
            .as_ref()
            .and_then(|log| log.gates.lock().unwrap().get(&slot.0).cloned());
        let Some(gate) = gate else {
            return Ok(());
        };
        let _exit = GateExit(gate.clone());
        gate.entered.notify_one();
        gate.release
            .acquire()
            .await
            .expect("fixture gate remains open")
            .forget();
        Err(RuntimeError::InvalidInput(gate.error.into()))
    }

    fn begin(&self, slot: LeafSlot, request: &ArmRequest<'_, ()>) -> Option<usize> {
        let log = self.log.as_ref()?;
        let input = match request.input {
            ArmInput::Text {
                query,
                snippet_chars,
                ..
            } => {
                log.text_entered
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                serde_json::json!({"kind":"keyword", "text":query, "snippet_chars":snippet_chars})
            }
            ArmInput::Vector {
                engine,
                model_id,
                space_id,
                input: VectorInput::Embedding(vector),
            } => {
                serde_json::json!({"kind":"vector", "embedding":vector, "engine":engine, "model":model_id, "space":space_id})
            }
            _ => panic!("memory interpreter must pass prepared embeddings"),
        };
        let mut entries = log.entries.lock().unwrap();
        let index = entries.len();
        entries.push(serde_json::json!({"slot":slot.0, "input":input,
            "namespaces":request.scope.namespaces, "limit":request.candidate_limit}));
        Some(index)
    }

    fn complete(&self, index: Option<usize>, completion: &ArmCompletion<Uuid, RawHit, Evidence>) {
        let (Some(log), Some(index)) = (&self.log, index) else {
            return;
        };
        let rows: Vec<_> = completion.hits.iter().map(|(_, _, label)| match label {
            RawHit::Text(hit) => serde_json::json!({"id":hit.subject_id.to_string(), "score":hit.score.to_f64(),
                "rank":hit.rank, "title":hit.title, "snippet":hit.snippet}),
            RawHit::Vector(hit) => serde_json::json!({"id":hit.subject_id.to_string(), "score":hit.score.to_f64(), "rank":hit.rank}),
        }).collect();
        log.entries.lock().unwrap()[index]["rows"] = serde_json::json!(rows);
    }
}

#[cfg(test)]
#[path = "query_ir_adapter_tests.rs"]
mod tests;
