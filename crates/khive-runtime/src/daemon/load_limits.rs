//! Load limits for the daemon: the cap on concurrently served connections with its
//! busy refusal, and the bound on recall serve-ledger tasks.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tokio::net::UnixStream;

use super::spawn_named_tracked_task;
#[cfg(unix)]
use super::{write_frame, DaemonResponseFrame, PROTOCOL_VERSION};

// ── connection admission ──────────────────────────────────────────────────────

/// Environment variable that sets the cap on concurrently served connections.
/// A zero or non-integer value is ignored with a warning and the default applies.
#[cfg(unix)]
const MAX_CONNECTIONS_ENV: &str = "KHIVE_DAEMON_MAX_CONNECTIONS";

/// Default cap on concurrently served connections, used when
/// `KHIVE_DAEMON_MAX_CONNECTIONS` is unset. Every open connection holds a task
/// and a file descriptor until its peer closes it, and clients open one
/// connection per request, so the default is far above the concurrency of an
/// ordinary client population. The cap that is enforced is this value, or the
/// configured one, reduced to fit the process's descriptor limit (see
/// [`effective_connection_cap`]); a daemon whose limit leaves room for it
/// behaves exactly as it did before the cap existed.
#[cfg(unix)]
const DEFAULT_MAX_CONNECTIONS: usize = 512;

/// Descriptors the daemon holds for purposes other than serving a connection.
/// They are kept out of the connection cap so that the cap is reached before the
/// process runs out of descriptors. A running daemon measured at rest held 160:
/// 130 on five SQLite database files (each backend pool holds a writer, up to 8
/// readers and one checkpoint connection, each with a database and a WAL
/// descriptor, plus one shared-memory descriptor per file) and 30 on the
/// listener and its peers, event-loop handles, log files, ANN and model files
/// and shared libraries. 192 is those 160 plus 32 for growth (one more database
/// file holds about 25) and for descriptors that a request opens while it runs
/// (outbound sockets, child-process pipes, blob reads), which are not measured.
#[cfg(unix)]
const RESERVED_DESCRIPTORS: u64 = 192;

/// Where the enforced connection cap came from.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CapSource {
    /// `DEFAULT_MAX_CONNECTIONS`.
    Builtin,
    /// `KHIVE_DAEMON_MAX_CONNECTIONS`.
    Configured,
    /// Reduced to what the descriptor limit leaves after `RESERVED_DESCRIPTORS`.
    DescriptorLimit,
}

/// Longest the accept loop waits to write a busy refusal. The frame is a few
/// hundred bytes written into the empty buffer of a connection that has not been
/// sent anything yet, so a live peer never reaches this bound.
#[cfg(unix)]
const BUSY_REFUSAL_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Parse a limit that must be a positive integer. Absent, zero, negative and
/// non-integer values are all `None`.
pub(super) fn parse_positive_limit(raw: Option<&str>) -> Option<u64> {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
}

/// Read a positive limit from the environment. An unset variable is `None`
/// silently; a set but unusable value is `None` with a warning, so the caller's
/// default applies and the operator is told the setting was ignored.
fn positive_limit_from_env(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    let parsed = parse_positive_limit(Some(&raw));
    if parsed.is_none() {
        tracing::warn!(
            variable = name,
            value = %raw,
            "ignoring a limit that is not a positive integer; the default applies"
        );
    }
    parsed
}

/// Like [`positive_limit_from_env`], as a `usize`.
#[cfg(unix)]
fn positive_usize_from_env(name: &str) -> Option<usize> {
    let value = positive_limit_from_env(name)?;
    Some(usize::try_from(value).unwrap_or(usize::MAX))
}

/// The process's soft `RLIMIT_NOFILE`, or `None` when the call fails.
#[cfg(unix)]
pub(super) fn soft_nofile_limit() -> Option<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable rlimit for the duration of the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return None;
    }
    // `rlim_t` varies in width and signedness across Unix targets. The cast gives
    // the u64 the cap arithmetic uses even where it is redundant.
    #[allow(clippy::unnecessary_cast)]
    let soft = limit.rlim_cur as u64;
    Some(soft)
}

/// The connection cap to enforce: the configured value (or the default),
/// reduced to `soft_nofile - RESERVED_DESCRIPTORS` when that is smaller, and
/// never below 1. `soft_nofile` is the process's soft descriptor limit, or
/// `None` when it could not be read, in which case the configured or default
/// value stands.
#[cfg(unix)]
pub(super) fn effective_connection_cap(
    configured: Option<usize>,
    soft_nofile: Option<u64>,
) -> (usize, CapSource) {
    let (wanted, source) = match configured {
        Some(value) => (value, CapSource::Configured),
        None => (DEFAULT_MAX_CONNECTIONS, CapSource::Builtin),
    };
    let room = match soft_nofile {
        Some(soft) => soft.saturating_sub(RESERVED_DESCRIPTORS),
        None => u64::MAX,
    };
    let room = usize::try_from(room).unwrap_or(usize::MAX);
    let reduced = room < wanted;
    let cap = if reduced { room } else { wanted };
    let source = if reduced {
        CapSource::DescriptorLimit
    } else {
        source
    };
    (cap.max(1), source)
}

/// Admission control for new connections on the daemon socket: one permit per
/// connection, taken before the connection task is spawned and released when
/// the task ends. Admission never waits, so a connection past the cap is
/// refused at once instead of queueing work behind it.
#[cfg(unix)]
pub(super) struct ConnectionAdmission {
    /// The cap that is enforced.
    limit: usize,
    /// The configured or default cap, present only when the process's
    /// descriptor limit forced `limit` below it.
    configured: Option<usize>,
    permits: Arc<tokio::sync::Semaphore>,
    refused: std::sync::atomic::AtomicU64,
}

#[cfg(unix)]
impl ConnectionAdmission {
    pub(super) fn new(limit: usize) -> Self {
        let limit = limit.clamp(1, tokio::sync::Semaphore::MAX_PERMITS);
        Self {
            limit,
            configured: None,
            permits: Arc::new(tokio::sync::Semaphore::new(limit)),
            refused: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Start-up path: the enforced cap is [`effective_connection_cap`] of the
    /// configured value and the soft descriptor limit. A cap reduced to fit the
    /// limit, or a limit that could not be read, is logged once here.
    pub(super) fn from_limits(configured: Option<usize>, soft_nofile: Option<u64>) -> Self {
        let (limit, source) = effective_connection_cap(configured, soft_nofile);
        let mut admission = Self::new(limit);
        match soft_nofile {
            None => tracing::warn!(
                cap = admission.limit,
                "could not read the descriptor limit; the connection cap is not reduced to fit it"
            ),
            Some(soft) if source == CapSource::DescriptorLimit => {
                let wanted = configured.unwrap_or(DEFAULT_MAX_CONNECTIONS);
                admission.configured = Some(wanted);
                tracing::warn!(
                    configured_cap = wanted,
                    effective_cap = admission.limit,
                    soft_nofile = soft,
                    reserved_descriptors = RESERVED_DESCRIPTORS,
                    "connection cap reduced to fit the process descriptor limit"
                );
            }
            Some(_) => {}
        }
        admission
    }

    pub(super) fn from_env() -> Self {
        Self::from_limits(
            positive_usize_from_env(MAX_CONNECTIONS_ENV),
            soft_nofile_limit(),
        )
    }

    /// Take a permit for one connection. `Err` carries the running count of
    /// refused connections, this one included.
    pub(super) fn try_admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, u64> {
        Arc::clone(&self.permits).try_acquire_owned().map_err(|_| {
            self.refused
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1
        })
    }

    pub(super) fn snapshot(&self) -> ConnectionCapSnapshot {
        ConnectionCapSnapshot {
            limit: self.limit,
            configured: self.configured,
            active: self.limit - self.permits.available_permits(),
            refused: self.refused.load(std::sync::atomic::Ordering::SeqCst),
        }
    }
}

/// Admit one accepted connection, or answer it with a typed busy refusal.
///
/// Past the cap the peer receives a `daemon_busy` error frame (`NotCommitted`,
/// carrying the enforced limit) and the caller drops the stream, which closes
/// it. The refusal writes to the stream that `accept` already returned, so it
/// needs no further descriptor. The request frame is not read, so nothing of
/// it is dispatched. Connections that already hold a permit are not touched.
/// The refusal is logged when the running refusal count reaches a power of two,
/// which keeps a connection flood from flooding the log while still reporting
/// that it began.
#[cfg(unix)]
pub(super) async fn admit_or_refuse_busy(
    admission: &ConnectionAdmission,
    stream: &mut UnixStream,
    config_id: &str,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let refused_total = match admission.try_admit() {
        Ok(permit) => return Some(permit),
        Err(refused_total) => refused_total,
    };
    if refused_total.is_power_of_two() {
        tracing::warn!(
            cap = admission.limit,
            refused_total,
            "daemon at connection cap; refusing new connections"
        );
    }
    let limit = admission.limit;
    let refusal = DaemonResponseFrame {
        ok: false,
        result: None,
        error: Some(format!(
            "daemon is at its connection limit ({limit}); request was not admitted, retry shortly"
        )),
        error_detail: Some(serde_json::json!({
            "kind": "runtime", "code": "daemon_busy", "limit": limit,
            "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
        })),
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(config_id.to_owned()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    };
    if let Ok(payload) = serde_json::to_vec(&refusal) {
        let write = write_frame(stream, &payload);
        match tokio::time::timeout(BUSY_REFUSAL_WRITE_TIMEOUT, write).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::debug!(%error, "failed to write connection-limit refusal"),
            Err(_) => tracing::debug!("connection-limit refusal write timed out"),
        }
    }
    None
}

/// Connection cap state of the main daemon socket: the cap that is enforced,
/// the connections holding a slot right now, and how many connections have been
/// refused with the busy answer since the daemon started.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionCapSnapshot {
    /// The enforced cap, after any reduction to fit the process's descriptor limit.
    pub limit: usize,
    /// The configured or default cap, present only when the descriptor limit
    /// forced `limit` below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured: Option<usize>,
    pub active: usize,
    pub refused: u64,
}

// ── bounded recall serve-ledger tasks ────────────────────────────────────────

/// Environment variable that sets how many recall serve-ledger tasks may be
/// pending at once. A zero or non-integer value is ignored with a warning and
/// the default applies.
const RECALL_LEDGER_MAX_PENDING_ENV: &str = "KHIVE_RECALL_LEDGER_MAX_PENDING";

/// Environment variable that sets, in milliseconds, how long a recall
/// serve-ledger task may run before it is ended. A zero or non-integer value is
/// ignored with a warning and the default applies.
const RECALL_LEDGER_TIMEOUT_MS_ENV: &str = "KHIVE_RECALL_LEDGER_TIMEOUT_MS";

/// Default bound on pending recall serve-ledger tasks: the capacity of the
/// database write queue they write through, so the tasks cannot pile up faster
/// than the queue can absorb them.
const DEFAULT_RECALL_LEDGER_MAX_PENDING: usize = 256;

/// Default completion timeout for one recall serve-ledger task. A task normally
/// finishes in milliseconds, so this only ends a task whose write is stalled.
const DEFAULT_RECALL_LEDGER_TIMEOUT_MS: u64 = 30_000;

/// The recall serve ledger and its telemetry event are best-effort records
/// written after the recall response is built. This bounds how many such tasks
/// are pending and how long each may run, so a stalled writer cannot accumulate
/// tasks without limit. A recall never waits on this: admission is a non-blocking
/// permit, and a write that is skipped at the limit or ended at its timeout is
/// counted instead of retried.
pub(super) struct RecallLedgerBound {
    max_pending: usize,
    timeout: std::time::Duration,
    permits: Arc<tokio::sync::Semaphore>,
    skipped: std::sync::atomic::AtomicU64,
    timed_out: std::sync::atomic::AtomicU64,
}

impl RecallLedgerBound {
    pub(super) fn new(max_pending: usize, timeout: std::time::Duration) -> Self {
        let max_pending = max_pending.clamp(1, tokio::sync::Semaphore::MAX_PERMITS);
        Self {
            max_pending,
            timeout,
            permits: Arc::new(tokio::sync::Semaphore::new(max_pending)),
            skipped: std::sync::atomic::AtomicU64::new(0),
            timed_out: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn from_env() -> Self {
        let max_pending = match positive_limit_from_env(RECALL_LEDGER_MAX_PENDING_ENV) {
            Some(value) => usize::try_from(value).unwrap_or(usize::MAX),
            None => DEFAULT_RECALL_LEDGER_MAX_PENDING,
        };
        let timeout_ms = positive_limit_from_env(RECALL_LEDGER_TIMEOUT_MS_ENV)
            .unwrap_or(DEFAULT_RECALL_LEDGER_TIMEOUT_MS);
        Self::new(max_pending, std::time::Duration::from_millis(timeout_ms))
    }

    /// Start `fut` as a tracked task when a pending slot is free, ending it if
    /// it has not completed within the timeout. At the limit nothing is spawned
    /// and the skipped write is counted. Never waits.
    pub(super) fn try_spawn<F>(self: &Arc<Self>, fut: F) -> Option<tokio::task::JoinHandle<()>>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            self.skipped
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return None;
        };
        let bound = Arc::clone(self);
        let handle = spawn_named_tracked_task("memory_recall_serve_ledger", async move {
            // Released when the task ends, completed or timed out.
            let _permit = permit;
            if tokio::time::timeout(bound.timeout, fut).await.is_err() {
                bound
                    .timed_out
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tracing::warn!(
                    timeout_ms = bound.timeout.as_millis() as u64,
                    "recall serve ledger task timed out; the ledger write was missed"
                );
            }
        });
        Some(handle)
    }

    pub(super) fn snapshot(&self) -> RecallLedgerSnapshot {
        RecallLedgerSnapshot {
            max_pending: self.max_pending,
            pending: self.max_pending - self.permits.available_permits(),
            timeout_ms: self.timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            skipped: self.skipped.load(std::sync::atomic::Ordering::SeqCst),
            timed_out: self.timed_out.load(std::sync::atomic::Ordering::SeqCst),
        }
    }
}

fn recall_ledger() -> &'static Arc<RecallLedgerBound> {
    static BOUND: std::sync::OnceLock<Arc<RecallLedgerBound>> = std::sync::OnceLock::new();
    BOUND.get_or_init(|| Arc::new(RecallLedgerBound::from_env()))
}

/// Run a recall serve-ledger task under the process-wide bound. Returns at once:
/// the task is either started as a tracked background task or skipped and
/// counted when the pending limit is reached.
pub fn track_recall_ledger_task<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    drop(recall_ledger().try_spawn(fut));
}

/// Current bounds and counts of the recall serve-ledger tasks.
pub fn recall_ledger_snapshot() -> RecallLedgerSnapshot {
    recall_ledger().snapshot()
}

/// State of the recall serve-ledger task bound: the configured pending limit
/// and completion timeout, the tasks pending right now, and the ledger writes
/// that did not complete. A write is missed when it was skipped at the limit
/// (`skipped`) or ended at its timeout (`timed_out`).
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallLedgerSnapshot {
    pub max_pending: usize,
    pub pending: usize,
    pub timeout_ms: u64,
    pub skipped: u64,
    pub timed_out: u64,
}
