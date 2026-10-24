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
//! - [`ForwardingEventStore`] — the lane-side [`khive_storage::EventStore`] over the socket.
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

#[path = "events_count_read_budget.rs"]
mod events_count_read_budget;
#[doc(hidden)]
pub use events_count_read_budget::WireReadFailure;

#[cfg(unix)]
mod events_daemon;
mod file_hardening;
#[cfg(unix)]
mod forwarder;
mod paths;
mod protocol;
mod registry;
#[cfg(unix)]
mod server;
mod stores;

#[cfg(unix)]
pub(crate) use events_daemon::harness_duration_ms;
#[cfg(unix)]
pub use events_daemon::{
    events_readiness, events_readiness_window, run_events_daemon, run_events_daemon_with_policies,
    supervise_events_daemon, supervise_events_daemon_with_policies,
    supervise_events_daemon_with_readiness, try_acquire_events_daemon_guard, EventsDaemonGuard,
    EventsNotReady, EventsNotReadyReason, EventsReadiness, EventsReadinessSignal,
    EVENTS_READINESS_WINDOW,
};
#[cfg(unix)]
pub use forwarder::{EventsForwardingMetrics, EventsSplitClient};
#[cfg(unix)]
use paths::absolutize;
pub use paths::{
    events_db_path_beside, events_socket_path_beside, EVENTS_PROTOCOL_VERSION,
    MAX_QUERY_EVENTS_PAGE_ROWS,
};
#[cfg(unix)]
pub use paths::{DEFAULT_APPEND_QUEUE_BATCHES, DEFAULT_APPEND_QUEUE_BYTES};
pub use protocol::{EventsRequest, EventsResponse, WireWriterTaskFailure, WireWriterTaskState};
#[cfg(any(test, feature = "test-internals"))]
#[doc(hidden)]
pub use registry::TestRegistryGuard;
#[cfg(unix)]
pub use registry::{client_for, forwarding_metrics};
pub use registry::{direct_backend_for, direct_backend_read_only_for, EventsSplitConfig};
pub(crate) use registry::{direct_backend_with_max_readers, direct_backend_with_policies};
#[cfg(unix)]
pub use stores::ForwardingEventStore;
pub use stores::SplitEventStore;

#[cfg(unix)]
#[path = "events_split_socket_path.rs"]
pub(crate) mod socket_path;
#[cfg(unix)]
pub use socket_path::validate_events_socket_path;

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

#[cfg(all(test, unix))]
#[path = "events_count_read_budget_tests.rs"]
mod count_read_budget_tests;

#[cfg(all(test, unix))]
#[path = "events_split_sqlite_full_tests.rs"]
mod sqlite_full_tests;

#[cfg(test)]
mod tests;
