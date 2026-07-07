//! khive-runtime: composable Service API used by daemon, MCP server, and CLI.
//!
//! Wraps `StorageBackend` and query compilation into a single Rust API surface.

pub mod atomic_plan;
pub mod atomic_runner;
pub mod config;
pub mod curation;
#[cfg(unix)]
pub mod daemon;
pub mod embedder_registry;
pub mod engine_config;
pub mod error;
pub mod fusion;
pub mod graph_traversal;
pub mod objectives;
pub mod operations;
pub mod pack;
pub mod portability;
pub mod presentation;
pub mod registry;
pub mod retrieval;
pub mod runtime;
pub mod secret_gate;
pub mod validation;

pub use atomic_plan::{
    AffectedRowGuard, DeletePlan, GovernanceOp, GovernancePlan, GtdCompletePlan, GtdTransitionPlan,
    LinkPlan, MergePlan, PlanPredicate, PlanStatement, PostCommitEffect, UpdatePlan,
};
pub use atomic_runner::{
    run_atomic_unit, AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome, AtomicRunnerError,
};
pub use curation::{
    entity_fts_document, note_fts_document, ContentMergeStrategy, EdgeListFilter, EdgePatch,
    EntityDedupMergePolicy, EntityPatch, MergeSummary, NotePatch,
};
#[cfg(unix)]
pub use daemon::acquire_recovery_lock;
pub use daemon::{
    background_task_count, pid_path, run_daemon, socket_path, track_background_task,
    DaemonDispatch, DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
};
pub use embedder_registry::{EmbedderProvider, EmbedderRegistry, LatticeEmbedderProvider};
pub use engine_config::{
    config_from_env, BackendConfig, BackendKind, ConfigError, EngineConfig, KhiveConfig, PackConfig,
};
pub use error::{RuntimeError, RuntimeResult};
pub use fusion::FusionStrategy;
pub use graph_traversal::PathNode;
pub use khive_db::{
    checkpoint_once, run_checkpoint_task, run_migrations, CheckpointConfig, CheckpointTick,
    ConnectionPool, StorageBackend,
};
pub use khive_gate::{
    ActorRef, AllowAllGate, AuditDecision, AuditEvent, Gate, GateContext, GateDecision, GateError,
    GateRef, GateRequest, Obligation,
};
pub use khive_storage::types::TraversalOptions;
pub use khive_storage::{EventObservation, EventView, ObservationRole, ReferentKind};
pub use khive_types::namespace::Namespace;
pub use objectives::{
    AmplifiedDecayAwareSalienceObjective, DecayAwareSalienceObjective, GraphProximityObjective,
    MemoryRecallPipeline, NoteCandidate, RerankerObjective, RetrievalCandidate, RrfFusionObjective,
    TemporalRecencyObjective, TextRelevanceObjective, VectorSimilarityObjective,
};
#[cfg(any(test, feature = "fault-injection"))]
pub use operations::{
    arm_fts_fail, arm_fts_fail_many, arm_fts_fail_many_partial, arm_rollback_cleanup_fail,
    arm_vector_fail, arm_vector_fail_after,
};
pub use operations::{EntityCreateSpec, LinkSpec, NoteSearchHit, QueryResult, Resolved};
pub use pack::{
    resolve_explicit_namespace, DispatchHook, HandlerDef, KindHook, NoteKindSpec,
    NoteLifecycleSpec, PackByIdResolver, PackFactory, PackLoadError, PackRegistration,
    PackRegistry, PackRuntime, PackSchemaCollisionError, PackSchemaPlan, ParamDef, RequestIdentity,
    SchemaPlan, VerbCategory, VerbPresentationPolicy, VerbRegistry, VerbRegistryBuilder,
    Visibility,
};
pub use portability::{ImportSummary, KgArchive};
pub use presentation::{
    apply_redundancy_drop, micros_to_iso, present, render_format, OutputFormat, PresentationMode,
};
pub use registry::{ObjectiveRegistry, RegisteredObjective};
pub use retrieval::{SearchHit, SearchSource};
pub use runtime::{
    assert_db_anchor_consistent, parse_pack_list, resolve_db_anchor, resolve_project_actor_id,
    runtime_config_from_khive_config, BackendId, EntityTypeValidatorFn, KhiveRuntime,
    NamespaceToken, RuntimeConfig,
};
pub use secret_gate::SecretMatch;
pub use validation::{
    GraphPatch, GraphSnapshot, RuleFn, RuleId, Severity, ValidationContext, ValidationReport,
    ValidationRule, Violation,
};
