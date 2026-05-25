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
pub mod error;
pub mod fusion;
pub mod graph_traversal;
pub mod objectives;
pub mod operations;
pub mod pack;
pub mod portability;
pub mod registry;
pub mod retrieval;
pub mod runtime;

pub use curation::{
    ContentMergeStrategy, EdgeListFilter, EdgePatch, EntityDedupMergePolicy, EntityPatch,
    MergeSummary, NotePatch,
};
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
    GraphProximityObjective, RetrievalCandidate, RrfFusionObjective, TextRelevanceObjective,
    VectorSimilarityObjective,
};
pub use operations::{LinkSpec, NoteSearchHit, QueryResult, Resolved};
pub use pack::{
    DispatchHook, KindHook, PackFactory, PackRegistration, PackRegistry, PackRuntime, VerbRegistry,
    VerbRegistryBuilder,
};
pub use portability::{ImportSummary, KgArchive};
pub use registry::{ObjectiveRegistry, RegisteredObjective};
pub use retrieval::{SearchHit, SearchSource};
pub use runtime::{parse_pack_list, BackendId, KhiveRuntime, NamespaceToken, RuntimeConfig};
