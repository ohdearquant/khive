use super::events_count_read_budget::count_storage_error_response;
use super::paths::{
    CONN_IO_TIMEOUT, EVENTS_PROTOCOL_VERSION, MAX_CACHED_NAMESPACE_STORES,
    MAX_QUERY_EVENTS_PAGE_ROWS,
};
use super::protocol::{EventsRequest, EventsResponse, WireWriterTaskFailure};
use crate::daemon::write_frame;
use khive_db::StorageBackend;
use khive_storage::{EventStore, StorageError};
use std::sync::Arc;
use tokio::net::UnixStream;

/// Read one length-prefixed request frame, admitting the body buffer against
/// the shared byte budget before allocating it. The returned permit holds
/// the admitted bytes for as long as the buffer may be alive — the caller
/// drops it after the response is written. The per-frame cap is checked
/// before admission, so a single frame is always satisfiable against the
/// full budget (`MAX_INFLIGHT_REQUEST_BYTES >= MAX_FRAME_BYTES`) and a
/// waiter can never deadlock on an impossible request.
#[cfg(unix)]
pub(super) async fn read_frame_budgeted(
    stream: &mut UnixStream,
    budget: &Arc<tokio::sync::Semaphore>,
) -> std::io::Result<(Vec<u8>, tokio::sync::OwnedSemaphorePermit)> {
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > crate::daemon::MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "daemon frame of {len} bytes exceeds {} cap",
                crate::daemon::MAX_FRAME_BYTES
            ),
        ));
    }
    let permit = Arc::clone(budget)
        .acquire_many_owned(len as u32)
        .await
        .map_err(|_| std::io::Error::other("frame budget closed"))?;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok((buf, permit))
}

#[cfg(unix)]
pub(super) type NamespaceStores =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<dyn EventStore>>>>;

/// The cached per-namespace store, constructing (and caching) it on first
/// use. Construction failures are not cached — the next request retries.
///
/// The cache key is the trimmed namespace, matching the backend's own
/// normalization, so spellings that resolve to one store share one handle.
/// The map is bounded: at `MAX_CACHED_NAMESPACE_STORES` entries an arbitrary
/// existing entry is evicted to admit the new namespace, so the map never
/// grows past the cap and no namespace is permanently condemned to
/// per-request store rebuilds. Keys reaching this cache have already passed
/// `Namespace` validation at dispatch (charset + length bound), so cap ×
/// bounded key is the worst-case retained memory.
#[cfg(unix)]
pub(super) fn namespace_store(
    backend: &StorageBackend,
    stores: &NamespaceStores,
    namespace: &str,
) -> Result<Arc<dyn EventStore>, khive_db::SqliteError> {
    namespace_store_with_cap(backend, stores, namespace, MAX_CACHED_NAMESPACE_STORES)
}

#[cfg(unix)]
pub(super) fn namespace_store_with_cap(
    backend: &StorageBackend,
    stores: &NamespaceStores,
    namespace: &str,
    cap: usize,
) -> Result<Arc<dyn EventStore>, khive_db::SqliteError> {
    let key = namespace.trim();
    if let Some(store) = stores
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
    {
        return Ok(Arc::clone(store));
    }
    let store = backend.events_for_namespace(namespace)?;
    let mut map = stores
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= cap && !map.contains_key(key) {
        // At the cap, evict an arbitrary entry instead of refusing
        // admission: stores are cheap pool handles, so eviction costs one
        // rebuild on the victim's next request, while refusing admission
        // would make every request beyond the cap rebuild its store and
        // rerun schema init on the SQLite writer forever. Keys are
        // validated `Namespace` values (bounded length), so the map's
        // worst-case memory is cap × small key + cap handles.
        if let Some(victim) = map.keys().next().cloned() {
            map.remove(&victim);
        }
    }
    map.insert(key.to_string(), Arc::clone(&store));
    Ok(store)
}

#[cfg(unix)]
pub(super) async fn serve_events_conn(
    mut stream: UnixStream,
    backend: Arc<StorageBackend>,
    stores: NamespaceStores,
    frame_budget: Arc<tokio::sync::Semaphore>,
) {
    loop {
        // Deadline on the whole frame read — budget admission included: a
        // peer that opens a connection and stalls, or that cannot be
        // admitted because other connections hold the byte budget, is
        // closed instead of holding this task and its descriptor
        // indefinitely.
        let (payload, budget_permit) = match tokio::time::timeout(
            CONN_IO_TIMEOUT,
            read_frame_budgeted(&mut stream, &frame_budget),
        )
        .await
        {
            Ok(Ok(pair)) => pair,
            // Includes clean EOF on peer disconnect, and the expired deadline.
            Ok(Err(_)) | Err(_) => return,
        };
        let response = match serde_json::from_slice::<EventsRequest>(&payload) {
            Ok(request) if request.count_budget_secs().is_some() => {
                let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                let dispatch = khive_storage::scope_request_read_cancellation(
                    cancel_rx,
                    dispatch_events_request(request, &backend, &stores),
                );
                tokio::pin!(dispatch);
                tokio::select! {
                    response = &mut dispatch => response,
                    _ = count_peer_disconnected(&stream) => {
                        let _ = cancel_tx.send(true);
                        // The SQLite owner must interrupt and join before this frame's permit drops.
                        let _ = dispatch.await;
                        return;
                    }
                }
            }
            Ok(request) => dispatch_events_request(request, &backend, &stores).await,
            Err(error) => EventsResponse::Error {
                message: format!("events daemon could not parse request frame: {error}"),
                retryable: false,
                writer_task_failure: None,
                sqlite_write_failure: None,
                read_failure: None,
            },
        };
        let bytes = match serde_json::to_vec(&response) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(error = %error, "events daemon response serialization failed");
                return;
            }
        };
        let bytes = if bytes.len() > crate::daemon::MAX_FRAME_BYTES {
            // A page can obey the row cap yet exceed the framing cap. Reply
            // with a small, non-retryable refusal instead of closing the
            // socket and making a healthy daemon look unreachable.
            let refusal = EventsResponse::Error {
                message: format!(
                    "response_frame_size_limit: {} response bytes exceed the {}-byte events IPC frame cap; request a narrower page",
                    bytes.len(),
                    crate::daemon::MAX_FRAME_BYTES
                ),
                retryable: false,
                writer_task_failure: None,
                sqlite_write_failure: None,
                read_failure: None,
            };
            match serde_json::to_vec(&refusal) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::error!(error = %error, "events daemon frame-size refusal serialization failed");
                    return;
                }
            }
        } else {
            bytes
        };
        // Same deadline on the response write: a peer that stops reading
        // would otherwise park this task in a full socket buffer.
        match tokio::time::timeout(CONN_IO_TIMEOUT, write_frame(&mut stream, &bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => return,
        }
        // The request buffer is long dropped; release its byte budget only
        // now, after the whole request lifecycle, so admission tracks live
        // work rather than just live buffers.
        drop(budget_permit);
    }
}

#[cfg(unix)]
pub(super) async fn count_peer_disconnected(stream: &UnixStream) {
    loop {
        if stream.readable().await.is_err() {
            return;
        }
        let mut byte = [0u8; 1];
        match stream.try_read(&mut byte) {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            // Count clients own a one-request socket; extra bytes also end that exchange.
            _ => return,
        }
    }
}

#[cfg(unix)]
pub(super) async fn dispatch_events_request(
    request: EventsRequest,
    backend: &StorageBackend,
    stores: &NamespaceStores,
) -> EventsResponse {
    if request.protocol_version() != EVENTS_PROTOCOL_VERSION {
        return EventsResponse::Error {
            message: format!(
                "events protocol version mismatch: daemon speaks {}, client sent {}",
                EVENTS_PROTOCOL_VERSION,
                request.protocol_version()
            ),
            retryable: false,
            writer_task_failure: None,
            sqlite_write_failure: None,
            read_failure: None,
        };
    }
    // The wire namespace is an unrestricted client-supplied String until this
    // point. Validate it as a real `Namespace` (charset plus the 256-byte
    // length bound) before it can become a cache key, a store, or a database
    // row — without this, near-frame-sized strings are attacker-controlled
    // retained memory in a process that lives for months.
    if let Err(error) = khive_types::Namespace::parse(request.namespace().trim()) {
        return EventsResponse::Error {
            message: format!("events request namespace rejected: {error}"),
            retryable: false,
            writer_task_failure: None,
            sqlite_write_failure: None,
            read_failure: None,
        };
    }
    let count_budget_secs = match request.count_budget_secs() {
        Some(Some(seconds))
            if (1..=khive_storage::request_context::MAX_REQUEST_READ_TIMEOUT_SECS)
                .contains(&seconds) =>
        {
            Some(seconds)
        }
        Some(_) => {
            return storage_error_response(&StorageError::InvalidInput {
                capability: khive_storage::StorageCapability::Events,
                operation: "events count".into(),
                message: "missing or invalid operator_read_budget_secs attribution".into(),
            })
        }
        None => None,
    };
    if let Some(seconds) = count_budget_secs {
        if let Err(error) = khive_storage::ensure_request_read_active("events count") {
            return count_storage_error_response(error, seconds);
        }
    }
    let store = match namespace_store(backend, stores, request.namespace()) {
        Ok(store) => store,
        Err(error) => {
            return EventsResponse::Error {
                message: format!("events store unavailable: {error}"),
                retryable: true,
                writer_task_failure: None,
                sqlite_write_failure: None,
                read_failure: None,
            };
        }
    };
    if let Some(seconds) = count_budget_secs {
        if let Err(error) = khive_storage::ensure_request_read_active("events count") {
            return count_storage_error_response(error, seconds);
        }
    }
    match request {
        EventsRequest::AppendEvents { events, .. } => match store.append_events(events).await {
            Ok(summary) => EventsResponse::Appended { summary },
            Err(error) => storage_error_response(&error),
        },
        EventsRequest::AppendEventsIdempotent { events, .. } => {
            match store.append_events_idempotent(events).await {
                Ok(result) => EventsResponse::Idempotent { result },
                Err(error) => storage_error_response(&error),
            }
        }
        EventsRequest::GetEvent { id, .. } => match store.get_event(id).await {
            Ok(event) => EventsResponse::Event { event },
            Err(error) => storage_error_response(&error),
        },
        EventsRequest::QueryEvents { filter, page, .. } => {
            if page.limit > MAX_QUERY_EVENTS_PAGE_ROWS {
                return EventsResponse::Error {
                    message: format!(
                        "events query page limit {} exceeds the daemon cap of {} rows; \
                         request narrower pages",
                        page.limit, MAX_QUERY_EVENTS_PAGE_ROWS
                    ),
                    retryable: false,
                    writer_task_failure: None,
                    sqlite_write_failure: None,
                    read_failure: None,
                };
            }
            match store.query_events(filter, page).await {
                Ok(page) => EventsResponse::Pageful { page },
                Err(error) => storage_error_response(&error),
            }
        }
        EventsRequest::QueryEventPage { query, .. } => {
            if let Err(error) = crate::event_page::validate_page_query(&query) {
                return storage_error_response(&error);
            }
            match store.query_event_page(query).await {
                Ok(window) => EventsResponse::EventPageWindow { window },
                Err(error) => storage_error_response(&error),
            }
        }
        EventsRequest::CountEvents { filter, .. } => {
            let Some(seconds) = count_budget_secs else {
                return storage_error_response(&StorageError::InvalidInput {
                    capability: khive_storage::StorageCapability::Events,
                    operation: "events count".into(),
                    message: "missing operator_read_budget_secs attribution".into(),
                });
            };
            match store.count_events(filter).await {
                Ok(count) => match khive_storage::ensure_request_read_active("events count") {
                    Ok(()) => EventsResponse::Count { count },
                    Err(error) => count_storage_error_response(error, seconds),
                },
                Err(error) => count_storage_error_response(error, seconds),
            }
        }
        EventsRequest::CountEventsGrouped {
            filter, group_by, ..
        } => {
            let Some(seconds) = count_budget_secs else {
                return storage_error_response(&StorageError::InvalidInput {
                    capability: khive_storage::StorageCapability::Events,
                    operation: "events count".into(),
                    message: "missing operator_read_budget_secs attribution".into(),
                });
            };
            match store.count_events_grouped(filter, group_by).await {
                Ok(counts) => match khive_storage::ensure_request_read_active("events count") {
                    Ok(()) => EventsResponse::GroupedCount { counts },
                    Err(error) => count_storage_error_response(error, seconds),
                },
                Err(error) => count_storage_error_response(error, seconds),
            }
        }
    }
}

#[cfg(unix)]
pub(super) fn storage_error_response(error: &StorageError) -> EventsResponse {
    let (message, writer_task_failure) = match error.without_sqlite_write_stage() {
        StorageError::WriterTaskRequestFailed {
            request_state,
            source,
        } => (
            source.to_string(),
            Some(WireWriterTaskFailure::RequestFailed {
                request_state: (*request_state).into(),
            }),
        ),
        StorageError::WriterTaskTerminated {
            request_state,
            sqlite_full_codes,
        } => (
            error.to_string(),
            Some(WireWriterTaskFailure::TaskTerminated {
                request_state: (*request_state).into(),
                sqlite_full_codes: *sqlite_full_codes,
            }),
        ),
        _ => (error.to_string(), None),
    };
    EventsResponse::Error {
        message,
        // Defer to the storage layer's own transience classifier rather than
        // re-enumerating variants here: a hand-rolled subset silently turns
        // transient writer contention (`WriterTaskBusy`, `Transaction`) into
        // a terminal error on the client side of the socket.
        retryable: error.is_retryable(),
        writer_task_failure,
        sqlite_write_failure: error.sqlite_write_failure(),
        read_failure: None,
    }
}
