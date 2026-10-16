//! Warm ANN bridge: wraps `VamanaIndex` per model to cache memory-note vector search.
//! One index per model covers all namespaces; namespace filtering is applied at recall time.
//! See `crates/khive-pack-memory/docs/api/ann-lifecycle.md` for lifecycle and race handling,
//! and `crates/khive-pack-memory/docs/ann.md` for the restart classifier and ADR-118 design.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use khive_retrieval::ann::corpus::{CorpusScope, LiveRowJoin, WatermarkCapture};
use khive_retrieval::ann::registry::{self as ann_registry, WatermarkAuthority, PENDING_WATERMARK};
use khive_runtime::config::ann_rebuild_threshold_from_env as ann_rebuild_threshold;
use khive_runtime::{
    is_benign_shutdown_cancellation, KhiveRuntime, Namespace, NamespaceToken, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::StorageError;
use khive_vamana::bridge::AnnBridgeCore;
use khive_vamana::distance::l2_normalize;
use khive_vamana::{read_commit_info, segment_commit_digest, CorpusFingerprint};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

// Reached by the test modules through `use super::*`.
#[cfg(test)]
use khive_vamana::write_external_ids_sidecar;

#[cfg(test)]
tokio::task_local! {
    static SESSION_EXACT_STATEMENT_COUNT: std::cell::Cell<usize>;
    static SESSION_FENCE_PROBE_COUNT: std::cell::Cell<usize>;
}

#[cfg(test)]
pub(crate) async fn count_session_statements<F: std::future::Future>(
    future: F,
) -> (F::Output, usize, usize) {
    SESSION_EXACT_STATEMENT_COUNT
        .scope(std::cell::Cell::new(0), async {
            SESSION_FENCE_PROBE_COUNT
                .scope(std::cell::Cell::new(0), async {
                    let output = future.await;
                    let probes = SESSION_FENCE_PROBE_COUNT.with(std::cell::Cell::get);
                    let exact = SESSION_EXACT_STATEMENT_COUNT.with(std::cell::Cell::get);
                    (output, probes, exact)
                })
                .await
        })
        .await
}

#[path = "ann/incremental.rs"]
mod incremental;
use incremental::*;
#[path = "ann/checkpoint_timer.rs"]
mod checkpoint_timer;
#[path = "ann/delta.rs"]
mod delta;
#[path = "ann/final_tail.rs"]
mod final_tail;
use final_tail::fetch_final_tail_on;
#[path = "ann/epoch.rs"]
mod epoch;
pub(crate) use epoch::{
    bump_durable_epoch, durable_epoch, ensure_epoch_schema, maybe_check_durable_epoch,
    MEMORY_SCHEMA_PLAN_STMTS,
};
#[path = "ann/scoring.rs"]
mod scoring;
pub(crate) use scoring::{AnnScoreRoute, FreshTailSearch};
#[path = "ann/session_exact.rs"]
mod session_exact;
pub(crate) use session_exact::session_exact_candidates;
#[cfg(test)]
#[path = "ann/rotation_watch_test_support.rs"]
mod rotation_watch_test_support;
#[cfg(test)]
pub(crate) use rotation_watch_test_support::take_rotation_watch_handle_for_test;

mod bridge_impl;
mod checkpoint;
mod fresh_tail;
mod loaded_index;
mod state;
mod warm;
mod watermark_log;

use checkpoint::*;
pub(crate) use fresh_tail::*;
pub(crate) use loaded_index::*;
pub(crate) use state::*;
pub(crate) use warm::*;
pub(crate) use watermark_log::*;

// ── helpers ───────────────────────────────────────────────────────────────────

pub(crate) use khive_runtime::config::sanitize_key as sanitize_model_key;

// Runtime owns the shared typed shutdown-cancellation classifier.

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "ann_owned_build_tests.rs"]
mod owned_build_tests;

#[cfg(test)]
#[path = "ann_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ann/bridge_incremental_tests.rs"]
mod bridge_incremental_tests;

#[cfg(test)]
#[path = "ann/corpus_statement_tests.rs"]
mod corpus_statement_tests;

#[cfg(test)]
#[path = "ann/corpus_capture_tests.rs"]
mod corpus_capture_tests;
