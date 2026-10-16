//! Vector filter compliance suite (ADR-044 §2).
//!
//! Any backend that sets `supports_filter = true` in its `VectorStoreCapabilities`
//! MUST pass this suite. The suite covers the scenarios listed in ADR-044:
//!   - namespace isolation
//!   - kind gating
//!   - single-property Eq predicate
//!   - multi-property AND
//!   - empty filter delegates to `search`
//!
//! # Usage for backend crates
//!
//! Backend crates that override `search_with_filter` and set `supports_filter = true`
//! should call each `assert_*` helper below against their concrete store implementation
//! inside their own `#[tokio::test]` functions.
//!
//! The helpers in this module test the *default* trait behavior (namespace isolation
//! and kind gating via the default `search_with_filter` delegation logic). Backends
//! with native filter pushdown must run their own integration tests that verify the
//! full SQL pushdown path in addition to calling these helpers.

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, PropertyFilter, PropertyOp, StorageResult,
    VectorIndexKind, VectorMetadataFilter, VectorRecord, VectorSearchHit, VectorSearchRequest,
    VectorStoreInfo,
};
use khive_storage::{StorageError, VectorStore};
use khive_types::SubstrateKind;

include!("compliance/baseline.rs");

include!("compliance/validation.rs");

include!("compliance/boundaries.rs");

include!("compliance/non_finite.rs");
