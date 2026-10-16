//! Integration tests for VectorStore default-method behavior.
//!
//! These tests were moved from `src/vectors.rs` (inline section was 420 lines,
//! exceeding the 300-line gate in the coding standards).

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, OrphanSweepConfig, VectorIndexKind, VectorMetadataFilter,
    VectorSearchHit, VectorSearchRequest, VectorStoreInfo,
};
use khive_storage::{StorageError, VectorStore};
use khive_types::SubstrateKind;

use khive_storage::capability::StorageCapability;
use khive_storage::types::{PropertyFilter, PropertyOp, StorageResult, VectorRecord};

include!("vectors/baseline.rs");

include!("vectors/search_defaults.rs");

include!("vectors/mutation_and_scan.rs");
