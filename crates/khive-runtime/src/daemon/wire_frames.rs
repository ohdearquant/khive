use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(doc)]
use super::PROTOCOL_VERSION;
use super::{ConnectionCapSnapshot, DaemonLifecycleSnapshot, RecallLedgerSnapshot};
#[cfg(unix)]
use super::{DAEMON_LEXICAL_TIMEOUT_MARKER, MAX_FRAME_BYTES};

/// Request frame sent from a client to the daemon.
#[derive(Serialize, Deserialize, Default)]
pub struct DaemonRequestFrame {
    pub ops: String,
    /// Parse and inspect the catalog without dispatch, identity, or storage access.
    #[serde(default)]
    pub plan: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation_per_op: Option<Vec<Option<String>>>,
    /// The client's resolved storage/gate default namespace for this request.
    ///
    /// As of protocol version 3 (ADR-096) the daemon serves the request under
    /// this namespace instead of rejecting on mismatch: a per-request
    /// identity input, not a same-process-identity assertion.
    pub namespace: String,
    /// The client's resolved write-stamp / gate actor identity (ADR-057),
    /// carried on the frame so the warm daemon stamps writes with the
    /// *caller's* actor instead of its own baked `actor_id` (ADR-096). `None`
    /// mints `ActorRef::anonymous()`, matching an unconfigured actor.
    #[serde(default)]
    pub actor_id: Option<String>,
    /// Opaque process provenance resolved in the originating client process.
    /// It is carried per request because a shared warm daemon's environment
    /// does not identify the worker that submitted the operation. Protocol v4
    /// makes this field part of dispatch semantics: a v3 daemon must reject the
    /// request rather than execute it while silently discarding provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_ref: Option<String>,
    /// The client's resolved extra read-visibility namespaces (ADR-007 Rule
    /// 3b), carried on the frame so the warm daemon widens read scope to
    /// match the caller's own configuration rather than its own baked
    /// `visible_namespaces` (ADR-096). A non-`local` `actor_id` joins default
    /// reads where the registry mints the token (ADR-007 Rev 4 Rule 3b), so
    /// an empty list still includes that actor in default reads. Explicit
    /// `namespace=` operations remain scoped to exactly that namespace.
    #[serde(default)]
    pub visible_namespaces: Vec<String>,
    /// Fingerprint of the client's engine-coherence config: packs, db target,
    /// embedders, backend routing, and construction-baked outbound policy.
    /// Identity fields are carried separately in this frame. The daemon rejects
    /// requests whose configuration differs, except when its extra-embedder set
    /// is a superset of the client's and every other field matches. See
    /// ADR-027 / ADR-049 / ADR-096.
    #[serde(default)]
    pub config_id: String,
    /// IPC protocol version sent by the client. Pre-versioning clients omit
    /// this field (deserializes to 0). The daemon compares against
    /// [`PROTOCOL_VERSION`] and rejects mismatches with an explicit error.
    #[serde(default)]
    pub protocol_version: u32,
    /// When `true`, the daemon returns an identity frame (ok=true, result=None)
    /// immediately after identity validation — without calling the dispatcher.
    /// Used by the client's under-lock recovery probe to confirm a daemon is
    /// alive and identity-matching without dispatching any mutating verb.
    /// Pre-probe clients omit this field (deserializes to false → normal dispatch).
    #[serde(default)]
    pub probe_only: bool,
    /// When `true`, the daemon returns a point-in-time [`MetricsSnapshot`] of
    /// its server-side gauges (a read-only measurement surface for the
    /// load/perf harness) instead of dispatching any op. Handled before the
    /// `config_id` equality reject: a gauge read is process-global and
    /// namespace/config-agnostic, not a namespaced record operation.
    /// READ-ONLY — this field is the only input the frame accepts for a
    /// metrics request; there is no reset or mutation reachable over the
    /// wire. Pre-metrics clients omit this field (deserializes to `false` →
    /// normal dispatch, unaffected).
    #[serde(default)]
    pub metrics_only: bool,
    /// Output format for this request (ADR-078). Forwarded to the daemon's
    /// serialization seam. `None` means use the daemon's resolved default.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Per-operation output format overrides (ADR-078).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format_per_op: Option<Vec<Option<String>>>,
    /// Whether this request originated from the agent-facing MCP `request`
    /// tool (the wire surface). When `true`, the daemon rejects
    /// `Visibility::Subhandler` verbs: agents must not invoke internal
    /// subhandlers. When `false` (the default, and the only value any
    /// operator path sends), subhandlers are allowed: `kkernel exec` and
    /// other in-process callers are trusted operator surfaces.
    ///
    /// This is the origin discriminator, not a daemon-vs-local one: operator
    /// requests flow through the daemon by default too, so the gate cannot
    /// key on transport.
    #[serde(default)]
    pub from_wire: bool,
    /// Request-group correlation id (khive#948), echoed back unchanged on
    /// [`DaemonResponseFrame::request_id`] and stamped into the dispatch's
    /// audit event (`resource.request_id`) so a benchmark harness can join
    /// its own pre-send sample to the server-side audit row for the same
    /// request. Agent-facing MCP requests always carry one: the bridge keeps a
    /// caller-supplied value or mints an opaque nonzero value when absent.
    /// Operator-built/probe frames may still use `None`. Purely additive —
    /// `#[serde(default)]` matches `metrics_only`/`format`/`format_per_op`
    /// precedent, with no `PROTOCOL_VERSION` bump.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
}

/// A dispatch failure whose domain outcome remains available to the transport.
#[derive(Debug, Clone)]
pub struct DaemonDispatchError {
    pub message: String,
    pub error_detail: serde_json::Value,
}

/// Per-field container limit, asserted equal to the request parser's bound by MCP.
pub const ERROR_DETAIL_NESTING_DEPTH_LIMIT: usize = 64;

fn error_detail_value_within_limit(value: &serde_json::Value) -> bool {
    let mut pending = vec![(value, 0_usize)];
    while let Some((value, depth)) = pending.pop() {
        match value {
            serde_json::Value::Array(items) if depth < ERROR_DETAIL_NESTING_DEPTH_LIMIT => {
                pending.extend(items.iter().map(|child| (child, depth + 1)));
            }
            serde_json::Value::Object(fields) if depth < ERROR_DETAIL_NESTING_DEPTH_LIMIT => {
                pending.extend(fields.values().map(|child| (child, depth + 1)));
            }
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => return false,
            _ => {}
        }
    }
    true
}

fn drop_error_detail_iteratively(value: serde_json::Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Array(items) => pending.extend(items),
            serde_json::Value::Object(fields) => pending.extend(fields.into_values()),
            _ => {}
        }
    }
}

impl DaemonDispatchError {
    /// Missing or unrecognized disposition from a legacy implementation is unknown.
    pub fn new(message: impl Into<String>, error_detail: Option<serde_json::Value>) -> Self {
        let message = message.into();
        let mut fields = match error_detail {
            Some(serde_json::Value::Object(fields)) => fields,
            Some(data) => serde_json::Map::from_iter([("data".to_string(), data)]),
            None => serde_json::Map::new(),
        };
        let disposition = match fields
            .get("domain_disposition")
            .and_then(serde_json::Value::as_str)
        {
            Some("committed") => crate::DomainDisposition::Committed,
            Some("not_committed") => crate::DomainDisposition::NotCommitted,
            _ => crate::DomainDisposition::Unknown,
        };
        if disposition != crate::DomainDisposition::Committed {
            if let Some(result) = fields.remove("domain_result") {
                drop_error_detail_iteratively(result);
            }
        }
        let rejected: Vec<String> = fields
            .iter()
            .filter(|(_, value)| !error_detail_value_within_limit(value))
            .map(|(name, _)| name.clone())
            .collect();
        let omitted_result = rejected.iter().any(|name| name == "domain_result");
        let omitted_detail = !rejected.is_empty();
        for name in rejected {
            if let Some(value) = fields.remove(&name) {
                drop_error_detail_iteratively(value);
            }
        }
        let mut error_detail = serde_json::Value::Object(fields);
        if error_detail["kind"].as_str().is_none() {
            error_detail["kind"] = serde_json::json!("internal");
        }
        if error_detail["message"].as_str().is_none() {
            error_detail["message"] = serde_json::json!(message);
        }
        error_detail["domain_disposition"] = serde_json::json!(disposition.as_str());
        if omitted_detail {
            error_detail["code"] = serde_json::json!(if omitted_result {
                "result_too_deep"
            } else {
                "error_detail_too_deep"
            });
        }
        Self {
            message,
            error_detail,
        }
    }
}

/// Response frame sent from the daemon back to a client.
#[derive(Serialize, Deserialize, Debug)]
pub struct DaemonResponseFrame {
    pub ok: bool,
    pub result: Option<String>,
    pub error: Option<String>,
    /// Additive failure metadata; legacy protocol-v4 peers still read `error` as text.
    /// On a successful response, `{"lexical_timeout":true}` is a daemon-only
    /// diagnostic that old clients ignore and new clients log locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<serde_json::Value>,
    pub namespace_mismatch: bool,
    /// Set when the request's `config_id` does not match the daemon's. Like
    /// `namespace_mismatch`, this signals the client to fall back to local
    /// dispatch rather than execute under a different runtime/config.
    #[serde(default)]
    pub config_mismatch: bool,
    /// The `config_id` the daemon dispatched under, echoed back so the client
    /// can positively confirm the result came from a matching runtime. A
    /// pre-`config_id` daemon omits this field (deserializes to `None`), which
    /// the client treats as a mismatch and falls back to local dispatch — this
    /// closes the upgrade window where a new restricted client could otherwise
    /// trust a still-warm legacy daemon's broader registry.
    #[serde(default)]
    pub served_config_id: Option<String>,
    /// Set when the client's `protocol_version` does not match the daemon's
    /// [`PROTOCOL_VERSION`]. The client must treat this as a hard error and
    /// surface the human-readable `error` field rather than falling back to
    /// local dispatch (which would hide the version skew).
    #[serde(default)]
    pub version_mismatch: bool,
    /// The daemon's [`PROTOCOL_VERSION`], echoed in error responses so the
    /// client can include both sides in the diagnostic message. Pre-versioning
    /// daemons omit this field (deserializes to 0).
    #[serde(default)]
    pub daemon_protocol_version: u32,
    /// Populated when the request set `metrics_only: true`: a point-in-time
    /// snapshot of the daemon's server-side gauges. `None` on every other
    /// response, and on any response from a daemon that predates this field
    /// (client-side back-compat via `#[serde(default)]`, matching
    /// `served_config_id`'s upgrade-window handling above).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsSnapshot>,
    /// Echo of the request's `request_id` (khive#948), present whenever the
    /// frame that produced this response carried one — including on every
    /// error/denied arm, not only success, so a client can join a failure
    /// the same way it joins a success. `#[serde(default)]` so an older
    /// daemon's response (predating this field) deserializes to `None`
    /// rather than a parse error.
    #[serde(default)]
    pub request_id: Option<u64>,
}

/// Move the private dispatch signal out of the result before any client can
/// observe it. Unmarked results retain their exact bytes. The marked result
/// was serialized from a JSON Value by the MCP server, so reserializing after
/// removal reproduces its public envelope. The marker's escaped frame cost
/// equals `error_detail:{"lexical_timeout":true}`, keeping the server's exact
/// frame-fit calculation valid after this move.
#[cfg(unix)]
pub(super) fn take_daemon_lexical_timeout_marker(
    raw: String,
) -> (String, Option<serde_json::Value>) {
    if !raw.contains(DAEMON_LEXICAL_TIMEOUT_MARKER) {
        return (raw, None);
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return (raw, None);
    };
    let Some(fields) = value.as_object_mut() else {
        return (raw, None);
    };
    if !fields
        .get("results")
        .is_some_and(serde_json::Value::is_array)
    {
        return (raw, None);
    }
    let Some(marker) = fields.remove(DAEMON_LEXICAL_TIMEOUT_MARKER) else {
        return (raw, None);
    };
    let detail =
        (marker.as_bool() == Some(true)).then(|| serde_json::json!({"lexical_timeout": true}));
    (
        serde_json::to_string(&value).expect("serde_json::Value is serializable"),
        detail,
    )
}

/// One checkpoint store in this daemon's fixed topology. IDs are process-local:
/// `main` or `secondary:<index>` in dispatcher order. The basename is display-only,
/// not an identity; no directory path is exposed. Restart/topology changes reset
/// the interpretation of interval deltas.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct CheckpointStoreMetrics {
    pub store_id: String,
    pub role: String,
    pub database: Option<String>,
    #[serde(flatten)]
    pub timing: khive_db::checkpoint::CheckpointTiming,
}

/// Point-in-time snapshot of the daemon's server-side gauges — the
/// load/perf harness read-surface (measurement substrate, not a product feature).
///
/// Every field here is a **server-side** gauge reachable from `handle_conn`
/// without any mutation: [`khive_storage::tx_registry`] (ADR-091 Plank 0,
/// process-global singleton), the main pool's backend-keyed routine WAL
/// sample and process TRUNCATE counters (`khive_db::checkpoint`), and the
/// ADR-067 Component A write queue depth/latest writer-stage sample. There is
/// no reset reachable through this type or through [`DaemonRequestFrame`] —
/// gauges out, nothing in.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct MetricsSnapshot {
    /// Launch mode and the current lifecycle of this daemon incarnation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<DaemonLifecycleSnapshot>,
    /// Last-observed WAL page count from the periodic checkpoint tick.
    /// `None` when the checkpoint task has never ticked in this process
    /// (for example, an in-memory dispatcher with no pool, or a daemon that
    /// just started and hasn't hit its first tick yet).
    pub wal_pages: Option<u64>,
    /// Logical frames present in the WAL at the main backend's most recent
    /// periodic PASSIVE checkpoint. Kept separate from physical allocation.
    #[serde(default)]
    pub wal_log_frames: Option<u64>,
    /// Frames backfilled by that same periodic PASSIVE pass.
    #[serde(default)]
    pub wal_checkpointed_frames: Option<u64>,
    /// Logical frames still pending after that pass (`log - checkpointed`).
    #[serde(default)]
    pub wal_pending_frames: Option<u64>,
    /// Physical `-wal` sidecar bytes captured at the same routine tick.
    #[serde(default)]
    pub wal_physical_bytes: Option<u64>,
    /// Wall-clock timestamp of the routine WAL sample, for staleness checks.
    #[serde(default)]
    pub wal_observed_at_unix_ms: Option<u64>,
    /// Cumulative actual routine PASSIVE calls for each store this daemon
    /// checkpoints. Microseconds preserve sub-millisecond costs; max is since
    /// process start, not an interval maximum. Counters saturate rather than wrap.
    #[serde(default)]
    pub wal_checkpoint_stores: Vec<CheckpointStoreMetrics>,
    /// Total WAL TRUNCATE escalation attempts (ADR-091 Plank 2) made in this
    /// process's lifetime, regardless of whether they succeeded in reclaiming
    /// pages.
    pub wal_truncate_attempts: u64,
    /// Current consecutive-failure count for TRUNCATE attempts that failed to
    /// bring the WAL back below `warn_pages`; resets to 0 the next time an
    /// attempt clears it.
    pub wal_truncate_consecutive_failures: u64,
    /// Total checkpoint ticks skipped because the dedicated checkpoint
    /// connection was unavailable (ADR-091 checkpoint-pressure telemetry),
    /// across this process's lifetime. `#[serde(default)]` so an older client
    /// decoding a newer daemon's snapshot (or vice versa) does not fail.
    #[serde(default)]
    pub wal_checkpoint_skipped_ticks: u64,
    /// Current consecutive-skip run length; 0 once the next tick is observed.
    #[serde(default)]
    pub wal_checkpoint_consecutive_skips: u64,
    /// WAL page count last known at the time of the most recent skip, if any
    /// skip has occurred yet in this process.
    #[serde(default)]
    pub wal_checkpoint_last_skip_wal_pages: Option<u64>,
    /// Age, in microseconds, of the oldest currently-open transaction
    /// registry entry (ADR-091 Plank 0). `None` when no transaction is
    /// currently open.
    pub oldest_pinned_tx_micros: Option<u64>,
    /// Diagnostic label of the oldest currently-open transaction registry
    /// entry, if any and if it was registered with one.
    pub oldest_pinned_tx_label: Option<String>,
    /// Number of currently open transaction registry entries.
    pub open_tx_count: usize,
    /// Current write-queue backlog depth (ADR-067 Component A): requests
    /// enqueued but not yet accepted by the `WriterTask` drain loop. `None`
    /// unless the write queue is enabled (`KHIVE_WRITE_QUEUE=1`) and a
    /// file-backed pool is available.
    pub write_queue_depth: Option<usize>,
    /// The write queue's configured bounded capacity
    /// (`PoolConfig::write_queue_capacity`), gated the same as
    /// `write_queue_depth`.
    pub write_queue_capacity: Option<usize>,
    /// Latest completed writer-task span: bounded-channel admission/backlog.
    #[serde(default)]
    pub write_last_queue_wait_micros: Option<u64>,
    /// Latest completed writer-task span: `BEGIN IMMEDIATE` acquisition.
    #[serde(default)]
    pub write_last_transaction_acquire_micros: Option<u64>,
    /// Latest completed writer-task span: application transaction body.
    #[serde(default)]
    pub write_last_body_micros: Option<u64>,
    /// Latest completed writer-task span: SQLite COMMIT/fsync phase.
    #[serde(default)]
    pub write_last_commit_micros: Option<u64>,
    /// Whole latest writer-task request span, retained for compatibility and
    /// comparison with the decomposed stages.
    #[serde(default)]
    pub write_last_total_micros: Option<u64>,
    /// Wall-clock timestamp of the writer-stage sample.
    #[serde(default)]
    pub write_last_observed_at_unix_ms: Option<u64>,
    /// Connection cap of the daemon socket this snapshot was served from.
    /// `None` from a daemon that predates the cap and when no listener owns
    /// the connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<ConnectionCapSnapshot>,
    /// Bounds and counts of the best-effort recall serve-ledger tasks.
    /// `None` from a daemon that predates the bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_ledger: Option<RecallLedgerSnapshot>,
}

// ── framing ───────────────────────────────────────────────────────────────────

/// Read one length-prefixed frame (4-byte BE u32 length + JSON bytes).
#[cfg(unix)]
pub async fn read_frame<R>(stream: &mut R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("daemon frame of {len} bytes exceeds {MAX_FRAME_BYTES} cap"),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(unix)]
fn initial_frame_timeout_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "daemon initial request frame read timed out",
    )
}

#[cfg(unix)]
pub(super) async fn read_initial_frame<R>(
    stream: &mut R,
    deadline: tokio::time::Instant,
) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    // Tokio polls the inner future before checking its timer. A frame already
    // buffered when a delayed connection task first runs would otherwise pass
    // even though its acceptance-time deadline has expired.
    if tokio::time::Instant::now() >= deadline {
        return Err(initial_frame_timeout_error());
    }
    let raw = tokio::time::timeout_at(deadline, read_frame(stream))
        .await
        .map_err(|_| initial_frame_timeout_error())??;
    if tokio::time::Instant::now() >= deadline {
        return Err(initial_frame_timeout_error());
    }
    Ok(raw)
}

/// Write one length-prefixed frame.
#[cfg(unix)]
pub async fn write_frame<W>(stream: &mut W, payload: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if payload.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "daemon frame of {} bytes exceeds {MAX_FRAME_BYTES} cap",
                payload.len()
            ),
        ));
    }
    let len = (payload.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}
