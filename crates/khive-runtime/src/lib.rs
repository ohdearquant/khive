//! khive-runtime: composable Service API used by daemon, MCP server, and CLI.
//!
//! Wraps `StorageBackend` + query compilation into a single Rust API.
//!
//! # Quick start
//!
//! ```ignore
//! use khive_runtime::{KhiveRuntime, RuntimeConfig};
//! use khive_types::Namespace;
//!
//! // In-memory for tests:
//! let rt = KhiveRuntime::memory()?;
//! let tok = rt.authorize(Namespace::local());
//!
//! // Create an entity:
//! let entity = rt.create_entity(&tok, "concept", None, "LoRA", None, None, vec![]).await?;
//!
//! // Link two entities:
//! let edge = rt.link(&tok, entity.id, other_id, EdgeRelation::Extends, 1.0, None).await?;
//! ```

pub mod curation;
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
pub mod validation;

pub use curation::{
    ContentMergeStrategy, EdgeListFilter, EdgePatch, EntityDedupMergePolicy, EntityPatch,
    MergeSummary, NotePatch,
};
pub use embedder_registry::{EmbedderProvider, EmbedderRegistry, LatticeEmbedderProvider};
pub use engine_config::{config_from_env, ConfigError, EngineConfig, KhiveConfig};
pub use error::{RuntimeError, RuntimeResult};
pub use fusion::FusionStrategy;
pub use graph_traversal::{PathNode, TraversalOptions};
pub use khive_gate::{
    ActorRef, AllowAllGate, AuditDecision, AuditEvent, Gate, GateContext, GateDecision, GateError,
    GateRef, GateRequest, Obligation,
};
pub use khive_storage::{EventObservation, EventView, ObservationRole, ReferentKind};
pub use khive_types::namespace::Namespace;
pub use objectives::{
    DecayAwareImportanceObjective, GraphProximityObjective, NoteCandidate, RerankerObjective,
    RetrievalCandidate, RrfFusionObjective, TemporalRecencyObjective, TextRelevanceObjective,
    VectorSimilarityObjective,
};
pub use operations::{LinkSpec, NoteSearchHit, QueryResult, Resolved};
pub use pack::{
    DispatchHook, HandlerDef, KindHook, NoteKindSpec, NoteLifecycleSpec, PackFactory,
    PackRegistration, PackRegistry, PackRuntime, PackSchemaPlan, ParamDef, SchemaPlan,
    VerbCategory, VerbRegistry, VerbRegistryBuilder, Visibility,
};
pub use portability::{ImportSummary, KgArchive};
pub use presentation::{present, PresentationMode};
pub use registry::{ObjectiveRegistry, RegisteredObjective};
pub use retrieval::{SearchHit, SearchSource};
pub use runtime::{
    parse_pack_list, runtime_config_from_khive_config, BackendId, KhiveRuntime, NamespaceToken,
    RuntimeConfig,
};
pub use validation::{
    GraphPatch, GraphSnapshot, RuleFn, RuleId, Severity, ValidationContext, ValidationReport,
    ValidationRule, Violation,
};
