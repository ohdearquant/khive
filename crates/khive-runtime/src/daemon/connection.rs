#[cfg(unix)]
use super::*;

#[cfg(unix)]
pub(super) async fn write_response_frame<W>(stream: &mut W, payload: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    tokio::time::timeout(INITIAL_FRAME_READ_TIMEOUT, write_frame(stream, payload))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "daemon response write timed out",
            )
        })?
}

#[cfg(unix)]
pub(super) async fn wait_for_peer_disconnect(read: &mut tokio::net::unix::OwnedReadHalf) {
    let mut byte = [0u8; 1];
    // One request is admitted per connection. EOF, a read error, or any
    // subsequent byte all make this connection no longer a valid response
    // peer, so the first completed read is the entire observation.
    let _ = read.read(&mut byte).await;
}

#[cfg(all(unix, test))]
pub(super) async fn handle_conn<D: DaemonDispatch>(stream: UnixStream, dispatcher: D) {
    handle_conn_with_shutdown(
        stream,
        dispatcher,
        None,
        tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
    )
    .await;
}

#[cfg(all(unix, feature = "fault-injection"))]
#[doc(hidden)]
pub async fn handle_conn_for_test<D: DaemonDispatch>(stream: UnixStream, dispatcher: D) {
    handle_conn_with_shutdown(
        stream,
        dispatcher,
        None,
        tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
    )
    .await;
}

#[cfg(unix)]
pub(super) fn plan_frame_companion(raw: &[u8]) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_slice(raw).ok()?;
    if value.get("plan").and_then(serde_json::Value::as_bool) != Some(true)
        || value
            .get("protocol_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(PROTOCOL_VERSION))
    {
        return None;
    }
    [
        "presentation",
        "presentation_per_op",
        "format",
        "format_per_op",
        "request_id",
    ]
    .into_iter()
    .find(|field| value.get(*field).is_some())
}

#[cfg(all(
    unix,
    any(test, feature = "fault-injection", feature = "test-internals")
))]
pub(super) async fn handle_conn_with_shutdown<D: DaemonDispatch>(
    stream: UnixStream,
    dispatcher: D,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    initial_frame_deadline: tokio::time::Instant,
) {
    handle_conn_with_lifecycle(stream, dispatcher, shutdown, initial_frame_deadline, None).await;
}

#[cfg(unix)]
pub(super) async fn handle_conn_with_lifecycle<D: DaemonDispatch>(
    mut stream: UnixStream,
    dispatcher: D,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    initial_frame_deadline: tokio::time::Instant,
    lifecycle: Option<Arc<DaemonLifecycle>>,
) {
    let mut ordinary_admission = None;
    let production_shutdown = shutdown.is_some();
    // A handover is delivered over the probed connection. The production
    // listener enforces same-uid admission, and direct handler tests cannot
    // self-signal because they do not supply the daemon shutdown receiver.
    let handover_peer_allowed = peer_uid(&stream)
        .ok()
        .is_some_and(|uid| uid == unsafe { libc::geteuid() } as u32);
    let (local_shutdown_tx, local_shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown = shutdown.unwrap_or(local_shutdown_rx);
    // Keeps the fallback receiver open in direct/test calls. Production owns
    // a sender at the daemon-run scope and passes its receiver above.
    let _local_shutdown_tx = local_shutdown_tx;
    let raw = match read_initial_frame(&mut stream, initial_frame_deadline).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "failed to read daemon request frame");
            return;
        }
    };
    #[derive(Deserialize)]
    struct SupervisorRequestEnvelope {
        #[serde(flatten)]
        frame: DaemonRequestFrame,
        #[serde(default)]
        supervisor_handover: bool,
    }
    let decoded: Result<SupervisorRequestEnvelope, _> = serde_json::from_slice(&raw);
    if decoded.as_ref().ok().is_none_or(|item| item.frame.plan) {
        if let Some(field) = plan_frame_companion(&raw) {
            let response = DaemonResponseFrame {
                ok: false,
                result: None,
                error: Some(format!(
                    "invalid_params: plan=true cannot be combined with {field}"
                )),
                error_detail: Some(serde_json::json!({
                    "kind": "protocol",
                    "code": "invalid_params",
                    "message": format!("plan=true cannot be combined with {field}"),
                    "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
                })),
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id: Some(dispatcher.config_id().to_string()),
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: None,
            };
            if let Ok(payload) = serde_json::to_vec(&response) {
                if let Err(error) = write_response_frame(&mut stream, &payload).await {
                    tracing::debug!(%error, "failed to write plan envelope refusal");
                }
            }
            return;
        }
    }
    let (frame, handover_requested) = match decoded {
        Ok(item) => (item.frame, item.supervisor_handover),
        Err(e) => {
            tracing::debug!(error = %e, "failed to decode daemon request frame");
            return;
        }
    };
    let supervisor_probe = frame.probe_only;
    let handover_accepted = handover_requested
        && supervisor_probe
        && production_shutdown
        && handover_peer_allowed
        && frame.protocol_version == PROTOCOL_VERSION
        && !frame.plan
        && !frame.metrics_only
        && frame.ops.is_empty()
        && read_supervisor_marker_claim().is_some_and(|(pid, _)| pid != std::process::id());
    let (mut peer_read, mut peer_write) = stream.into_split();

    let served_config_id = Some(dispatcher.config_id().to_string());
    let resp = if frame.protocol_version != PROTOCOL_VERSION {
        let msg = format!(
            "daemon protocol mismatch: client={} daemon={} — \
             rebuild/update the client binary (make local)",
            frame.protocol_version, PROTOCOL_VERSION,
        );
        tracing::warn!(
            client_version = frame.protocol_version,
            daemon_version = PROTOCOL_VERSION,
            "daemon protocol version mismatch"
        );
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some(msg.clone()),
            error_detail: Some(serde_json::json!({
                "kind": "protocol",
                "code": "version_mismatch",
                "message": msg,
                "domain_disposition": crate::DomainDisposition::Unknown.as_str(),
            })),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            // A client below this protocol is a bridge that predates the binary this
            // daemon was spawned from. Through protocol 5 the bridge treats an
            // explicit `version_mismatch` from a higher-numbered daemon as a terminal
            // error it repeats on every request, and re-execs itself onto the on-disk
            // binary only for the implicit shape: an unequal `daemon_protocol_version`
            // with the flag clear. Answering older clients in that shape, still
            // refused and still carrying the code in `error_detail`, lets every
            // pre-swap bridge replace itself on its first request instead of staying
            // refused until a person reconnects the session. A client above this
            // protocol keeps the explicit flag. Remove once no live bridge predates
            // the two-direction re-exec in khive-mcp (an inode census of `kkernel mcp`
            // processes before the swap): bridges built with it no longer read the
            // flag, but a bump while older bridges still run must keep this shape so
            // they replace themselves too.
            version_mismatch: frame.protocol_version > PROTOCOL_VERSION,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: frame.request_id,
        }
    } else if handover_requested {
        if handover_accepted {
            DaemonResponseFrame {
                ok: true,
                result: None,
                error: None,
                error_detail: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id,
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: frame.request_id,
            }
        } else {
            DaemonResponseFrame {
                ok: false,
                result: None,
                error: Some("supervisor handover refused".to_string()),
                error_detail: Some(serde_json::json!({
                    "kind": "protocol",
                    "code": "supervisor_handover_refused",
                    "message": "supervisor handover refused",
                    "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
                })),
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id,
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: frame.request_id,
            }
        }
    } else if frame.metrics_only && !frame.plan {
        // Process-global gauge read: namespace/config-agnostic, so this is
        // handled BEFORE the `config_id` equality reject below (unlike every
        // other arm) — a metrics probe must work regardless of which
        // client's config is asking, since it never touches the dispatcher's
        // packs/db/embed registry. READ-ONLY: builds a snapshot and returns
        // immediately, never reaching the ops-dispatch arm.
        DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            error_detail: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: Some({
                let mut metrics = build_metrics_snapshot(&dispatcher);
                metrics.lifecycle = lifecycle.as_ref().map(|state| {
                    let mut snapshot = state.snapshot();
                    snapshot.idle_blockers = idle_retirement_blockers(&dispatcher);
                    snapshot
                });
                metrics.connections = lifecycle.as_ref().map(|state| state.connections.snapshot());
                metrics
            }),
            request_id: frame.request_id,
        }
    // There is no `frame.namespace != dispatcher.namespace()` reject here.
    // The daemon accepts and serves the request under the frame's own
    // identity (namespace / actor / visible_namespaces, built into a
    // `RequestIdentity` below) over its one shared warm registry, rather
    // than rejecting a differently-attributed same-uid connection to a cold
    // local-dispatch fallback. `config_id`: which governs packs/db/embed
    // coherence for the shared warm engine: remains a hard reject for every
    // field; only legacy IDs allow a superset of the client's extra embedders.
    } else if !config_ids_compatible(&frame.config_id, dispatcher.config_id()) {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            error_detail: Some(serde_json::json!({
                "kind": "protocol",
                "code": "config_mismatch",
                "message": "daemon configuration does not match the request",
                "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
            })),
            namespace_mismatch: false,
            config_mismatch: true,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: frame.request_id,
        }
    } else if frame.plan {
        DaemonResponseFrame {
            ok: true,
            result: Some(dispatcher.plan(&frame.ops)),
            error: None,
            error_detail: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        }
    } else if frame.probe_only {
        // Probe-only request: identity checks passed; return immediately without
        // dispatching any verb. The client uses this to confirm the daemon is
        // alive and identity-matching without triggering any mutation.
        DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            error_detail: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: frame.request_id,
        }
    } else {
        if let Some(lifecycle) = &lifecycle {
            ordinary_admission = lifecycle.admit();
            if ordinary_admission.is_none() {
                let refusal = DaemonResponseFrame {
                    ok: false,
                    result: None,
                    error: Some("daemon is draining; request was not admitted".to_owned()),
                    error_detail: Some(serde_json::json!({
                        "kind": "runtime", "code": "daemon_draining",
                        "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
                    })),
                    request_id: frame.request_id,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id: Some(dispatcher.config_id().to_owned()),
                    version_mismatch: false,
                    metrics: None,
                };
                if let Ok(payload) = serde_json::to_vec(&refusal) {
                    let _ = write_response_frame(&mut peer_write, &payload).await;
                }
                return;
            }
        }
        // A supervised events daemon that has not answered yet would fail
        // this request's audit commit after its domain write committed, so
        // the request waits for it here, before anything runs, and is
        // refused with nothing committed when it does not answer in time.
        if let Some(readiness) = dispatcher.events_readiness() {
            let not_ready = tokio::select! {
                outcome = readiness.wait() => outcome.err(),
                _ = wait_for_peer_disconnect(&mut peer_read) => return,
            };
            if let Some(not_ready) = not_ready {
                let message = not_ready.to_string();
                tracing::warn!(
                    socket = %not_ready.socket.display(),
                    reason = ?not_ready.reason,
                    request_id = frame.request_id,
                    "request refused: the supervised events daemon is not answering"
                );
                let refusal = DaemonResponseFrame {
                    ok: false,
                    result: None,
                    error: Some(message.clone()),
                    error_detail: Some(serde_json::json!({
                        "kind": "runtime",
                        "code": "events_daemon_unavailable",
                        "message": message,
                        "retryable": true,
                        "socket": not_ready.socket.display().to_string(),
                        "domain_disposition": crate::DomainDisposition::NotCommitted.as_str(),
                    })),
                    request_id: frame.request_id,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id: Some(dispatcher.config_id().to_owned()),
                    version_mismatch: false,
                    metrics: None,
                };
                if let Ok(payload) = serde_json::to_vec(&refusal) {
                    let _ = write_response_frame(&mut peer_write, &payload).await;
                }
                return;
            }
        }
        // Build the per-request identity context from the frame so the
        // implementor mints the storage/gate token from the CALLER's
        // identity, not the dispatcher's own construction-baked scalars.
        // This is always `Some` here: every frame that reaches this arm
        // carries a `namespace` (required on the wire) plus whatever
        // `actor_id`/`visible_namespaces` the client resolved (defaulting to
        // `None`/`vec![]` for an older, field-absent payload, which is
        // exactly the prior anonymous/no-extra-visibility behavior).
        // The caller's actor namespace joins default reads where the registry
        // mints the token (ADR-007 Rev 4 Rule 3b), the one seam every identity
        // path shares; the frame's list is forwarded as sent.
        let identity = RequestIdentity {
            namespace: frame.namespace.clone(),
            actor_id: frame.actor_id.clone(),
            visible_namespaces: frame.visible_namespaces.clone(),
            process_ref: frame.process_ref.clone(),
            request_id: frame.request_id,
        };
        tracing::debug!(
            request_id = frame.request_id,
            "daemon RequestIdentity constructed"
        );
        let (read_cancel_tx, read_cancel_rx) = tokio::sync::watch::channel(false);
        // The connection's own ceiling nests inside the dispatcher's, and a
        // nested scope keeps the earlier deadline, so the allowance must be
        // granted here or a long poll times out at the operator ceiling.
        let read_timeout = dispatcher.request_read_timeout(&frame.ops);
        let excluded_embedder_names =
            config_id_extra_embedder_exclusions(&frame.config_id, dispatcher.config_id())
                .expect("compatible configuration ids must expose their extra embedder sets");
        let dispatch = crate::runtime::scope_request_embedder_exclusions(
            excluded_embedder_names,
            khive_storage::scope_request_read_cancellation(
                shutdown,
                khive_storage::scope_request_read_cancellation(
                    read_cancel_rx,
                    khive_storage::scope_request_read_deadline(
                        read_timeout,
                        dispatcher.dispatch_with_error_detail(
                            frame.ops,
                            frame.presentation,
                            frame.presentation_per_op,
                            frame.format,
                            frame.format_per_op,
                            frame.from_wire,
                            Some(identity),
                        ),
                    ),
                ),
            ),
        );
        tokio::pin!(dispatch);
        let dispatch_result = tokio::select! {
            result = &mut dispatch => result,
            _ = wait_for_peer_disconnect(&mut peer_read) => {
                let _ = read_cancel_tx.send(true);
                dispatch.await
            }
        };
        match dispatch_result {
            Ok(result) => {
                let (result, detail) = take_daemon_lexical_timeout_marker(result);
                DaemonResponseFrame {
                    ok: true,
                    result: Some(result),
                    error: None,
                    error_detail: detail,
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id,
                    version_mismatch: false,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    metrics: None,
                    request_id: frame.request_id,
                }
            }
            Err(error) => {
                let error = DaemonDispatchError::new(error.message, Some(error.error_detail));
                DaemonResponseFrame {
                    ok: false,
                    result: None,
                    error: Some(error.message),
                    error_detail: Some(error.error_detail),
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id,
                    version_mismatch: false,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    metrics: None,
                    request_id: frame.request_id,
                }
            }
        }
    };

    let payload = if supervisor_probe {
        serde_json::to_value(&resp).and_then(|mut value| {
            if let Some(claim) = current_supervisor_claim() {
                value["supervisor_claim"] = serde_json::Value::String(claim);
            }
            if handover_accepted {
                value["supervisor_handover_accepted"] = serde_json::Value::Bool(true);
            }
            serde_json::to_vec(&value)
        })
    } else {
        serde_json::to_vec(&resp)
    };
    let mut handover_ack_written = false;
    match payload {
        Ok(payload) => {
            if payload.len() > MAX_FRAME_BYTES {
                // The serialized response exceeds the IPC frame cap.  Send a
                // small explicit error frame so the client can distinguish a
                // per-request payload-size failure from a daemon crash.  A
                // client that receives this error frame will NOT trigger
                // stale-daemon kill/respawn (ParseFailure requires a read_frame
                // error, not an ok=false result).
                tracing::warn!(
                    bytes = payload.len(),
                    limit = MAX_FRAME_BYTES,
                    "daemon response exceeds MAX_FRAME_BYTES; sending explicit error frame"
                );
                let message = format!(
                    "response too large: {} bytes exceeds {} byte IPC cap",
                    payload.len(),
                    MAX_FRAME_BYTES,
                );
                // One frame may aggregate successful, failed, and aborted operations.
                let err_resp = DaemonResponseFrame {
                    ok: false,
                    result: None,
                    error: Some(message.clone()),
                    error_detail: Some(serde_json::json!({
                        "kind": "transport",
                        "code": "response_frame_size_limit",
                        "message": message,
                        "domain_disposition": crate::DomainDisposition::Unknown.as_str(),
                    })),
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id: resp.served_config_id,
                    version_mismatch: false,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    metrics: None,
                    request_id: resp.request_id,
                };
                if let Ok(err_payload) = serde_json::to_vec(&err_resp) {
                    if let Err(e) = write_response_frame(&mut peer_write, &err_payload).await {
                        tracing::debug!(error = %e, "failed to write oversized-response error frame");
                    }
                }
            } else {
                match write_response_frame(&mut peer_write, &payload).await {
                    Ok(()) => handover_ack_written = true,
                    Err(e) => tracing::debug!(error = %e, "failed to write daemon response frame"),
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to serialize daemon response frame"),
    }
    if handover_accepted && handover_ack_written {
        // Signal this process, not a PID observed earlier over a socket.
        // Production installed its SIGTERM handler before binding the socket.
        if unsafe { libc::raise(libc::SIGTERM) } != 0 {
            tracing::error!(error = %std::io::Error::last_os_error(), "self-directed handover signal failed");
        }
    }
    drop(ordinary_admission);
}

/// An accepted connection owns a drain slot from the accept loop until its
/// handler future is dropped. The claim happens before spawning so shutdown
/// cannot observe zero in the gap between `accept()` and the task's first poll.
#[cfg(unix)]
pub(super) struct ActiveConnectionGuard {
    pub(super) active: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(unix)]
impl ActiveConnectionGuard {
    pub(super) fn claim(active: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self { active }
    }
}

#[cfg(unix)]
impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(unix)]
pub(super) fn spawn_connection_task<F>(
    active: Arc<std::sync::atomic::AtomicUsize>,
    future: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let guard = ActiveConnectionGuard::claim(active);
    tokio::spawn(async move {
        let _guard = guard;
        future.await;
    })
}
