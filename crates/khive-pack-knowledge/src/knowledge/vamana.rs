// FILE SIZE JUSTIFICATION: This module exceeds the 700-line soft target because it owns
// the complete Vamana ANN lifecycle for knowledge search: SharedAnn type, AnnKey, snapshot
// persistence (warm_known_snapshots / ensure_ann_background), index build (build_ann),
// search (search_loaded_with_seq plus the fresh-tail exact leg), and all associated SQL
// queries and serialization logic. These responsibilities are tightly coupled through the
// shared AnnState and its generation-fenced warm/install protocol; splitting them would obscure
// the lock ordering and ownership contract.

//! Vamana ANN bridge — parallel semantic signal for `knowledge.search`.
//!
//! Wraps `khive_vamana::VamanaIndex` with an ID map (u32 → UUID) so search
//! results can be fused with FTS5 candidates via RRF. Persistence (ADR-079,
//! Amendment 1): v2 binary segments under `<db-file>.ann/<hex>/`, restored
//! through the write-log restart classifier, falling back to legacy v1
//! JSON snapshot rows, then a full corpus rebuild on cache-miss. See
//! crates/khive-pack-knowledge/docs/api/vamana.md for the persistence
//! fallback chain and the file-size/module-coupling rationale.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use khive_retrieval::ann::corpus::{CorpusScope, WatermarkCapture};
use khive_retrieval::ann::registry::{self as ann_registry, WatermarkAuthority};
use khive_runtime::config::ann_rebuild_threshold_from_env as ann_rebuild_threshold;
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::snapshot_invalidation::invalidate_legacy_vamana_snapshots;
use khive_vamana::bridge::AnnBridgeCore;
use khive_vamana::distance::l2_normalize;
use khive_vamana::{
    read_commit_info, segment_commit_digest, CorpusFingerprint, VamanaIndex, VamanaSnapshot,
};
use tokio::sync::RwLock;
use uuid::Uuid;

// Reached by the test modules through `use super::*`.
#[cfg(test)]
use khive_vamana::{write_external_ids_sidecar, VamanaConfig};

// ── persistence helpers ───────────────────────────────────────────────────────

mod bridge_impl;
mod fresh_tail;
mod persistence;
mod segment_key;
mod snapshot;
mod state;
mod warm_and_rotate;

use bridge_impl::*;
pub(crate) use fresh_tail::*;
pub(crate) use persistence::*;
pub(crate) use snapshot::*;
pub(crate) use state::*;
pub(crate) use warm_and_rotate::*;

pub(crate) use segment_key::snapshot_key;
use segment_key::{ann_segment_dir, ann_segment_dir_from_root, decode_ann_dir_name};

pub(crate) use khive_runtime::config::sanitize_key as sanitize_model_key;

#[cfg(test)]
#[path = "vamana_owned_build_tests.rs"]
mod owned_build_tests;

#[cfg(test)]
#[path = "vamana_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "vamana_corpus_statement_tests.rs"]
mod corpus_statement_tests;

#[cfg(test)]
#[path = "vamana_corpus_capture_tests.rs"]
mod corpus_capture_tests;
