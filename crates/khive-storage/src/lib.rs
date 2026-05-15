//! Storage capability traits — contracts that backend implementations satisfy.
//!
//! This crate contains zero implementations. It defines:
//! - [`SqlAccess`]: base SQL capability (reader / writer / transaction)
//! - [`VectorStore`]: embedding storage and similarity search
//! - [`TextSearch`]: full-text search and document indexing
//! - [`GraphStore`]: directed edge CRUD and graph traversal
//! - [`NoteStore`]: temporal-referential note CRUD
//! - [`EventStore`]: append-only operation log
//! - Shared types ([`SqlValue`], [`VectorSearchHit`], [`TextSearchHit`], etc.)
//! - [`StorageError`]: unified error type

pub mod capability;
pub mod entity;
pub mod error;
pub mod event;
pub mod graph;
pub mod note;
pub mod sql;
pub mod text;
pub mod types;
pub mod vectors;

pub use capability::StorageCapability;
pub use entity::{Entity, EntityFilter, EntityStore};
pub use error::StorageError;

pub use event::{Event, EventFilter, EventStore};
pub use graph::GraphStore;
pub use note::{Note, NoteKind, NoteStore};
pub use sql::{SqlAccess, SqlReader, SqlTransaction, SqlWriter};
pub use text::TextSearch;
pub use types::StorageResult;
pub use vectors::VectorStore;

pub use types::{
    BatchWriteSummary, DeleteMode, Direction, Edge, EdgeFilter, EdgeSortField, GraphPath,
    IndexRebuildScope, LinkId, NeighborHit, NeighborQuery, Page, PageRequest, PathNode,
    SortDirection, SortOrder, SqlIsolation, SqlRow, SqlStatement, SqlTxOptions, SqlValue,
    TextDocument, TextFilter, TextIndexStats, TextQueryMode, TextSearchHit, TextSearchRequest,
    TimeRange, TraversalOptions, TraversalRequest, VectorRecord, VectorSearchHit,
    VectorSearchRequest, VectorStoreInfo,
};

pub use khive_types::{EdgeCategory, EdgeRelation, EntityKind, EventOutcome, SubstrateKind};
