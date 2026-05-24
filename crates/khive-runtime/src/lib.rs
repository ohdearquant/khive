//! khive-runtime: composable Service API used by daemon, MCP server, and CLI.
//!
//! Wraps `StorageBackend` + query compilation into a single Rust API.
//!
//! # Quick start
//!
//! ```ignore
//! use khive_runtime::{KhiveRuntime, RuntimeConfig};
//!
//! // In-memory for tests:
//! let rt = KhiveRuntime::memory()?;
//!
//! // Default (production): reads ~/.khive/khive-graph.db
//! let rt = KhiveRuntime::new(RuntimeConfig::default())?;
//!
//! // Create an entity:
//! let entity = rt.create_entity(None, "concept", "LoRA", None, None, vec![]).await?;
//!
//! // Link two entities (EdgeRelation is the typed relation):
//! let edge = rt.link(None, entity.id, other_id, EdgeRelation::Extends, 1.0, None).await?;
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

pub use curation::{EdgeListFilter, EntityPatch, MergeStrategy, MergeSummary};
pub use error::{RuntimeError, RuntimeResult};
pub use fusion::FusionStrategy;
pub use graph_traversal::{PathNode, TraversalOptions};
pub use khive_gate::{
    ActorRef, AllowAllGate, AuditDecision, AuditEvent, Gate, GateContext, GateDecision, GateError,
    GateRef, GateRequest, Obligation,
};
pub use khive_storage::{EventObservation, EventView, ObservationRole, ReferentKind};
pub use objectives::{
    GraphProximityObjective, RetrievalCandidate, RrfFusionObjective, TextRelevanceObjective,
    VectorSimilarityObjective,
};
pub use operations::{LinkSpec, NamespaceToken, NoteSearchHit, QueryResult, Resolved};
pub use pack::{
    DispatchHook, KindHook, PackFactory, PackRegistration, PackRegistry, PackRuntime, VerbRegistry,
    VerbRegistryBuilder,
};
pub use portability::{ImportSummary, KgArchive};
pub use registry::{ObjectiveRegistry, RegisteredObjective};
pub use retrieval::{SearchHit, SearchSource};
pub use runtime::{parse_pack_list, KhiveRuntime, RuntimeConfig};
