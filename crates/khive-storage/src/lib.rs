//! Storage capability traits: `SqlAccess`, `VectorStore`, `TextSearch`, `GraphStore`, `NoteStore`, `EventStore`, `BlobStore`.

pub mod agent;
pub mod blob;
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
pub mod usage;
pub mod vectors;

pub use agent::AgentStore;
pub use blob::{BlobOrphanSweepConfig, BlobOrphanSweepResult, BlobStore, ContentRef};
pub use capability::StorageCapability;
pub use entity::{Entity, EntityFilter, EntityStore};
pub use error::{StorageError, WriterTaskRequestState};

pub use event::{
    Event, EventFilter, EventObservation, EventStore, EventView, ObservationRole, ReferentKind,
};
pub use graph::GraphStore;
pub use note::{FilterOp, Note, NoteFilter, NoteInsertOutcome, NoteStore, SortDir};
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
    OrphanSweepResult, Page, PageRequest, PathNode, PropertyFilter, PropertyOp, SeekCursor,
    SeekPage, SortDirection, SortOrder, SparseRecord, SparseSearchHit, SparseSearchRequest,
    SparseVector, SqlRow, SqlStatement, SqlValue, TextDocument, TextFilter, TextGatherMode,
    TextIndexStats, TextQueryMode, TextSearchHit, TextSearchOptions, TextSearchRequest,
    TextTermStats, TextTermStatsRequest, TimeRange, TraversalExecutionBudget, TraversalOptions,
    TraversalRequest, VectorIndexKind, VectorMetadataFilter, VectorRecord, VectorSearchHit,
    VectorSearchRequest, VectorStoreCapabilities, VectorStoreInfo, DEFAULT_TRAVERSAL_LIMIT,
    MAX_SPARSE_SEARCH_TOP_K, MAX_TRAVERSAL_DEPTH, MAX_TRAVERSAL_LIMIT, MAX_TRAVERSAL_MILLIS,
    MAX_TRAVERSAL_ROOTS, MAX_TRAVERSAL_WORK,
};

pub use khive_types::{
    AgentRecord, AgentState, EdgeCategory, EdgeRelation, EventOutcome, SubstrateKind,
    TerminalReason,
};
