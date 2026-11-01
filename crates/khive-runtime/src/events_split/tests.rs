#[cfg(unix)]
use super::events_daemon::{
    acquire_events_daemon_guard_outcome, events_readiness, run_events_daemon,
    try_acquire_events_daemon_guard, EventsDaemonGuardAcquisition, EventsNotReadyReason,
};
#[cfg(unix)]
use super::file_hardening::{
    ensure_events_db_owner_only, events_db_targets, harden_events_db_sidecars,
    verify_events_db_owner_only_unopened, EventsFileIdentity,
};
#[cfg(unix)]
use super::forwarder::{run_forwarder, EventsSplitClient, ForwardingCounters, QueuedAppend};
use super::paths::{events_db_path_beside, events_socket_path_beside};
#[cfg(unix)]
use super::paths::{
    EVENTS_PROTOCOL_VERSION, MAX_INFLIGHT_REQUEST_BYTES, MAX_QUERY_EVENTS_PAGE_ROWS,
};
use super::protocol::EventsResponse;
#[cfg(unix)]
use super::protocol::{EventsRequest, WireWriterTaskFailure, WireWriterTaskState};
#[cfg(unix)]
use super::registry::EventsSplitConfig;
use super::registry::{direct_backend_for, TestRegistryGuard};
#[cfg(unix)]
use super::server::{
    dispatch_events_request, namespace_store_with_cap, read_frame_budgeted, serve_events_conn,
    storage_error_response, NamespaceStores,
};
#[cfg(unix)]
use super::stores::ForwardingEventStore;
use super::stores::SplitEventStore;
#[cfg(unix)]
use crate::daemon::{read_frame, write_frame};
use khive_db::StorageBackend;
use khive_storage::{Event, EventFilter, EventStore, PageRequest, StorageError};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

#[test]
fn fixture_registry_cleanup_preserves_other_roots() {
    let live_dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let _live_guard = TestRegistryGuard::new(live_dir.path());
    let live_path = live_dir.path().join("events.db");
    let live_backend = direct_backend_for(&live_path).unwrap();

    let finished_dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let finished_backend = {
        let _finished_guard = TestRegistryGuard::new(finished_dir.path());
        let backend = direct_backend_for(&finished_dir.path().join("events.db")).unwrap();
        let weak = Arc::downgrade(&backend);
        drop(backend);
        assert!(
            weak.upgrade().is_some(),
            "registry retains the live fixture"
        );
        weak
    };

    assert!(
        finished_backend.upgrade().is_none(),
        "FD_FIXTURE_REGISTRY_RELEASED"
    );
    let same_live_backend = direct_backend_for(&live_path).unwrap();
    assert!(
        Arc::ptr_eq(&live_backend, &same_live_backend),
        "FD_FIXTURE_OTHER_ROOT_RETAINED"
    );
}

#[cfg(unix)]
mod target_filter_tests {
    include!("tests/target_filter.rs");
}

// The full daemon/forwarding test suite for this module lands with the
// final slice of this series; these tests cover the naming and
// classification contracts the module itself defines.

include!("tests/sidecar_path.rs");

#[cfg(unix)]
mod store_trust_tests {
    use crate::events_split::file_hardening::{ensure_events_db_parent_trusted, events_db_targets};
    use crate::events_split::registry::{
        direct_backend_for, direct_backend_read_only_for, TestRegistryGuard,
    };
    use std::path::Path;
    include!("tests/store_trust.rs");
}

#[cfg(unix)]
#[test]
fn deep_symlink_chains_within_the_kernel_bound_derive_the_target_sidecar() {
    // Linux resolves up to 40 links per lookup; a 35-hop dangling chain
    // therefore opens fine, so it must ALSO derive the target's sidecar
    // — a resolver stopping short would split audit records between the
    // alias and target spellings of one database.
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let real = dir.path().join("real.db");
    let mut prev = real.clone();
    for i in 0..35 {
        let link = dir.path().join(format!("hop{i}.db"));
        std::os::unix::fs::symlink(&prev, &link).unwrap();
        prev = link;
    }
    assert_eq!(
        events_db_path_beside(&prev),
        events_db_path_beside(&real),
        "a 35-hop dangling chain must derive the target's sidecar"
    );
    // Control: an unrelated dangling path still derives its own name.
    assert_ne!(
        events_db_path_beside(&dir.path().join("unrelated.db")),
        events_db_path_beside(&real)
    );
}

include!("tests/sidecar_hardening.rs");
include!("tests/lock_search_parent.rs");

#[cfg(unix)]
fn write_owner_only_test_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, b"").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(unix)]
#[test]
fn unopened_check_refuses_same_mode_regular_file_replacement() {
    // Must fail with the old mode-only verifier: every replacement remains
    // a regular 0600 file. Creating it before rename guarantees a different
    // live inode, without relying on inode reuse after unlink or on timing.
    for target_index in 0..3 {
        let dir = crate::test_process::private_tempdir_builder()
            .tempdir()
            .unwrap();
        let db = dir.path().join("events.db");
        let targets = events_db_targets(&db);
        for path in &targets {
            write_owner_only_test_file(path);
        }
        let before_open = harden_events_db_sidecars(&db).unwrap();
        let replacement = dir.path().join("replacement");
        write_owner_only_test_file(&replacement);
        let replacement_id =
            EventsFileIdentity::from_metadata(&std::fs::symlink_metadata(&replacement).unwrap());
        assert_ne!(before_open[target_index], Some(replacement_id));
        // Keep a real SQLite connection open across the pathname change.
        // Do not run SQL against deliberately replaced files afterward.
        let _connection = rusqlite::Connection::open(&db).unwrap();
        verify_events_db_owner_only_unopened(&db, &before_open).unwrap();
        std::fs::rename(&replacement, &targets[target_index]).unwrap();
        let error = verify_events_db_owner_only_unopened(&db, &before_open)
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed identity"), "{error}");
        assert!(
            error.contains(&targets[target_index].display().to_string()),
            "{error}"
        );
    }
}

#[cfg(unix)]
#[test]
fn unopened_check_accepts_sidecars_missing_at_check_time() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let db = dir.path().join("events.db");
    let targets = events_db_targets(&db);
    for path in &targets {
        write_owner_only_test_file(path);
    }
    let before_open = harden_events_db_sidecars(&db).unwrap();
    let _connection = rusqlite::Connection::open(&db).unwrap();
    for path in targets.iter().skip(1) {
        std::fs::remove_file(path).unwrap();
        verify_events_db_owner_only_unopened(&db, &before_open)
            .expect("SQLite sidecars may disappear without losing the main database");
    }
}

#[cfg(unix)]
#[test]
fn unopened_check_refuses_main_database_removed_after_sqlite_open() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let db = dir.path().join("events.db");
    ensure_events_db_owner_only(&db).unwrap();
    let before_open = harden_events_db_sidecars(&db).unwrap();
    let backend = StorageBackend::sqlite_for_test(&db).unwrap();
    backend.events().unwrap();
    verify_events_db_owner_only_unopened(&db, &before_open).unwrap();
    std::fs::remove_file(&db).unwrap();
    // Must fail with the old NotFound-for-every-target behavior. The live
    // connection can still name an unlinked inode; it must not be served.
    let error = verify_events_db_owner_only_unopened(&db, &before_open)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("main database") && error.contains("disappeared"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn unopened_check_accepts_fresh_sqlite_sidecars_and_unchanged_main() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let db = dir.path().join("new.events.db");
    assert!(!db.exists());
    ensure_events_db_owner_only(&db).unwrap();
    let before_open = harden_events_db_sidecars(&db).unwrap();
    assert!(before_open[0].is_some());
    assert_eq!(&before_open[1..], &[None, None]);
    let backend = StorageBackend::sqlite_for_test(&db).unwrap();
    backend.events().unwrap();
    for path in events_db_targets(&db).iter().skip(1) {
        assert!(path.exists(), "SQLite creates {}", path.display());
    }
    verify_events_db_owner_only_unopened(&db, &before_open)
        .expect("new SQLite sidecars have no pre-open identity to contradict");
}

#[test]
fn socket_derives_beside_the_sidecar() {
    let db = events_db_path_beside(Path::new("/data/khive.db"));
    let socket = events_socket_path_beside(&db);
    assert!(socket.ends_with("khive.db.events.sock"), "got {socket:?}");
}

#[test]
fn bare_relative_main_db_yields_an_absolute_sidecar() {
    // A bare file name has `Some("")` for a parent; every downstream
    // consumer (lock parenting, socket-dir trust validation) needs a
    // real directory, so the sidecar path must come back anchored.
    let path = events_db_path_beside(Path::new("khive.db"));
    assert!(path.is_absolute(), "got relative {path:?}");
    assert!(path.ends_with("khive.db.events.db"), "got {path:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn over_cap_query_page_limit_is_refused_before_materialization() {
    // `PageRequest.limit` arrives client-controlled off the wire; the
    // daemon must refuse an over-cap page with a typed, actionable error
    // instead of materializing and serializing it — and must never
    // silently clamp, or the short page reads as end-of-data.
    let backend = Arc::new(StorageBackend::memory().unwrap());
    let stores: NamespaceStores = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let request = |limit: u32| EventsRequest::QueryEvents {
        protocol_version: EVENTS_PROTOCOL_VERSION,
        namespace: "local".to_string(),
        filter: EventFilter::default(),
        page: PageRequest { offset: 0, limit },
    };
    let refused =
        dispatch_events_request(request(MAX_QUERY_EVENTS_PAGE_ROWS + 1), &backend, &stores).await;
    match refused {
        EventsResponse::Error {
            message,
            retryable,
            writer_task_failure,
            ..
        } => {
            assert!(
                message.contains(&MAX_QUERY_EVENTS_PAGE_ROWS.to_string()),
                "refusal must name the cap: {message}"
            );
            assert!(!retryable, "an over-cap page is not transient");
            assert!(writer_task_failure.is_none());
        }
        other => panic!("over-cap query must be refused, got {other:?}"),
    }
    // Control: a request AT the cap passes admission and reaches the
    // store — an empty page, not a refusal.
    let at_cap =
        dispatch_events_request(request(MAX_QUERY_EVENTS_PAGE_ROWS), &backend, &stores).await;
    assert!(
        matches!(at_cap, EventsResponse::Pageful { .. }),
        "at-cap query must reach the store, got {at_cap:?}"
    );
}

#[cfg(unix)]
#[test]
fn lease_contention_crosses_events_wire_as_retryable() {
    assert_eq!(EVENTS_PROTOCOL_VERSION, 6);
    for (phase, expected_retryable) in [
        (khive_storage::CapacityUnavailablePhase::LockContended, true),
        (khive_storage::CapacityUnavailablePhase::Lock, false),
    ] {
        let response = storage_error_response(&StorageError::CapacityUnavailable {
            capability: khive_storage::StorageCapability::Sql,
            phase,
            message: "lease admission failed".into(),
        });
        let encoded = serde_json::to_vec(&response).expect("serialize server error");
        let decoded: EventsResponse =
            serde_json::from_slice(&encoded).expect("decode unchanged protocol");
        let EventsResponse::Error { retryable, .. } = decoded else {
            panic!("capacity failure must be an error frame");
        };
        assert_eq!(retryable, expected_retryable, "{phase:?}");
    }
}

#[cfg(unix)]
#[test]
fn side_effects_unknown_state_crosses_the_wire_as_terminal_failure() {
    let response = storage_error_response(&StorageError::writer_task_terminated(
        khive_storage::WriterTaskRequestState::SideEffectsUnknown,
    ));
    assert!(
        matches!(
            response,
            EventsResponse::Error {
                writer_task_failure: Some(WireWriterTaskFailure::TaskTerminated {
                    request_state: WireWriterTaskState::SideEffectsUnknown,
                    sqlite_full_codes: None,
                }),
                ..
            }
        ),
        "an unknown-commit-state termination must carry its state on the wire"
    );
    // Any other refusal must NOT carry a writer state.
    let busy = storage_error_response(&StorageError::WriterTaskBusy { timeout_ms: 5 });
    assert!(matches!(
        busy,
        EventsResponse::Error {
            writer_task_failure: None,
            sqlite_write_failure: None,
            read_failure: None,
            ..
        }
    ));
}

#[cfg(unix)]
#[test]
fn proven_rollback_crosses_the_wire_without_claiming_task_termination() {
    let response = storage_error_response(&StorageError::WriterTaskRequestFailed {
        request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
        source: Box::new(StorageError::Pool {
            operation: "writer_task_commit".into(),
            message: "commit refused".into(),
        }),
    });
    assert!(matches!(
        response,
        EventsResponse::Error {
            writer_task_failure: Some(WireWriterTaskFailure::RequestFailed {
                request_state: WireWriterTaskState::TransactionRolledBack,
            }),
            ..
        }
    ));
}

#[test]
fn error_frames_without_writer_failure_still_parse() {
    // `writer_task_failure` is `#[serde(default)]`: an Error frame for a
    // non-writer failure (parse error, version refusal) omits it and the
    // client must read `None`, not fail the parse.
    let bytes = br#"{"kind":"error","message":"boom","retryable":true}"#;
    let parsed: EventsResponse = serde_json::from_slice(bytes).expect("stateless frame parses");
    assert!(matches!(
        parsed,
        EventsResponse::Error {
            retryable: true,
            writer_task_failure: None,
            sqlite_write_failure: None,
            read_failure: None,
            ..
        }
    ));
}

fn split_retry_event(verb: &str) -> Event {
    Event::new(
        "test",
        verb,
        khive_types::EventKind::RecallExecuted,
        khive_types::SubstrateKind::Note,
        "agent:test",
    )
}

fn store_pair(dir: &Path) -> (Arc<dyn EventStore>, Arc<dyn EventStore>) {
    let legacy = direct_backend_for(&dir.join("legacy.db"))
        .expect("legacy backend")
        .events_for_namespace("test")
        .expect("legacy store");
    let lane = direct_backend_for(&dir.join("lane.db"))
        .expect("lane backend")
        .events_for_namespace("test")
        .expect("lane store");
    (legacy, lane)
}

/// A retried audit batch whose first attempt landed on the legacy store
/// before the cutover must be answered by the legacy store's comparison,
/// never re-inserted into the lane — otherwise merged reads and counts
/// double-count the id.
#[tokio::test]
async fn idempotent_retry_of_legacy_resident_rows_does_not_duplicate() {
    use khive_storage::event::EventAppendDisposition;

    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let (legacy, lane) = store_pair(dir.path());

    let resident = split_retry_event("recall");
    legacy
        .append_events_idempotent(vec![resident.clone()])
        .await
        .expect("pre-cutover landing");

    let split = SplitEventStore::new(Arc::clone(&legacy), Arc::clone(&lane));
    let fresh = split_retry_event("search");
    let result = split
        .append_events_idempotent(vec![resident.clone(), fresh.clone()])
        .await
        .expect("mixed retry batch");
    assert_eq!(
        result.rows,
        vec![
            EventAppendDisposition::AlreadyPresentIdentical,
            EventAppendDisposition::Inserted,
        ],
        "input order must be preserved across the two sub-batches"
    );
    assert_eq!(
        lane.count_events(EventFilter::default()).await.unwrap(),
        1,
        "the legacy-resident row must not reach the lane"
    );
    assert_eq!(
        split.count_events(EventFilter::default()).await.unwrap(),
        2,
        "merged count must not double-count the retried id"
    );

    // A conflicting retry of the resident row reports the conflict from
    // the legacy comparison and inserts nowhere.
    let mut mutated = resident.clone();
    mutated.verb = "other".to_string();
    let conflict = split
        .append_events_idempotent(vec![mutated])
        .await
        .expect("conflicting retry");
    assert_eq!(
        conflict.rows,
        vec![EventAppendDisposition::IdentityConflict]
    );
    assert_eq!(split.count_events(EventFilter::default()).await.unwrap(), 2);
}

/// The merged plane refuses to materialize an unbounded offset prefix;
/// the refusal names the cursor remedy, and the bound itself is usable.
#[tokio::test]
async fn merged_offset_window_is_bounded() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let (legacy, lane) = store_pair(dir.path());
    let split = SplitEventStore::new(legacy, lane);

    let err = split
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: SplitEventStore::MAX_MERGED_WINDOW_ROWS,
                limit: 1,
            },
        )
        .await
        .expect_err("a window past the bound must be refused");
    assert!(
        matches!(err, StorageError::InvalidInput { .. }),
        "got {err:?}"
    );
    assert!(
        err.to_string().contains("before"),
        "the refusal must name the cursor remedy: {err}"
    );

    let at_bound = split
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: SplitEventStore::MAX_MERGED_WINDOW_ROWS - 1,
                limit: 1,
            },
        )
        .await
        .expect("a window at the bound is admitted");
    assert!(at_bound.items.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn stateless_non_retryable_error_maps_terminal_not_writer_terminated() {
    // The terminal arm of the client mapping, pinned deliberately: an
    // Error frame with `retryable: false` and NO `writer_task_failure` is
    // a non-writer failure (parse error, version refusal) and must map
    // to terminal `InvalidInput` — never to `WriterTaskTerminated`
    // (there is no state to reconstruct) and never to the retryable
    // `Pool` arm (retrying a version refusal cannot succeed).
    let client = EventsSplitClient::new(std::path::PathBuf::from("/tmp/never-bound.sock"))
        .expect("client builds");
    let store = ForwardingEventStore::new("test", client);
    let bytes = br#"{"kind":"error","message":"refused","retryable":false}"#;
    let parsed: EventsResponse = serde_json::from_slice(bytes).expect("frame parses");
    assert!(matches!(
        store.unexpected("append", parsed),
        StorageError::InvalidInput { .. }
    ));
    // Control: the same non-retryable frame WITH a writer state must
    // take the reconstruction arm instead — proving the terminal arm
    // above is selected by the field's absence, not by `retryable`.
    let with_state = br#"{"kind":"error","message":"died","retryable":false,"writer_task_failure":{"kind":"task_terminated","request_state":"side_effects_unknown"}}"#;
    let parsed: EventsResponse = serde_json::from_slice(with_state).expect("stateful frame parses");
    assert!(matches!(
        store.unexpected("append", parsed),
        StorageError::WriterTaskTerminated { .. }
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn writer_task_states_cross_the_wire_verbatim() {
    // The ADR-133 retry classifier decides per-state, so every
    // WriterTaskTerminated state must survive server → wire → client
    // reconstruction exactly. Before this field existed, NotStarted and
    // TransactionRolledBack crossed as a generic non-retryable refusal
    // and came back as terminal InvalidInput — a broken retry contract.
    use khive_storage::WriterTaskRequestState as S;
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let client =
        EventsSplitClient::new(dir.path().join("never-bound.sock")).expect("client builds");
    let store = ForwardingEventStore::new("test", client);
    for state in [
        S::NotStarted,
        S::TransactionRolledBack,
        S::SideEffectsUnknown,
    ] {
        let response = storage_error_response(&StorageError::writer_task_terminated(state));
        // Round-trip through serde like the socket does.
        let bytes = serde_json::to_vec(&response).unwrap();
        let parsed: EventsResponse = serde_json::from_slice(&bytes).unwrap();
        let err = store.unexpected("append_events_idempotent", parsed);
        assert!(
            matches!(
                err,
                StorageError::WriterTaskTerminated { request_state, .. } if request_state == state
            ),
            "state {state:?} did not survive the socket: got {err:?}"
        );
    }
    // Control: a non-writer-task refusal must NOT carry the field.
    let busy = storage_error_response(&StorageError::WriterTaskBusy { timeout_ms: 5 });
    assert!(matches!(
        busy,
        EventsResponse::Error {
            writer_task_failure: None,
            sqlite_write_failure: None,
            read_failure: None,
            ..
        }
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn native_write_stages_survive_direct_and_request_error_frames() {
    use khive_storage::error::{SqliteWriteFailure, SqliteWriteStage};
    use khive_storage::WriterTaskRequestState;
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let client = EventsSplitClient::new(dir.path().join("never-bound.sock")).unwrap();
    let store = ForwardingEventStore::new("test", client);
    for (stage, extended) in [
        (SqliteWriteStage::Begin, rusqlite::ffi::SQLITE_BUSY_RECOVERY),
        (
            SqliteWriteStage::Statement,
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
        ),
        (SqliteWriteStage::Commit, rusqlite::ffi::SQLITE_IOERR_FSYNC),
        (SqliteWriteStage::Statement, rusqlite::ffi::SQLITE_FULL),
    ] {
        for request in [false, true] {
            for retryable in [false, true] {
                let failure = SqliteWriteFailure {
                    stage,
                    primary_code: extended & 0xff,
                    extended_code: extended,
                    settlement_unknown: false,
                };
                let source = if retryable {
                    StorageError::Pool {
                        operation: "append".into(),
                        message: "no code in this message".into(),
                    }
                } else {
                    StorageError::InvalidInput {
                        capability: khive_storage::StorageCapability::Events,
                        operation: "append".into(),
                        message: "no code in this message".into(),
                    }
                }
                .with_sqlite_write_failure(failure);
                let source = if request {
                    StorageError::WriterTaskRequestFailed {
                        request_state: WriterTaskRequestState::TransactionRolledBack,
                        source: Box::new(source),
                    }
                } else {
                    source
                };
                let bytes = serde_json::to_vec(&storage_error_response(&source)).unwrap();
                let error = store.unexpected("append", serde_json::from_slice(&bytes).unwrap());
                assert_eq!(error.sqlite_write_failure(), Some(failure));
                assert_eq!(error.is_retryable(), retryable);
                assert_eq!(
                    matches!(
                        &error,
                        StorageError::WriterTaskRequestFailed {
                            request_state: WriterTaskRequestState::TransactionRolledBack,
                            ..
                        }
                    ),
                    request
                );
                let wire =
                    crate::runtime_error_value(error.into(), crate::DomainDisposition::Unknown);
                let expected = if extended == rusqlite::ffi::SQLITE_FULL {
                    "sqlite_disk_full"
                } else {
                    stage.as_str()
                };
                assert_eq!(wire["stage"], expected);
                assert_eq!(wire["sqlite_primary_code"], failure.primary_code);
                assert_eq!(wire["sqlite_extended_code"], extended);
                if request {
                    assert_eq!(wire["task_terminated"], false);
                }
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_native_evidence_keeps_unknown_outcome_precedence() {
    use khive_storage::error::{SqliteWriteFailure, SqliteWriteStage};
    use khive_storage::WriterTaskRequestState;
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let client = EventsSplitClient::new(dir.path().join("never-bound.sock")).unwrap();
    let store = ForwardingEventStore::new("test", client);
    for terminal in [false, true] {
        for extended in [
            rusqlite::ffi::SQLITE_IOERR_FSYNC,
            rusqlite::ffi::SQLITE_FULL,
        ] {
            let failure = SqliteWriteFailure {
                stage: SqliteWriteStage::Commit,
                primary_code: extended & 0xff,
                extended_code: extended,
                settlement_unknown: false,
            };
            // Even serialized false evidence cannot override an existing unknown outcome.
            let source = if terminal {
                StorageError::WriterTaskTerminated {
                    request_state: WriterTaskRequestState::SideEffectsUnknown,
                    sqlite_full_codes: (extended == rusqlite::ffi::SQLITE_FULL)
                        .then_some((extended, extended)),
                }
            } else {
                StorageError::WriterTaskRequestFailed {
                    request_state: WriterTaskRequestState::SideEffectsUnknown,
                    source: Box::new(StorageError::Internal("original cause".into())),
                }
            }
            .with_sqlite_write_failure(failure);
            let bytes = serde_json::to_vec(&storage_error_response(&source)).unwrap();
            let error = store.unexpected("append", serde_json::from_slice(&bytes).unwrap());
            assert_eq!(error.sqlite_write_failure(), Some(failure));
            if terminal {
                assert!(std::error::Error::source(error.without_sqlite_write_stage()).is_none());
            }
            let wire = crate::runtime_error_value(error.into(), crate::DomainDisposition::Unknown);
            let expected = if extended == rusqlite::ffi::SQLITE_FULL {
                "sqlite_disk_full"
            } else if terminal {
                "writer_task_terminated"
            } else {
                "writer_task_request_failed"
            };
            assert_eq!(wire["code"], expected);
            assert_eq!(wire["stage"], expected);
            assert_eq!(wire["sqlite_write_stage"], "sqlite_commit_failure");
            assert_eq!(wire["sqlite_extended_code"], extended);
            assert_eq!(wire["request_state"], "side_effects_unknown");
            assert_eq!(wire["task_terminated"], terminal);
            assert_eq!(wire["retryable"], false);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn poisoned_commit_evidence_does_not_claim_definite_failure() {
    use khive_storage::error::{SqliteWriteFailure, SqliteWriteStage};
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let client = EventsSplitClient::new(dir.path().join("never-bound.sock")).unwrap();
    let store = ForwardingEventStore::new("test", client);
    let failure = SqliteWriteFailure {
        stage: SqliteWriteStage::Commit,
        primary_code: rusqlite::ffi::SQLITE_AUTH,
        extended_code: rusqlite::ffi::SQLITE_AUTH,
        settlement_unknown: true,
    };
    let original = StorageError::driver(
        khive_storage::StorageCapability::Sql,
        "execute_batch",
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_AUTH), None),
    );
    let source = original.with_sqlite_write_failure(failure);
    let bytes = serde_json::to_vec(&storage_error_response(&source)).unwrap();
    let error = store.unexpected("append", serde_json::from_slice(&bytes).unwrap());
    assert_eq!(error.sqlite_write_failure(), Some(failure));
    assert!(!error.is_retryable());
    let wire = crate::runtime_error_value(error.into(), crate::DomainDisposition::Unknown);
    assert!(wire.get("code").is_none());
    assert!(wire.get("stage").is_none());
    assert_eq!(wire["sqlite_write_stage"], "sqlite_commit_failure");
    assert_eq!(wire["sqlite_primary_code"], rusqlite::ffi::SQLITE_AUTH);
    assert_eq!(wire["domain_disposition"], "unknown");
}

#[cfg(unix)]
#[tokio::test]
async fn proven_rollback_round_trips_as_non_terminal_request_failure() {
    use khive_storage::WriterTaskRequestState as S;

    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let client =
        EventsSplitClient::new(dir.path().join("never-bound.sock")).expect("client builds");
    let store = ForwardingEventStore::new("test", client);
    let response = storage_error_response(&StorageError::WriterTaskRequestFailed {
        request_state: S::TransactionRolledBack,
        source: Box::new(StorageError::Pool {
            operation: "writer_task_commit".into(),
            message: "commit refused".into(),
        }),
    });
    let bytes = serde_json::to_vec(&response).unwrap();
    let parsed: EventsResponse = serde_json::from_slice(&bytes).unwrap();
    let err = store.unexpected("append_events_idempotent", parsed);
    assert!(
        matches!(
            err,
            StorageError::WriterTaskRequestFailed {
                request_state: S::TransactionRolledBack,
                ..
            }
        ),
        "proven rollback must not reconstruct as a terminal writer: {err:?}"
    );
}

#[cfg(unix)]
#[test]
fn direct_backend_hardens_preexisting_db_and_sidecars() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let db = dir.path().join("pre-existing.events.db");
    let wal = dir.path().join("pre-existing.events.db-wal");
    std::fs::write(&db, b"").unwrap();
    std::fs::write(&wal, b"").unwrap();
    for path in [&db, &wal] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    // Control: the loose mode is really in place before the open.
    assert_eq!(
        std::fs::metadata(&db).unwrap().permissions().mode() & 0o777,
        0o644
    );
    direct_backend_for(&db).expect("writable open succeeds");
    for path in [&db, &wal] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600,
            "pre-existing {} must be tightened to owner-only",
            path.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn namespace_store_cache_is_bounded_and_trim_normalized() {
    let backend = StorageBackend::memory().unwrap();
    let stores: NamespaceStores = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    // cap=1: the first namespace caches; the second evicts it and takes
    // the slot — bounded, and never condemned to per-request rebuilds.
    namespace_store_with_cap(&backend, &stores, "alpha", 1).expect("first store");
    namespace_store_with_cap(&backend, &stores, "beta", 1).expect("second store evicts");
    {
        let map = stores.lock().unwrap();
        assert_eq!(map.len(), 1, "cache must not grow past its cap");
        assert!(
            map.contains_key("beta"),
            "the newest namespace must be admitted at the cap"
        );
    }
    // Trimmed spelling of a cached namespace hits the same entry rather
    // than minting a duplicate handle (the backend trims namespaces).
    namespace_store_with_cap(&backend, &stores, "beta", 1).expect("cached beta");
    namespace_store_with_cap(&backend, &stores, "  beta  ", 1).expect("trimmed spelling");
    {
        let map = stores.lock().unwrap();
        assert_eq!(
            map.len(),
            1,
            "spellings of one namespace must share one entry"
        );
        assert!(map.contains_key("beta"), "trimmed key is the cache key");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn frame_budget_admits_before_allocating_and_releases_after() {
    let (mut client, mut server) = UnixStream::pair().expect("socketpair");
    let budget = Arc::new(tokio::sync::Semaphore::new(1024));
    let payload = vec![7u8; 100];
    write_frame(&mut client, &payload).await.expect("write");
    let (bytes, permit) = read_frame_budgeted(&mut server, &budget)
        .await
        .expect("budgeted read");
    assert_eq!(bytes, payload);
    // The permit holds exactly the frame's bytes against the budget…
    assert_eq!(budget.available_permits(), 1024 - 100);
    // …and releases them when the request lifecycle ends.
    drop(permit);
    assert_eq!(budget.available_permits(), 1024);
    // A frame past the per-frame cap is refused before admission: the
    // budget is untouched, so an oversized declaration cannot starve it.
    let mut oversized = Vec::from(((crate::daemon::MAX_FRAME_BYTES + 1) as u32).to_be_bytes());
    oversized.extend_from_slice(&[0u8; 8]);
    use tokio::io::AsyncWriteExt;
    client.write_all(&oversized).await.expect("raw prefix");
    let err = read_frame_budgeted(&mut server, &budget)
        .await
        .expect_err("oversized declaration must refuse");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        budget.available_permits(),
        1024,
        "refusal must not consume budget"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dispatch_rejects_invalid_wire_namespace() {
    let backend = Arc::new(StorageBackend::memory().unwrap());
    let stores: NamespaceStores = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    // An attacker-shaped namespace: far past the 256-byte Namespace
    // bound. Must be refused as a typed non-retryable error before it
    // can become a cache key or a store.
    let huge = "n".repeat(64 * 1024);
    let response = dispatch_events_request(
        EventsRequest::CountEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: huge,
            operator_read_budget_secs: Some(30),
            filter: Default::default(),
        },
        &backend,
        &stores,
    )
    .await;
    assert!(
        matches!(
            &response,
            EventsResponse::Error {
                retryable: false,
                writer_task_failure: None,
                sqlite_write_failure: None,
                read_failure: None,
                ..
            }
        ),
        "oversized namespace must be a typed refusal, got {response:?}"
    );
    assert_eq!(
        stores.lock().unwrap().len(),
        0,
        "a rejected namespace must never enter the cache"
    );
    // Control: a valid namespace on the same dispatch path succeeds.
    let ok = dispatch_events_request(
        EventsRequest::CountEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: "local".to_string(),
            operator_read_budget_secs: Some(30),
            filter: Default::default(),
        },
        &backend,
        &stores,
    )
    .await;
    assert!(
        matches!(ok, EventsResponse::Count { .. }),
        "valid namespace must dispatch, got {ok:?}"
    );
}

include!("tests/shutdown.rs");
include!("tests/readiness.rs");

#[cfg(unix)]
#[tokio::test]
async fn forwarder_abandons_hung_delivery() {
    // A daemon that accepts the connection and then never responds must
    // not park the forwarder forever: the delivery deadline fires, the
    // batch is dropped and counted, and the task stays live.
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let socket = dir.path().join("hung.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    // Accept and hold connections open without ever reading or replying.
    let _server = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        }
    });
    let client = EventsSplitClient::new_with_queue_depth_and_delivery_timeout(
        socket.clone(),
        4,
        Duration::from_millis(100),
    )
    .expect("client builds");
    let event = Event::new(
        "test",
        "noop",
        khive_types::EventKind::Audit,
        khive_types::SubstrateKind::Event,
        "tester",
    );
    client.enqueue("test", vec![event]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if client.metrics().dropped_batches >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "forwarder never abandoned the hung delivery: {:?}",
            client.metrics()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[test]
fn wire_retryability_follows_the_storage_classifier() {
    let busy = storage_error_response(&StorageError::WriterTaskBusy { timeout_ms: 5 });
    assert!(
        matches!(
            busy,
            EventsResponse::Error {
                retryable: true,
                ..
            }
        ),
        "transient writer contention must stay retryable across the socket"
    );
    // A terminated writer task is not retryable per the storage layer's
    // own classifier; the wire response must agree rather than widen it.
    let terminated = storage_error_response(&StorageError::writer_task_terminated(
        khive_storage::WriterTaskRequestState::SideEffectsUnknown,
    ));
    assert!(
        matches!(
            terminated,
            EventsResponse::Error {
                retryable: false,
                ..
            }
        ),
        "a terminated writer must not be reported transient"
    );
}

use khive_storage::event::EventAppendDisposition;
use khive_types::{EventKind, SubstrateKind};

fn test_event(namespace: &str) -> Event {
    Event::new(
        namespace,
        "test.verb",
        EventKind::Audit,
        SubstrateKind::Event,
        "actor:test",
    )
}

/// Boot a real events daemon on temp paths, poll until its socket accepts.
#[cfg(unix)]
async fn boot_daemon(dir: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let db = dir.path().join("events.db");
    let socket = dir.path().join("events.sock");
    let (db_clone, socket_clone) = (db.clone(), socket.clone());
    tokio::spawn(async move {
        let _ = run_events_daemon(&db_clone, &socket_clone).await;
    });
    for _ in 0..100 {
        if UnixStream::connect(&socket).await.is_ok() {
            return (db, socket);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("events daemon did not come up on {}", socket.display());
}

/// The split store's routing contract: plain appends land ONLY in the
/// legacy store (whose raw-SQL consumers depend on finding them there),
/// the idempotent audit lane lands ONLY in the events lane, and reads
/// merge both sides into one event plane.
#[tokio::test]
async fn split_store_routes_plain_to_legacy_idempotent_to_lane_and_merges_reads() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let legacy_backend = direct_backend_for(&dir.path().join("legacy.db")).expect("legacy backend");
    let lane_backend = direct_backend_for(&dir.path().join("lane.db")).expect("lane backend");
    let legacy = legacy_backend
        .events_for_namespace("local")
        .expect("legacy store");
    let lane = lane_backend
        .events_for_namespace("local")
        .expect("lane store");
    let split = SplitEventStore::new(Arc::clone(&legacy), Arc::clone(&lane));

    let plain = test_event("local");
    let plain_id = plain.id;
    split.append_event(plain).await.expect("plain append");

    let audit = test_event("local");
    let audit_id = audit.id;
    let result = split
        .append_events_idempotent(vec![audit])
        .await
        .expect("idempotent append");
    assert_eq!(result.rows, vec![EventAppendDisposition::Inserted]);

    // Routing: each row is in exactly its own side.
    assert!(legacy
        .get_event(plain_id)
        .await
        .expect("legacy get")
        .is_some());
    assert!(lane.get_event(plain_id).await.expect("lane get").is_none());
    assert!(legacy
        .get_event(audit_id)
        .await
        .expect("legacy get")
        .is_none());
    assert!(lane.get_event(audit_id).await.expect("lane get").is_some());

    // Merged reads observe one event plane whichever side holds the row.
    assert!(split
        .get_event(plain_id)
        .await
        .expect("split get")
        .is_some());
    assert!(split
        .get_event(audit_id)
        .await
        .expect("split get")
        .is_some());
    assert_eq!(
        split
            .count_events(EventFilter::default())
            .await
            .expect("split count"),
        2
    );
    let page = split
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("split query");
    let ids: Vec<Uuid> = page.items.iter().map(|e| e.id).collect();
    assert!(ids.contains(&plain_id) && ids.contains(&audit_id));

    // Windowing across the merge: page size 1 at offsets 0 and 1 yields
    // the two rows exactly once each.
    let mut seen = Vec::new();
    for offset in 0..2 {
        let page = split
            .query_events(EventFilter::default(), PageRequest { offset, limit: 1 })
            .await
            .expect("windowed query");
        assert_eq!(page.items.len(), 1);
        seen.push(page.items[0].id);
    }
    seen.sort();
    let mut expected = vec![plain_id, audit_id];
    expected.sort();
    assert_eq!(seen, expected);
}

/// Mechanized co-residency guard: the amendment's load-bearing claim is
/// that every class a raw-SQL consumer of the legacy `events` table
/// depends on still LANDS in that table. Two of the three enumerated
/// consumers (kg projection guard, graph-query union) fail silently if
/// the classification drifts, so the classification is guarded here by
/// the consumers' own access pattern — a raw SQL read against the legacy
/// backend — rather than by enumeration. Arm 1 reddens if plain appends
/// (the provenance/domain class) ever stop reaching the legacy table.
/// Arm 2 is the boundary's other face and doubles as the positive
/// control for the instrument: the same query DOES find lane-routed rows
/// on the lane's own backend, so an empty arm-1 result could not be a
/// broken query.
#[tokio::test]
async fn raw_sql_consumers_of_the_legacy_events_table_still_see_plain_appends() {
    use khive_storage::types::{SqlStatement, SqlValue};

    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let legacy_backend =
        direct_backend_for(&dir.path().join("legacy-guard.db")).expect("legacy backend");
    let lane_backend = direct_backend_for(&dir.path().join("lane-guard.db")).expect("lane backend");
    let split = SplitEventStore::new(
        legacy_backend
            .events_for_namespace("local")
            .expect("legacy store"),
        lane_backend
            .events_for_namespace("local")
            .expect("lane store"),
    );

    // The schedule pack's creator-provenance write is a plain append; the
    // audit flusher's write is an idempotent batch. Route one of each.
    let provenance = test_event("local");
    let provenance_id = provenance.id;
    split.append_event(provenance).await.expect("plain append");
    let audit = test_event("local");
    let audit_id = audit.id;
    split
        .append_events_idempotent(vec![audit])
        .await
        .expect("idempotent append");

    let count_by_raw_sql = |backend: Arc<StorageBackend>, id: Uuid| async move {
        let mut reader = backend.sql().reader().await.expect("sql reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT actor FROM events WHERE id = ?1".to_string(),
                params: vec![SqlValue::Text(id.to_string())],
                label: None,
            })
            .await
            .expect("raw events query");
        rows.len()
    };

    // Arm 1: the co-residency contract. A raw-SQL consumer of the legacy
    // table finds the plain-append row there.
    assert_eq!(
        count_by_raw_sql(Arc::clone(&legacy_backend), provenance_id).await,
        1,
        "plain appends must stay visible to raw-SQL consumers of the legacy events table"
    );
    // Arm 2a: the moved class is genuinely gone from the legacy table —
    // this is the silent-failure face of the boundary, held visible.
    assert_eq!(
        count_by_raw_sql(Arc::clone(&legacy_backend), audit_id).await,
        0,
        "audit-lane rows must not land in the legacy events table"
    );
    // Arm 2b: positive control for the instrument — the identical query
    // finds the moved row on the lane backend, so arm 2a's zero (and any
    // future arm-1 zero) is a routing fact, not a dead query.
    assert_eq!(
        count_by_raw_sql(Arc::clone(&lane_backend), audit_id).await,
        1,
        "the raw query must prove it can find rows where they actually live"
    );
}

/// End to end over the real socket: the idempotent lane returns true
/// dispositions, and the row is readable back through the same store.
#[cfg(unix)]
#[tokio::test]
async fn idempotent_append_round_trips_through_the_daemon() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let (_db, socket) = boot_daemon(&dir).await;
    let client = EventsSplitClient::new(socket).expect("client");
    let store = ForwardingEventStore::new("local", client);

    let event = test_event("local");
    let id = event.id;
    let result = store
        .append_events_idempotent(vec![event.clone()])
        .await
        .expect("idempotent append over socket");
    assert_eq!(result.rows, vec![EventAppendDisposition::Inserted]);

    // Retry of the identical row reports AlreadyPresentIdentical — the
    // daemon ran the real idempotent path, not a blind re-insert.
    let retry = store
        .append_events_idempotent(vec![event])
        .await
        .expect("idempotent retry");
    assert_eq!(
        retry.rows,
        vec![EventAppendDisposition::AlreadyPresentIdentical]
    );

    let fetched = store.get_event(id).await.expect("get over socket");
    assert_eq!(fetched.map(|e| e.id), Some(id));
    let count = store
        .count_events(EventFilter::default())
        .await
        .expect("count over socket");
    assert_eq!(count, 1);
}

/// The fire-and-forget lane delivers to the daemon store: enqueue returns
/// immediately and the row becomes visible to a subsequent count.
#[cfg(unix)]
#[tokio::test]
async fn fire_and_forget_append_lands_in_the_daemon_store() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let (_db, socket) = boot_daemon(&dir).await;
    let client = EventsSplitClient::new(socket).expect("client");
    let store = ForwardingEventStore::new("local", Arc::clone(&client));

    store
        .append_event(test_event("local"))
        .await
        .expect("append_event is fire-and-forget");

    // Poll for BOTH effects: the row landing daemon-side and the
    // forwarder's own counter — the row can be visible a beat before the
    // forwarder task is rescheduled to record the delivery.
    let (count, metrics) = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let count = store
                .count_events(EventFilter::default())
                .await
                .expect("count over socket");
            let metrics = client.metrics();
            if (count == 1 && metrics.forwarded_events >= 1) || metrics.dropped_events > 0 {
                break (count, metrics);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("forwarder must deliver or report a drop within 30 seconds");
    assert_eq!(
        metrics.dropped_events, 0,
        "forwarder dropped an event before it reached the daemon store"
    );
    assert_eq!(count, 1, "forwarded event must land in the daemon store");
    assert!(metrics.forwarded_events >= 1, "delivery must be counted");
}

/// R4 overflow arm: with the daemon dead, appends beyond the queue bound
/// are DROPPED and counted while every append still returns Ok — the
/// domain path proceeds. The drop counter is the loss-tolerance contract.
#[cfg(unix)]
#[tokio::test]
async fn queue_overflow_drops_and_counts_but_never_errors() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let dead_socket = dir.path().join("nobody-home.sock");
    let client = EventsSplitClient::new_with_queue_depth(dead_socket, 2).expect("client");
    let store = ForwardingEventStore::new("local", Arc::clone(&client));

    for _ in 0..20 {
        store
            .append_event(test_event("local"))
            .await
            .expect("append_event must not error under overflow");
    }
    let metrics = client.metrics();
    assert!(
        metrics.dropped_events > 0,
        "overflow must register in the drop counter, got {metrics:?}"
    );
}

/// R4 dead-socket arm: synchronous lanes fail with a typed retryable
/// storage error (never hang, never a generic panic), and the offline
/// preflight validator keeps working with zero I/O.
#[cfg(unix)]
#[tokio::test]
async fn dead_socket_reads_fail_typed_and_preflight_stays_local() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let dead_socket = dir.path().join("nobody-home.sock");
    let client = EventsSplitClient::new(dead_socket).expect("client");
    let store = ForwardingEventStore::new("local", client);

    let error = store
        .get_event(Uuid::new_v4())
        .await
        .expect_err("read against a dead socket must fail");
    assert!(
        matches!(error, StorageError::Pool { .. }),
        "expected the typed unreachable error, got {error:?}"
    );

    // The audit-batch seam contract: preflight is local and functional
    // with the daemon down.
    assert!(store.supports_idempotent_audit_batch());
    store
        .preflight_event(&test_event("local"))
        .expect("offline preflight validates a well-formed event");
}

/// A connected peer that consumed the request but sent no response is a
/// broken response, not evidence that no daemon answered the socket.
#[cfg(unix)]
#[tokio::test]
async fn connected_events_peer_closing_without_response_is_not_unreachable() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let socket = dir.path().join("closes-after-request.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_frame(&mut stream)
            .await
            .expect("complete request arrives");
        // Drop the connection without writing a response frame.
    });
    let client = EventsSplitClient::new(socket).unwrap();
    let store = ForwardingEventStore::new("local", client);
    let error = store.get_event(Uuid::new_v4()).await.unwrap_err();
    assert!(
        matches!(error, StorageError::Serialization { .. }),
        "a post-connect response failure must not be Pool/unreachable: {error:?}"
    );
    server.await.unwrap();
}

/// 4096 ordinary 3-KiB events obey the row limit yet exceed one IPC
/// frame. The daemon must answer with a non-retryable size refusal so a
/// caller can use a narrower page on the same healthy socket.
#[cfg(unix)]
#[tokio::test]
async fn query_page_over_frame_cap_returns_typed_size_refusal() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let socket = dir.path().join("large-query.sock");
    let backend = Arc::new(StorageBackend::memory().unwrap());
    let event_store = backend.events_for_namespace("local").unwrap();
    let events: Vec<_> = (0..MAX_QUERY_EVENTS_PAGE_ROWS)
        .map(|_| {
            test_event("local").with_payload(serde_json::json!({"data": "x".repeat(3 * 1024)}))
        })
        .collect();
    event_store
        .append_events(events)
        .await
        .expect("seed large page");

    let listener = UnixListener::bind(&socket).unwrap();
    let stores: NamespaceStores = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_events_conn(
            stream,
            backend,
            stores,
            Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_REQUEST_BYTES)),
        )
        .await;
    });
    let client = EventsSplitClient::new(socket).unwrap();
    let store = ForwardingEventStore::new("local", client);
    let error = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: MAX_QUERY_EVENTS_PAGE_ROWS,
            },
        )
        .await
        .expect_err("the full page cannot fit one frame");
    assert!(
        matches!(error, StorageError::InvalidInput { .. }),
        "expected non-retryable frame-size refusal, got {error:?}"
    );
    assert!(
        error.to_string().contains("response_frame_size_limit"),
        "{error}"
    );
    assert!(
        error.to_string().contains("request a narrower page"),
        "{error}"
    );
    server.abort();
}

/// The byte budget counts the in-flight batch as well as queued batches.
/// A stalled daemon therefore cannot retain another large batch just
/// because the mpsc channel still has spare batch slots.
#[cfg(unix)]
#[tokio::test]
async fn forwarding_queue_enforces_serialized_byte_and_frame_limits_with_drop_metrics() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let socket = dir.path().join("stalled-forwarder.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _hold_open = stream;
        std::future::pending::<()>().await;
    });
    let event = test_event("local").with_payload(serde_json::json!({"data": "x".repeat(4096)}));
    let first = vec![event.clone(), event.clone()];
    let request_bytes = serde_json::to_vec(&EventsRequest::AppendEvents {
        protocol_version: EVENTS_PROTOCOL_VERSION,
        namespace: "local".into(),
        events: first.clone(),
    })
    .unwrap()
    .len();
    let byte_budget = request_bytes + 64;
    let client = EventsSplitClient::new_with_limits_and_delivery_timeout(
        socket,
        8,
        byte_budget,
        Duration::from_secs(30),
    )
    .unwrap();
    let store = ForwardingEventStore::new("local", Arc::clone(&client));
    store.append_events(first.clone()).await.unwrap();
    assert_eq!(client.metrics().queued_bytes, request_bytes);

    store.append_events(first).await.unwrap();
    let metrics = client.metrics();
    assert_eq!(metrics.dropped_batches, 1);
    assert_eq!(metrics.dropped_events, 2);
    assert_eq!(metrics.queued_bytes, request_bytes);
    assert!(metrics.queued_bytes <= byte_budget);

    // One append request larger than the daemon frame cannot ever be
    // delivered; it must be refused before occupying a queue slot.
    let oversized = test_event("local")
        .with_payload(serde_json::json!({"data": "x".repeat(crate::daemon::MAX_FRAME_BYTES)}));
    store.append_event(oversized).await.unwrap();
    let metrics = client.metrics();
    assert_eq!(metrics.dropped_batches, 2);
    assert_eq!(metrics.dropped_events, 3);
    assert_eq!(metrics.queued_bytes, request_bytes);
    server.abort();
}

/// Version-skew arm: a frame carrying an unknown protocol version gets a
/// typed non-retryable refusal, not a deserialization failure.
#[cfg(unix)]
#[tokio::test]
async fn protocol_version_skew_is_a_typed_refusal() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let (_db, socket) = boot_daemon(&dir).await;

    let mut stream = UnixStream::connect(&socket).await.expect("connect");
    let request = EventsRequest::CountEvents {
        protocol_version: EVENTS_PROTOCOL_VERSION + 1,
        namespace: "local".into(),
        operator_read_budget_secs: Some(30),
        filter: EventFilter::default(),
    };
    let payload = serde_json::to_vec(&request).expect("serialize");
    write_frame(&mut stream, &payload).await.expect("write");
    let bytes = read_frame(&mut stream).await.expect("read");
    let response: EventsResponse = serde_json::from_slice(&bytes).expect("parse");
    match response {
        EventsResponse::Error {
            message, retryable, ..
        } => {
            assert!(!retryable, "version skew is not retryable");
            assert!(message.contains("protocol version"), "message: {message}");
        }
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

/// The per-socket daemon guard is exclusive while held and reusable after
/// release — the mechanism that keeps a supervisor respawn race down to
/// one surviving daemon.
#[cfg(unix)]
#[test]
fn events_daemon_guard_is_exclusive_then_reusable() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let socket = dir.path().join("events.sock");
    let first = match acquire_events_daemon_guard_outcome(&socket) {
        EventsDaemonGuardAcquisition::Held(guard) => guard,
        other => panic!("first acquire must succeed: {other:?}"),
    };
    let second = acquire_events_daemon_guard_outcome(&socket);
    assert!(
        matches!(second, EventsDaemonGuardAcquisition::Contended),
        "second acquire must report contention while the first guard is held: {second:?}"
    );
    drop(first);
    // Another test may fork while the first description is open. A child
    // can retain that flock until exec or exit; retry only that diagnosed
    // contention, never an open/hardening error, and never without a bound.
    const MAX_ATTEMPTS: usize = 100;
    for attempt in 1..=MAX_ATTEMPTS {
        match acquire_events_daemon_guard_outcome(&socket) {
            EventsDaemonGuardAcquisition::Held(_) => return,
            EventsDaemonGuardAcquisition::Contended if attempt < MAX_ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => panic!(
                "acquire must succeed after release within {MAX_ATTEMPTS} attempts; \
                 attempt {attempt}: {other:?}"
            ),
        }
    }
    unreachable!("the final acquisition attempt returns or reports its refusal");
}

#[cfg(unix)]
#[test]
fn events_daemon_guard_open_failure_is_not_contention() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let clean = dir.path().join("clean.sock");
    assert!(matches!(
        acquire_events_daemon_guard_outcome(&clean),
        EventsDaemonGuardAcquisition::Held(_)
    ));

    assert!(
        try_acquire_events_daemon_guard(&clean).is_some(),
        "the public Option entrance must preserve a successful acquisition"
    );

    // Opening a directory for writing must fail even when no process
    // holds a daemon lock. Exercise the real open, not an injected result.
    let socket = dir.path().join("blocked.sock");
    let lock_path = socket.with_extension("lock");
    std::fs::create_dir(&lock_path).expect("create directory at lock entry");
    let outcome = acquire_events_daemon_guard_outcome(&socket);
    assert!(
        matches!(outcome, EventsDaemonGuardAcquisition::OpenFailed(_)),
        "an actual lock-file open failure must not report contention: {outcome:?}"
    );
    assert!(
        try_acquire_events_daemon_guard(&socket).is_none(),
        "the public Option entrance must refuse an actual open failure"
    );
    assert!(
        lock_path.is_dir(),
        "the refused entry must remain a directory"
    );
}

/// A read-only file-backed runtime with the split configured must never
/// create or schema-initialize an events database. Missing events db →
/// `events()` serves the legacy store alone and leaves the filesystem
/// untouched; a pre-existing events db (minted by an earlier writable
/// host) is opened read-only and its rows merge into reads.
/// (unix-gated for the sidecar-freeze harness step only.)
#[cfg(unix)]
#[tokio::test]
async fn read_only_runtime_never_creates_an_events_db() {
    use crate::{KhiveRuntime, Namespace, RuntimeConfig};

    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let main_db = dir.path().join("main.db");
    // Materialize + migrate the main database with a writable runtime.
    drop(
        KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: Some(main_db.clone()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("create main db"),
    );
    let events_db = events_db_path_beside(&main_db);
    assert!(!events_db.exists(), "precondition: no events db yet");

    // Freeze the WAL sidecars the writable runtime left behind — the
    // read-only opener refuses a snapshot with a writable -shm.
    khive_storage::test_support::freeze_snapshot_sidecars(&main_db);

    let split_config = |db: PathBuf| RuntimeConfig {
        db_path: Some(main_db.clone()),
        events_split: Some(EventsSplitConfig {
            db_path: db,
            socket_path: None,
        }),
        ..RuntimeConfig::no_embeddings()
    };

    // Arm 1: read-only runtime, no events db on disk. Reads work through
    // the legacy store and nothing is minted.
    let ro = KhiveRuntime::new_readonly_for_test(split_config(events_db.clone()))
        .expect("read-only runtime");
    let token = ro.authorize(Namespace::local()).expect("token");
    let store = ro.events(&token).expect("events store");
    let count = store
        .count_events(EventFilter::default())
        .await
        .expect("count through legacy-only plane");
    assert_eq!(count, 0);
    assert!(
        !events_db.exists(),
        "a read-only runtime must not mint the events database"
    );

    // Arm 2: a writable host mints the events db and lands a lane row;
    // the read-only runtime then opens it read-only and merges the row.
    // The writer is scoped (not the process-global registry) and its WAL
    // sidecars frozen after drop — the read-only opener rightly refuses a
    // snapshot with a live writer, and that refusal is not this test's
    // subject.
    {
        let lane_backend = StorageBackend::sqlite_for_test(&events_db).expect("writable lane");
        lane_backend
            .events_for_namespace("local")
            .expect("lane store")
            .append_event(test_event("local"))
            .await
            .expect("seed lane row");
    }
    khive_storage::test_support::freeze_snapshot_sidecars(&events_db);
    let store = ro.events(&token).expect("events store with lane present");
    let count = store
        .count_events(EventFilter::default())
        .await
        .expect("merged count");
    assert_eq!(count, 1, "the pre-existing lane row must merge into reads");
}

/// Embedded (direct) mode: the process-global backend writes and reads
/// `events.db` without any daemon.
#[tokio::test]
async fn direct_mode_appends_and_reads_without_a_daemon() {
    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let db = dir.path().join("events.db");
    let backend = direct_backend_for(&db).expect("direct backend");
    let store = backend.events_for_namespace("local").expect("store");
    store
        .append_event(test_event("local"))
        .await
        .expect("direct append");
    let count = store
        .count_events(EventFilter::default())
        .await
        .expect("count");
    assert_eq!(count, 1);
}
