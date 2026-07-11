//! Storage capability traits: `SqlAccess`, `VectorStore`, `TextSearch`, `GraphStore`, `NoteStore`, `EventStore`.

pub mod capability;
pub mod entity;
pub mod error;
pub mod event;
pub mod graph;
pub mod note;
pub mod sparse;
pub mod sql;
pub mod telemetry;
pub mod text;
pub mod tx_registry;
pub mod types;
pub mod vectors;

pub use capability::StorageCapability;
pub use entity::{Entity, EntityFilter, EntityStore};
pub use error::StorageError;

pub use event::{
    Event, EventFilter, EventObservation, EventStore, EventView, ObservationRole, ReferentKind,
};
pub use graph::GraphStore;
pub use note::{FilterOp, Note, NoteFilter, NoteStore, SortDir};
pub use sparse::SparseStore;
pub use sql::{AtomicUnitOp, BoxFuture, SqlAccess, SqlReader, SqlWriter};
pub use telemetry::{
    ChannelBackoffArmedPayload, ChannelBackoffResetPayload, ChannelHeartbeatPersistFailedPayload,
    ChannelPollFailedPayload, ChannelPollStartedPayload, ChannelPollSucceededPayload,
    CheckpointOutcomeRecordedPayload, ConfigLockedPayload, LifecycleEvent, PhaseCancelledPayload,
    PhaseCompletedPayload, PhaseStartedPayload,
};
pub use text::TextSearch;
pub use types::StorageResult;
pub use vectors::VectorStore;

pub use types::{
    BatchWriteSummary, DeleteMode, DirectedNeighborHit, Direction, Edge, EdgeFilter, EdgeSeekPage,
    EdgeSortField, GraphPath, GuardedBatchOutcome, GuardedBatchRefusal, GuardedWriteOutcome,
    IndexRebuildScope, LinkId, MissingEndpoints, NeighborHit, NeighborQuery, OrphanSweepConfig,
    OrphanSweepResult, Page, PageRequest, PathNode, PropertyFilter, PropertyOp, SortDirection,
    SortOrder, SparseRecord, SparseSearchHit, SparseSearchRequest, SparseVector, SqlRow,
    SqlStatement, SqlValue, TextDocument, TextFilter, TextGatherMode, TextIndexStats,
    TextQueryMode, TextSearchHit, TextSearchOptions, TextSearchRequest, TextTermStats,
    TextTermStatsRequest, TimeRange, TraversalOptions, TraversalRequest, VectorIndexKind,
    VectorMetadataFilter, VectorRecord, VectorSearchHit, VectorSearchRequest,
    VectorStoreCapabilities, VectorStoreInfo, MAX_SPARSE_SEARCH_TOP_K,
};

pub use khive_types::{EdgeCategory, EdgeRelation, EventOutcome, SubstrateKind};
