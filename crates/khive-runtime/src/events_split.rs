//! Events-daemon split (ADR-170): the audit lane leaves the domain store.
//!
//! The ADR-133 idempotent audit batch is the measured bulk of event write
//! volume, and in a single-store deployment its rows queue on the same SQLite
//! writer lane as domain mutations. This module moves that lane into a
//! dedicated events daemon that owns the events database, reachable over a
//! Unix socket with the same length-prefixed framing and peer-uid admission the
//! main daemon socket uses. Plain event appends stay on the domain store —
//! the legacy `events` table has raw-SQL consumers (schedule provenance, kg
//! projection guards, graph-query substrate unions) whose correctness
//! depends on finding those rows there.
//!
//! Cooperating pieces:
//!
//! - [`run_events_daemon`] — the server loop the `events-daemon` subcommand
//!   runs: binds the events socket, owns the only resident writer of the
//!   events database, and serves append/read requests through the ordinary
//!   `SqlEventStore`.
//! - [`EventsSplitClient`] — one per domain process. Plain appends ride a
//!   bounded in-memory queue drained by a background forwarder
//!   (fire-and-forget; overflow or a dead daemon drops the batch, counts it,
//!   and logs — the loss-tolerant durability class made concrete; unused by
//!   the default routing until telemetry producers opt in). Idempotent
//!   audit-batch appends and reads are synchronous framed round-trips with
//!   bounded timeouts, because their callers are background flushers or query
//!   paths, never the dispatch hot path.
//! - [`ForwardingEventStore`] — the lane-side [`EventStore`] over the socket.
//!   Preflight validation delegates to an in-memory `SqlEventStore`, so the
//!   ADR-133 audit-batch seam keeps its pre-enqueue shape check without any
//!   I/O on the dispatch path.
//! - [`SplitEventStore`] — the per-namespace handle
//!   [`crate::runtime::KhiveRuntime::events`] returns when the split is
//!   configured: routes the idempotent lane to the events store, plain
//!   appends to the legacy store, and merges reads across both.
//!
//! Domain availability never depends on events-daemon liveness: every failure
//! path here degrades (drop + count + log, or a typed storage error for the
//! synchronous lanes) instead of blocking the caller.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use async_trait::async_trait;
#[cfg(unix)]
use khive_db::{StorageBackend, WalCeilingPolicy};
use khive_storage::event::{
    EventGroupBy, EventPageQuery, EventPageWindow, IdempotentEventBatchResult,
};
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageError,
    StorageResult,
};
use serde::{Deserialize, Serialize};

#[path = "events_count_read_budget.rs"]
mod events_count_read_budget;
use events_count_read_budget::CountReadBudget;
#[doc(hidden)]
pub use events_count_read_budget::WireReadFailure;
#[cfg(unix)]
use events_count_read_budget::{count_storage_error_response, restore_count_read_failure};

#[cfg(all(test, unix))]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;
use uuid::Uuid;

#[cfg(unix)]
use crate::daemon::{read_frame, write_frame};

#[cfg(unix)]
mod daemon;
mod file_hardening;
#[cfg(unix)]
mod forwarder;
mod paths;
mod protocol;
mod registry;
#[cfg(unix)]
mod server;
mod stores;

pub use stores::*;

#[cfg(unix)]
pub use forwarder::*;

#[cfg(unix)]
use server::*;

#[cfg(unix)]
pub use daemon::*;

#[cfg(all(test, unix))]
use file_hardening::{
    ensure_events_db_owner_only, ensure_events_db_parent_trusted, events_db_targets,
    harden_events_db_sidecars, verify_events_db_owner_only_unopened, EventsFileIdentity,
};

pub use protocol::*;

pub use registry::*;

pub use paths::*;

#[cfg(unix)]
#[path = "events_split_socket_path.rs"]
pub(crate) mod socket_path;
#[cfg(unix)]
pub use socket_path::validate_events_socket_path;

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[path = "events_split_policy_entries.rs"]
mod policy_entries;
#[cfg(unix)]
pub use policy_entries::{
    run_events_daemon_with_wal_ceiling, supervise_events_daemon_with_wal_ceiling,
};

#[cfg(all(test, unix))]
#[path = "events_wal_policy_tests.rs"]
mod wal_policy_tests;

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The per-namespace EventStore handle
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
#[path = "events_count_read_budget_tests.rs"]
mod count_read_budget_tests;

#[cfg(test)]
#[path = "events_split_sqlite_full_tests.rs"]
mod sqlite_full_tests;

#[cfg(test)]
mod tests;
