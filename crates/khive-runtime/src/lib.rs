//! khive-runtime: composable Service API used by daemon, MCP server, and CLI.
//!
//! Wraps `StorageBackend` and query compilation into a single Rust API surface.

pub mod actor_identity;
pub mod agent_lifecycle;
pub mod ann_registry;
pub mod atomic_message;
pub mod atomic_plan;
pub mod atomic_prepare;
pub mod atomic_runner;
pub mod audit_batch;
pub mod blob;
pub mod build_info;
pub mod config;
pub mod config_ledger;
pub mod cost_unit;
pub mod curation;
pub mod daemon;
pub mod embedder_registry;
pub mod engine_config;
pub mod error;
pub mod events_split;
pub mod fusion;
pub mod graph_traversal;
mod note_store_guard;
pub mod objectives;
pub mod operations;
pub mod pack;
pub mod phase_events;
pub mod portability;
pub mod preference_verification;
pub mod presentation;
pub mod reference_resolution;
pub mod reference_ring;
pub mod registry;
pub mod resource;
pub mod retrieval;
pub mod runtime;
pub mod secret_gate;
pub(crate) mod secret_gate_finalizer;
pub mod time_anchor;
pub use khive_storage::usage;
pub mod validation;

pub use actor_identity::{actor_is_unattributed, resolve_actor, should_warn_unattributed_actor};
pub use agent_lifecycle::{
    apply_transition, spawn_fingerprint, AgentRecord, AgentState, IllegalTransition,
    TerminalReason, Transition, Trigger,
};
pub use atomic_message::{create_notes_atomic, create_notes_atomic_with_report, AtomicNoteSpec};
pub use atomic_plan::{
    AddEntityPlan, AddNotePlan, AffectedRowGuard, DeletePlan, GovernanceOp, GovernancePlan,
    GtdCompletePlan, GtdTransitionPlan, LinkPlan, MergePlan, PlanPredicate, PlanStatement,
    PostCommitEffect, UpdatePlan,
};
pub use atomic_runner::{
    run_atomic_unit, AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome, AtomicRunnerError,
    CommittedPostCommitEffects,
};
pub use blob::{
    resolve_blob_store, resolve_blob_store_for_mode, BlobHydrator, GovernedBlobError, VerifiedBlob,
    DEFAULT_BLOB_HYDRATION_BYTES,
};
pub use build_info::{BuildInfo, BUILD_INFO, BUILD_VERSION};
pub use config::{ann_fresh_tail_enabled_from_env, process_ref_from_env};
pub use cost_unit::{base_resource_payload, cost_unit_for_dispatch, resource_payload};
pub use curation::{
    entity_embedding_text, entity_fts_document, entity_merge_guard_error, note_embedding_text,
    note_fts_document, validate_entity_merge_floor, ContentMergeStrategy, EdgeListFilter,
    EdgePatch, EntityDedupMergePolicy, EntityMergeGuard, EntityPatch, MergeEdgeConflictPreimage,
    MergeEdgePreimage, MergeSummary, MergeTxBudgetReport, MergeTxLimits, NotePatch,
};
#[cfg(unix)]
pub use daemon::{acquire_recovery_lock, pid_path, run_daemon, socket_path, DaemonDispatch};
pub use daemon::{
    active_phase_names, background_task_count, daemon_shutdown_token, register_active_phase,
    track_background_task, DaemonRequestFrame, DaemonResponseFrame, PhaseGuard, PROTOCOL_VERSION,
};
pub use embedder_registry::{EmbedderProvider, EmbedderRegistry, LatticeEmbedderProvider};
pub use engine_config::{
    config_from_env, BackendConfig, BackendKind, BlobConfig, ConfigError, EngineConfig,
    GitWriteEntryConfig, GitWriteSectionConfig, KhiveConfig, PackConfig, StorageSectionConfig,
};
pub use error::{
    fts_text_leg_or_err, AdmissionFailureContext, ChannelIngestFailureClass, GuardedWriteFailure,
    RuntimeError, RuntimeResult, WriterPoolCheckoutTimeoutContext, WRITER_ADMISSION_SCOPE,
    WRITER_POOL_CHECKOUT_TIMEOUT_STAGE, WRITER_QUEUE_SATURATED_STAGE,
};
pub use fusion::FusionStrategy;
pub use graph_traversal::PathNode;
pub use khive_db::{
    checkpoint_once, run_checkpoint_task, run_migrations, CheckpointConfig,
    CheckpointLifecycleOwner, CheckpointTick, ConnectionPool, StorageBackend,
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
    arm_entity_compensation_fail_scoped, arm_fts_fail_many_partial_scoped,
    arm_fts_fail_many_scoped, arm_fts_fail_scoped, arm_prefix_resolve_fail_scoped,
    arm_rollback_cleanup_fail, arm_vector_fail_after, arm_vector_fail_scoped, FaultInjectionArm,
};
pub use operations::{
    base_entity_endpoint_rules, base_entity_rule_allows, endpoint_matches,
    hex_prefix_to_uuid_pattern, merge_entry_metadata, uuid_prefix_bounds, EdgeEndpointKind,
    EntityCreateSpec, LinkSpec, NoteSearchHit, QueryResult, Resolved,
};
pub use pack::{
    resolve_explicit_namespace, ChannelIngestCapability, DispatchHook, HandlerDef,
    IdResolutionMode, InterceptedDispatchResult, KindHook, NoteKindSpec, NoteLifecycleSpec,
    PackByIdResolver, PackFactory, PackInstall, PackLoadError, PackRegistration, PackRegistry,
    PackRuntime, PackSchemaCollisionError, PackSchemaPlan, ParamDef, RequestIdentity, SchemaPlan,
    VerbCategory, VerbPresentationPolicy, VerbRegistry, VerbRegistryBuilder, VerifiedActor,
    Visibility, AUDIT_PERSISTENCE_SKIPPED_READ_ONLY,
};
pub use phase_events::{emit_phase_event, is_benign_shutdown_cancellation};
pub use portability::{ImportSummary, KgArchive};
pub use preference_verification::{LegacyPreferenceVerifier, VerifiedModelNetworkAttachment};
pub use presentation::{
    apply_redundancy_drop, micros_to_iso, present, render_format, rfc3339_to_utc_micros,
    OutputFormat, PresentationMode,
};
pub use reference_resolution::{resolve_reference, ReferenceCandidate, ReferenceResolution};
pub use reference_ring::{ReferenceRing, RingEntry};
pub use registry::{ObjectiveRegistry, RegisteredObjective};
pub use resource::{cpu_delta_us, process_resource_usage, ProcessResourceUsage};
pub use retrieval::{SearchHit, SearchSource};
pub use runtime::{
    assert_captured_db_anchor_consistent, assert_db_anchor_consistent, expand_tilde,
    parse_pack_list, resolve_db_anchor, resolve_project_actor_id, runtime_config_from_khive_config,
    BackendId, EntityTypeValidatorFn, KhiveRuntime, NamedVectorIdentity, NamespaceToken,
    NoteMutationHookFn, NoteWriteValidatorFn, RuntimeConfig,
};
pub use secret_gate::SecretMatch;
#[cfg(any(test, feature = "test-internals"))]
pub use secret_gate_finalizer::boundary::{
    arm_secret_gate_test_audit_failure, arm_secret_gate_test_exemption, SecretGateAuditFailureArm,
    SecretGateTestArm,
};
pub use secret_gate_finalizer::boundary::{
    finalize_secret_gate_candidate, finalize_secret_gate_update_candidate,
    secret_gate_atomic_statements, SecretGateCandidateFields, SecretGateFieldScope,
    SecretGateFinalization, SecretGateTargetKind,
};
pub use validation::{
    GraphPatch, GraphSnapshot, RuleFn, RuleId, Severity, ValidationContext, ValidationReport,
    ValidationRule, Violation,
};
