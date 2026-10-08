//! Socket forwarding, read replay and supervisor handover.

use khive_runtime::daemon::{
    self, pid_path, read_frame, socket_path, write_frame, DaemonRequestFrame, DaemonResponseFrame,
    MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use rmcp::ErrorData as McpError;
use tokio::net::UnixStream;

use super::{
    fallback_or_reject, process_is_alive, supervised_daemon_error, FallbackReason,
    RECOVERER_LOCK_TIMEOUT_MS,
};
#[cfg(test)]
use super::{supervisor_discovery_hook, SupervisorDiscoveryPoint, FORCED_CONNECT_ERROR};

/// Result of a single forward attempt to the daemon socket.
#[derive(Debug)]
pub(super) enum ForwardOutcome {
    /// Successfully received and decoded a response frame.
    Response(Box<DaemonResponseFrame>),
    /// The socket is absent/refused, or the frame failed before it could reach
    /// dispatch. These outcomes are safe to route through recovery.
    NoSocket,
    /// Serialization produced a frame over the transport cap. `write_frame`
    /// would refuse it before writing any byte, so reconnecting or spawning
    /// cannot change the outcome.
    RequestTooLarge { bytes: usize },
    /// This process could not establish whether a daemon is listening. An OS
    /// access/policy failure is not proof that the daemon is absent, so it must
    /// never enter lifecycle recovery or local fallback (#1242).
    Unreachable {
        kind: std::io::ErrorKind,
        os_error_code: Option<i32>,
    },
    /// Invalid response framing/JSON or a response timeout after a full write.
    ParseFailure,
    /// EOF/reset after a full write; distinct from malformed frames/timeouts.
    ResponseLost,
    /// Connected and decoded a response, but the daemon's `daemon_protocol_version`
    /// does not match [`PROTOCOL_VERSION`], in either direction. Below: the
    /// new-client + old-daemon scenario, implicit (a pre-versioning daemon ignores
    /// the unknown request field and returns a decodable response whose protocol
    /// fields default to `false`/`0`) or explicit (`version_mismatch=true` with the
    /// daemon's lower number). Above: this bridge is the stale side, a rebuild
    /// swapped the on-disk binary and respawned the daemon under a newer protocol
    /// while this process kept running the old one. Since the real request was
    /// already written, the client treats both exactly like `ParseFailure`: a hard
    /// error without retrying, locally dispatching, killing, or respawning; the
    /// consumer arms the #714 self-heal beside it.
    ProtocolMismatch { daemon_protocol_version: u32 },
}

pub(super) fn classify_socket_connect_error(error: std::io::Error) -> ForwardOutcome {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ) {
        ForwardOutcome::NoSocket
    } else {
        ForwardOutcome::Unreachable {
            kind: error.kind(),
            os_error_code: error.raw_os_error(),
        }
    }
}

#[cfg(test)]
pub(super) async fn try_forward_inner(frame: &DaemonRequestFrame) -> ForwardOutcome {
    try_forward_before(frame, None).await
}

pub(super) fn socket_exchange_deadline(
    probe_only: bool,
    ops: &str,
    retry_deadline: Option<tokio::time::Instant>,
) -> tokio::time::Instant {
    // One absolute deadline bounds connect, write, and read together, so the
    // whole socket exchange can never exceed the ceiling the read phase alone
    // used to honour on its own. A same-UID peer that accepts the connection
    // and never reads it would otherwise block the write forever with
    // nothing above this function able to stop it.
    //
    // When this task inherited the caller's request-read deadline — the
    // spawned forward task in `server.rs` wraps this call in
    // `khive_storage::inherit_request_read_context` — that same absolute
    // instant is reused here, so an admitted forward cannot outlive the
    // deadline the rest of the request already obeys. Callers that never
    // scoped a request context (this function's own unit tests, or any
    // future caller outside the MCP bridge) fall back to a fresh relative
    // ceiling derived from the configured read timeout and the request's
    // valid long-poll waits plus a five-second transport margin.
    if probe_only {
        // Lifecycle probes are independent of the request that happened to
        // trigger recovery. Their caller supplies a fresh probe deadline; an
        // expired request deadline must never make a live daemon appear dead.
        retry_deadline.unwrap_or_else(|| {
            tokio::time::Instant::now() + khive_storage::request_read_timeout_from_env()
        })
    } else {
        let request_deadline = khive_storage::capture_request_read_context()
            .deadline()
            .map(khive_storage::RequestReadDeadline::async_at)
            .unwrap_or_else(|| {
                tokio::time::Instant::now()
                    + crate::request_policy::read_timeout(
                        ops,
                        khive_storage::request_read_timeout_from_env(),
                    )
            });
        retry_deadline.map_or(request_deadline, |retry| retry.min(request_deadline))
    }
}

pub(super) async fn try_forward_before(
    frame: &DaemonRequestFrame,
    retry_deadline: Option<tokio::time::Instant>,
) -> ForwardOutcome {
    let payload = match serde_json::to_vec(frame) {
        Ok(p) => p,
        Err(_) => return ForwardOutcome::NoSocket,
    };
    // Check before even connecting. The same cap is enforced by write_frame,
    // but that function returns InvalidData before its first write; treating
    // that as NoSocket would enter deterministic, futile lifecycle recovery.
    if payload.len() > MAX_FRAME_BYTES {
        return ForwardOutcome::RequestTooLarge {
            bytes: payload.len(),
        };
    }
    let sock = socket_path();
    #[cfg(test)]
    {
        let forced_error = FORCED_CONNECT_ERROR.load(std::sync::atomic::Ordering::SeqCst);
        if forced_error != 0 {
            return classify_socket_connect_error(std::io::Error::from_raw_os_error(forced_error));
        }
    }

    let deadline = socket_exchange_deadline(frame.probe_only, &frame.ops, retry_deadline);

    let mut stream = match tokio::time::timeout_at(deadline, UnixStream::connect(&sock)).await {
        Ok(Ok(s)) => s,
        Ok(Err(error)) => return classify_socket_connect_error(error),
        Err(_elapsed) => {
            // Nothing was ever written to a peer, so this is exactly the
            // ordinary not-listening case: safe to let recovery proceed.
            tracing::warn!(
                target: "khive_mcp::daemon",
                "daemon connect timed out before the socket-exchange deadline — \
                 treating as no socket"
            );
            return ForwardOutcome::NoSocket;
        }
    };
    match tokio::time::timeout_at(deadline, write_frame(&mut stream, &payload)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return ForwardOutcome::NoSocket,
        Err(_elapsed) => {
            // The write itself never completed. Dropping `stream` here (it
            // goes out of scope on this return) closes the socket, so a peer
            // that later resumes reading observes end-of-stream in the
            // middle of the frame — at most the 4-byte length prefix and a
            // short partial body — and can never finish `read_frame`'s
            // `read_exact` for the full body. It cannot misdispatch a
            // truncated request.
            //
            // Because nothing was fully delivered, this is NOT the
            // post-write ambiguity the read-timeout arm below returns for
            // (where the write had already completed and the request may
            // already be executing on the daemon side). Treat it like
            // `NoSocket` — the pre-write, "nothing sent" case — so
            // `forward_or_spawn_with_exe`'s recovery path can kill and
            // respawn a stuck peer and retry, instead of surfacing a
            // permanent no-retry ambiguity error for what may just be a
            // wedged process squatting on the socket path.
            tracing::warn!(
                target: "khive_mcp::daemon",
                "daemon write timed out before the socket-exchange deadline — \
                 dropping the connection and treating as no socket"
            );
            return ForwardOutcome::NoSocket;
        }
    }
    // The request is now fully written and may already be executing (or
    // committed) on the daemon side, so a stalled or unresponsive daemon
    // must not leave this read pending forever — that would keep the
    // calling task (and the MCP handler awaiting it) alive indefinitely.
    // Bounded by the same deadline as the connect and write phases above.
    let resp = match tokio::time::timeout_at(deadline, read_frame(&mut stream)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            // The request was sent but the daemon closed the connection before
            // sending a response frame. This does not establish why it closed
            // or whether dispatch completed. Only classified reads can replay;
            // mutations remain ambiguous, with no recovery or local fallback.
            tracing::warn!(
                target: "khive_mcp::daemon",
                error = %e,
                "daemon response unavailable after full request write"
            );
            return if matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ) {
                ForwardOutcome::ResponseLost
            } else {
                ForwardOutcome::ParseFailure
            };
        }
        Err(_elapsed) => {
            // The daemon never answered within the deadline. The write
            // already completed, so a timeout remains terminal. It does not
            // establish whether the daemon is still executing this request.
            tracing::warn!(
                target: "khive_mcp::daemon",
                "daemon response read timed out after the request was fully \
                 written — returning terminal ambiguity"
            );
            return ForwardOutcome::ParseFailure;
        }
    };
    match serde_json::from_slice::<DaemonResponseFrame>(&resp) {
        Ok(frame) => {
            // A pre-versioning daemon (old-daemon scenario) returns a decodable
            // response whose `daemon_protocol_version` defaults to 0. The field
            // `version_mismatch` is also false because the old daemon never set
            // it — making `map_response` accept the stale response when
            // `served_config_id` happens to match. Detect this here so the
            // caller can route it through the same terminal no-retry path.
            //
            // Also catch the explicit-mismatch / auto-upgrade case (#156): when a
            // warm OLD daemon receives a request from a NEWER client it responds
            // with `version_mismatch=true` and its own (lower) version number.
            // `daemon_protocol_version < PROTOCOL_VERSION` means the daemon is
            // stale — route through the same terminal error as the implicit case
            // above. `daemon_protocol_version > PROTOCOL_VERSION` means this bridge
            // binary is behind: a rebuild swapped the on-disk binary and respawned
            // the daemon under a newer protocol while this process kept running the
            // old one. That is the scenario the #714 self-heal exists for, so it
            // takes the same terminal path and the consumer arms the re-exec, which
            // picks up the on-disk binary the daemon itself was spawned from.
            // Leaving that direction to `map_response` returned the hard error on
            // every request for the rest of the process's life and never re-exec'd.
            if frame.daemon_protocol_version != PROTOCOL_VERSION {
                tracing::warn!(
                target: "khive_mcp::daemon",
                    daemon_version = frame.daemon_protocol_version,
                    expected = PROTOCOL_VERSION,
                    explicit_mismatch = frame.version_mismatch,
                    "daemon protocol version mismatch after request write — rejecting without retry",
                );
                return ForwardOutcome::ProtocolMismatch {
                    daemon_protocol_version: frame.daemon_protocol_version,
                };
            }
            ForwardOutcome::Response(Box::new(frame))
        }
        Err(e) => {
            tracing::warn!(
                target: "khive_mcp::daemon",
                error = %e,
                bytes = resp.len(),
                "daemon response could not be decoded after request write on {}",
                sock.display()
            );
            ForwardOutcome::ParseFailure
        }
    }
}

const HANDOVER_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);
const HANDOVER_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

pub(super) fn bounded_retry_deadline() -> tokio::time::Instant {
    let deadline = tokio::time::Instant::now() + HANDOVER_RETRY_WINDOW;
    khive_storage::capture_request_read_context()
        .deadline()
        .map(khive_storage::RequestReadDeadline::async_at)
        .map_or(deadline, |caller| caller.min(deadline))
}

pub(super) fn pid_file_directory_is_trusted_if_present(
    pid_file: &std::path::Path,
) -> Result<bool, String> {
    let parent = pid_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    match std::fs::metadata(parent) {
        // First startup may not have created the default rendezvous directory
        // yet. No PID record exists to read; daemon startup checks the parent
        // before it creates the PID file.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        _ => daemon::ensure_pid_file_dir_is_trusted(pid_file)
            .map(|()| true)
            .map_err(|error| format!("{error:#}")),
    }
}

pub(super) fn recorded_daemon_is_alive() -> bool {
    let pid_file = pid_path();
    match pid_file_directory_is_trusted_if_present(&pid_file) {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => {
            tracing::warn!(
                target: "khive_mcp::daemon",
                error = %error,
                "daemon PID-file directory is not trusted; skipping the recorded-process probe"
            );
            return false;
        }
    }
    std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .is_some_and(process_is_alive)
}

/// A process supervisor's claim on the daemon rendezvous, read from
/// [`daemon::supervisor_marker_path`]. Presence gives the supervisor a bounded
/// opportunity to bind the socket before ordinary client bootstrap resumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SupervisorMarker {
    pub(super) job: String,
    pub(super) pid: u32,
    pub(super) restart_interval: std::time::Duration,
    pub(super) modified: Option<std::time::SystemTime>,
}

pub(super) const DEFAULT_SUPERVISOR_RESTART_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(10);
const SUPERVISOR_RESTART_ATTEMPTS: u32 = 3;

impl SupervisorMarker {
    pub(super) fn pid_is_alive(&self) -> bool {
        process_is_alive(self.pid)
    }

    pub(super) fn wait_deadline(
        &self,
        request_started: tokio::time::Instant,
    ) -> tokio::time::Instant {
        self.restart_interval
            .checked_mul(SUPERVISOR_RESTART_ATTEMPTS)
            .and_then(|bound| request_started.checked_add(bound))
            .unwrap_or_else(|| {
                request_started + DEFAULT_SUPERVISOR_RESTART_INTERVAL * SUPERVISOR_RESTART_ATTEMPTS
            })
    }

    fn age(&self) -> Option<std::time::Duration> {
        self.modified
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
    }
}

/// Read the supervision marker, if any. A marker file that exists but cannot
/// be parsed is not the same as no marker: it still names a claim (job
/// `<unreadable>`, pid 0 — never alive per [`process_is_alive`]'s `pid <= 0`
/// guard), so it still suppresses this client's spawn for the default finite
/// interval. Only a genuinely absent file means "no marker".
pub(super) fn read_supervisor_marker() -> Option<SupervisorMarker> {
    let path = daemon::supervisor_marker_path();
    let modified = std::fs::symlink_metadata(&path)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    let read_regular_marker = || -> std::io::Result<String> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;

        // Inspect the opened file, not only a pathname that can be replaced.
        // Nonblocking open prevents a FIFO from stalling before that check;
        // no-follow treats a symlink as an unreadable declaration.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "supervisor marker is not a regular file",
            ));
        }
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(contents)
    };
    let contents = match read_regular_marker() {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            return Some(SupervisorMarker {
                job: "<unreadable>".to_owned(),
                pid: 0,
                restart_interval: DEFAULT_SUPERVISOR_RESTART_INTERVAL,
                modified,
            })
        }
    };
    let mut lines = contents.lines();
    let job = lines.next().unwrap_or("").trim();
    let pid = lines
        .next()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .unwrap_or(0);
    // Legacy two-line markers and malformed intervals retain the finite
    // default suppression, rather than accidentally authorizing an early spawn.
    let restart_interval = lines
        .next()
        .and_then(|seconds| seconds.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_SUPERVISOR_RESTART_INTERVAL);
    Some(SupervisorMarker {
        job: if job.is_empty() {
            "<unnamed>".to_owned()
        } else {
            job.to_owned()
        },
        pid,
        restart_interval,
        modified,
    })
}

pub(super) fn supervisor_marker_lock_path(marker: &std::path::Path) -> std::path::PathBuf {
    let mut lock_path = marker.as_os_str().to_os_string();
    lock_path.push(".lock");
    std::path::PathBuf::from(lock_path)
}

/// Serialize the client's last ownership decision with launcher publication.
/// The launcher locks this same permanent `<marker>.lock` inode through publish
/// and exec. A bounded nonblocking retry keeps a stalled launcher from pinning
/// a request worker; failure must never authorize a competing spawn.
pub(super) async fn acquire_supervisor_marker_lock() -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    let marker = daemon::supervisor_marker_path();
    if let Some(parent) = marker.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(supervisor_marker_lock_path(&marker))?;
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(RECOVERER_LOCK_TIMEOUT_MS);
    let deadline = khive_storage::capture_request_read_context()
        .deadline()
        .map(khive_storage::RequestReadDeadline::async_at)
        .map_or(deadline, |caller| caller.min(deadline));
    loop {
        if khive_storage::request_read_is_cancelled() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                tokio::time::sleep_until((now + HANDOVER_RETRY_INTERVAL).min(deadline)).await;
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

pub(super) struct SupervisorWaitResult {
    pub(super) outcome: ForwardOutcome,
    /// Only true when the first marker's grace actually expired while a marker
    /// remained. Marker disappearance alone cannot authorize degraded bootstrap.
    pub(super) degraded_bootstrap: bool,
}

/// Wait under the first observed restart interval, anchored to request entry.
/// Rewrites (including a fresh PID or interval) update diagnostics, not the
/// bound. A socket response, marker removal, or caller termination wins early.
pub(super) async fn wait_for_supervisor(
    frame: &DaemonRequestFrame,
    replay: &mut ReadReplayBudget,
    request_started: tokio::time::Instant,
    initial_marker: SupervisorMarker,
) -> Result<SupervisorWaitResult, McpError> {
    #[cfg(test)]
    supervisor_discovery_hook(SupervisorDiscoveryPoint::SupervisorWait);
    let supervisor_deadline = initial_marker.wait_deadline(request_started);
    let caller_deadline = khive_storage::capture_request_read_context()
        .deadline()
        .map(khive_storage::RequestReadDeadline::async_at);
    let retry_deadline =
        caller_deadline.map_or(supervisor_deadline, |d| d.min(supervisor_deadline));
    loop {
        let Some(marker) = read_supervisor_marker() else {
            return Ok(SupervisorWaitResult {
                outcome: ForwardOutcome::NoSocket,
                degraded_bootstrap: false,
            });
        };
        let now = tokio::time::Instant::now();
        if khive_storage::request_read_is_cancelled()
            || caller_deadline.is_some_and(|deadline| now >= deadline)
        {
            return Err(supervised_daemon_error(&marker));
        }
        if now >= supervisor_deadline {
            tracing::warn!(
                target: "khive_mcp::daemon",
                reason = "supervised_daemon_bootstrap_degraded",
                job = %marker.job,
                pid = marker.pid,
                pid_alive = marker.pid_is_alive(),
                marker_age_secs = ?marker.age().map(|age| age.as_secs_f64()),
                time_waited_secs = request_started.elapsed().as_secs_f64(),
                "supervisor present, daemon absent; proceeding with guarded client bootstrap"
            );
            return Ok(SupervisorWaitResult {
                outcome: ForwardOutcome::NoSocket,
                degraded_bootstrap: true,
            });
        }
        sleep_until_retry(retry_deadline).await;
        if tokio::time::Instant::now() >= retry_deadline
            || khive_storage::request_read_is_cancelled()
        {
            continue;
        }
        let outcome = try_forward_with_read_replay(frame, replay, Some(retry_deadline)).await;
        if !matches!(outcome, ForwardOutcome::NoSocket) {
            return Ok(SupervisorWaitResult {
                outcome,
                degraded_bootstrap: false,
            });
        }
    }
}

pub(super) async fn sleep_until_retry(deadline: tokio::time::Instant) {
    tokio::time::sleep_until((tokio::time::Instant::now() + HANDOVER_RETRY_INTERVAL).min(deadline))
        .await;
}

pub(super) struct ReadReplayBudget {
    pub(super) remaining: usize,
    deadline: Option<tokio::time::Instant>,
}

impl ReadReplayBudget {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            remaining: if enabled { 1 } else { 0 },
            deadline: None,
        }
    }
}

pub(super) async fn try_forward_with_read_replay(
    frame: &DaemonRequestFrame,
    replay: &mut ReadReplayBudget,
    attempt_deadline: Option<tokio::time::Instant>,
) -> ForwardOutcome {
    let outcome = try_forward_before(frame, attempt_deadline).await;
    if !matches!(outcome, ForwardOutcome::ResponseLost) || replay.remaining == 0 {
        return outcome;
    }
    let deadline = *replay.deadline.get_or_insert_with(|| {
        let allowance = crate::request_policy::read_timeout(
            &frame.ops,
            khive_storage::request_read_timeout_from_env(),
        );
        let deadline = tokio::time::Instant::now() + allowance + HANDOVER_RETRY_INTERVAL;
        let deadline = khive_storage::capture_request_read_context()
            .deadline()
            .map(khive_storage::RequestReadDeadline::async_at)
            .map_or(deadline, |caller| caller.min(deadline));
        attempt_deadline.map_or(deadline, |attempt| attempt.min(deadline))
    });
    while replay.remaining > 0
        && tokio::time::Instant::now() < deadline
        && !khive_storage::request_read_is_cancelled()
    {
        sleep_until_retry(deadline).await;
        if tokio::time::Instant::now() >= deadline || khive_storage::request_read_is_cancelled() {
            break;
        }
        replay.remaining -= 1;
        match try_forward_before(frame, Some(deadline)).await {
            ForwardOutcome::NoSocket | ForwardOutcome::ResponseLost => {}
            ForwardOutcome::Response(response)
                if response.config_mismatch
                    || response.namespace_mismatch
                    || !response.served_config_id.as_deref().is_some_and(|served| {
                        khive_runtime::daemon::config_ids_compatible(&frame.config_id, served)
                    }) =>
            {
                // A later identity rejection cannot erase the first dispatch
                // or permit map_response to select local fallback.
                return ForwardOutcome::ResponseLost;
            }
            other => return other,
        }
    }
    // A read may have executed before losing its response. Even if its retry
    // only saw missing sockets, never convert this to recovery/local fallback.
    ForwardOutcome::ResponseLost
}

pub(super) fn daemon_mcp_error(
    message: impl Into<String>,
    data: Option<serde_json::Value>,
) -> McpError {
    let error = daemon::DaemonDispatchError::new(message, data);
    McpError::internal_error(error.message, Some(error.error_detail))
}

pub(super) fn request_too_large_error(bytes: usize) -> McpError {
    let message =
        format!("request too large: {bytes} bytes exceeds {MAX_FRAME_BYTES} byte daemon IPC cap");
    let error = daemon::DaemonDispatchError::new(
        message,
        Some(serde_json::json!({
            "kind": "transport",
            "code": "request_frame_size_limit",
            "frame_bytes": bytes,
            "max_frame_bytes": MAX_FRAME_BYTES,
            "domain_disposition": khive_runtime::DomainDisposition::NotCommitted.as_str(),
        })),
    );
    McpError::invalid_params(error.message, Some(error.error_detail))
}

/// The operator-facing text for a protocol mismatch, by direction. A daemon ahead
/// of this bridge is the rebuilt-binary case: the bridge re-execs the on-disk binary
/// once this response has flushed (#714), so the caller's next request reaches a
/// bridge that matches.
pub(super) fn protocol_mismatch_message(daemon_protocol_version: u32) -> String {
    if daemon_protocol_version > PROTOCOL_VERSION {
        format!(
            "daemon protocol mismatch: this bridge speaks version {PROTOCOL_VERSION}, the \
             daemon speaks {daemon_protocol_version}; the bridge re-execs the current binary \
             after this response, retry the request"
        )
    } else {
        format!(
            "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
             run `make local` to rebuild the daemon binary"
        )
    }
}

pub(super) fn protocol_mismatch_error(
    message: String,
    data: Option<serde_json::Value>,
) -> McpError {
    let mut error = daemon::DaemonDispatchError::new(message, data);
    error.error_detail["domain_disposition"] =
        serde_json::json!(khive_runtime::DomainDisposition::Unknown.as_str());
    error.error_detail["code"] = serde_json::json!("version_mismatch");
    error.error_detail["kind"] = serde_json::json!("protocol");
    if let Some(fields) = error.error_detail.as_object_mut() {
        fields.remove("domain_result");
    }
    McpError::internal_error(error.message, Some(error.error_detail))
}

pub(super) fn map_response(
    resp: DaemonResponseFrame,
    expected_config_id: &str,
    namespace_client: &str,
) -> Option<Result<String, McpError>> {
    // Protocol version mismatch is a hard error — do NOT fall back to local
    // dispatch, which would hide the skew. Surface the daemon's own message.
    if resp.version_mismatch {
        let msg = resp.error.unwrap_or_else(|| {
            format!(
                "daemon protocol mismatch: client={} daemon={} — \
                 rebuild/update the client binary (make local)",
                PROTOCOL_VERSION, resp.daemon_protocol_version,
            )
        });
        return Some(Err(protocol_mismatch_error(msg, resp.error_detail)));
    }

    if resp.namespace_mismatch {
        return fallback_or_reject(
            FallbackReason::NamespaceMismatch,
            expected_config_id,
            resp.served_config_id.as_deref(),
            namespace_client,
        );
    }
    if resp.config_mismatch {
        return fallback_or_reject(
            FallbackReason::ConfigMismatch,
            expected_config_id,
            resp.served_config_id.as_deref(),
            namespace_client,
        );
    }
    // Fail closed: only trust a result the daemon positively confirms it served
    // under a compatible config. A legacy daemon omits `served_config_id` (→ None)
    // and a daemon with any incompatible field echoes a different id — both fall back local.
    if !resp.served_config_id.as_deref().is_some_and(|served| {
        khive_runtime::daemon::config_ids_compatible(expected_config_id, served)
    }) {
        return fallback_or_reject(
            FallbackReason::ConfigMismatch,
            expected_config_id,
            resp.served_config_id.as_deref(),
            namespace_client,
        );
    }
    if resp.ok {
        if resp
            .error_detail
            .as_ref()
            .and_then(|detail| detail.get("lexical_timeout"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            tracing::warn!(target: "khive_mcp::daemon", source = "daemon_response", "lexical read timed out");
        }
        Some(Ok(resp.result.unwrap_or_default()))
    } else {
        let msg = resp.error.unwrap_or_else(|| {
            format!(
                "daemon returned an error without a message \
                 (code: internal_error; daemon config: {})",
                resp.served_config_id.as_deref().unwrap_or("unknown"),
            )
        });
        Some(Err(daemon_mcp_error(msg, resp.error_detail)))
    }
}
