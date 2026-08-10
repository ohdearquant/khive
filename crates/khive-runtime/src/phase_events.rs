//! Shared helpers for ADR-103 phase-span event emission.
//!
//! `PhaseStarted` / `PhaseCompleted` / `PhaseCancelled` are ADR-094's
//! additive `EventKind` mechanism, extended by ADR-103 Decision (c) for
//! background work that is not itself a verb dispatch. `khive-pack-memory`'s
//! ANN background-rebuild task (`ann.rs`) originated the emission and
//! terminal-selection pattern this module lifts into a shared home so
//! ADR-103 Amendment 1 Part 2's two daemon-startup embedder-warmup hooks
//! (`KgPack::warm`, `KnowledgePack::warm`) can reuse it without duplicating
//! either the event-append plumbing or the shutdown-cancellation
//! classification.

use crate::error::RuntimeError;
use crate::runtime::{KhiveRuntime, NamespaceToken};
use khive_storage::{Event, StorageError, SubstrateKind};

/// True when `err` is the direct result of a `spawn_blocking` cancellation,
/// e.g. a short-lived process (or daemon shutdown) tearing the runtime down
/// mid-operation, rather than a genuine backend/driver failure.
///
/// Matches the concrete `tokio::task::JoinError` boxed inside
/// `StorageError::Driver` (the shape `with_reader`/`with_writer` produce
/// when their `spawn_blocking(...).await` is cut short) via a typed
/// downcast, not a message substring, so a real driver/SQL error is never
/// misclassified as benign.
///
/// Every ADR-103 phase-span emitter that must pick between `PhaseCompleted`
/// and `PhaseCancelled` on a shutdown-adjacent error path uses this same
/// check, so a benign shutdown is classified identically everywhere.
pub fn is_benign_shutdown_cancellation(err: &RuntimeError) -> bool {
    let RuntimeError::Storage(StorageError::Driver { source, .. }) = err else {
        return false;
    };
    source
        .downcast_ref::<tokio::task::JoinError>()
        .is_some_and(tokio::task::JoinError::is_cancelled)
}

/// Append one ADR-103 phase-span event (`PhaseStarted` / `PhaseCompleted` /
/// `PhaseCancelled`), logging and swallowing store/serialize failures.
///
/// Best-effort exactly like every other ADR-094/ADR-103 lifecycle-event
/// emitter in this codebase: telemetry must never interrupt or slow the
/// background phase it observes. `label` identifies the phase's owner in
/// the audit trail (e.g. `"kg.embedder_warm"`): it is a fixed label, not a
/// dispatched verb name, since no dispatch is happening.
pub async fn emit_phase_event<P: serde::Serialize>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    label: &str,
    kind: khive_types::EventKind,
    payload: P,
) {
    // A snapshot-inspection runtime has no durable audit sink by contract.
    // Returning before store resolution is stronger than merely swallowing a
    // rejected append: no writer-bearing EventStore path is entered at all.
    if rt.is_read_only() {
        return;
    }
    // Best-effort exactly like ADR-094's other lifecycle-event emitters: a
    // backend that cannot resolve an `EventStore` for this token's namespace
    // is treated as an unconfigured audit sink, not an error to propagate.
    let Ok(store) = rt.events(token) else {
        return;
    };
    let payload_value = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                event_kind = %kind.name(),
                label,
                "failed to serialize ADR-103 phase-span event payload"
            );
            return;
        }
    };
    let actor = format!("{}:{}", token.actor().kind, token.actor().id);
    let event = Event::new(
        token.namespace().as_str(),
        label,
        kind,
        SubstrateKind::Event,
        actor,
    )
    .with_payload(payload_value);
    if let Err(err) = store.append_event(event).await {
        tracing::warn!(
            error = %err,
            event_kind = %kind.name(),
            label,
            "ADR-103 phase-span event append failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::RuntimeConfig;

    struct MustNotSerialize;

    impl serde::Serialize for MustNotSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            panic!("a read-only phase event must return before payload serialization")
        }
    }

    #[tokio::test]
    async fn read_only_phase_event_returns_before_store_or_payload_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = RuntimeConfig {
            db_path: Some(dir.path().join("read-only-phase-events.db")),
            ..RuntimeConfig::no_embeddings()
        };
        drop(KhiveRuntime::new(config.clone()).expect("migrate snapshot source"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let db_path = config.db_path.as_ref().expect("db path");
            for suffix in ["-wal", "-shm"] {
                let mut name = db_path.file_name().expect("db file name").to_os_string();
                name.push(suffix);
                let sidecar = db_path.parent().expect("db parent dir").join(name);
                if sidecar.exists() {
                    let mut permissions = std::fs::metadata(&sidecar)
                        .expect("sidecar metadata")
                        .permissions();
                    permissions.set_mode(0o444);
                    std::fs::set_permissions(&sidecar, permissions).expect("freeze sidecar");
                }
            }
        }
        let runtime = KhiveRuntime::new_readonly(config).expect("open snapshot read-only");
        let token = runtime
            .authorize(crate::Namespace::local())
            .expect("authorize local");
        let before = runtime.backend().pool().writer_acquisition_snapshot();

        emit_phase_event(
            &runtime,
            &token,
            "read-only.phase-test",
            khive_types::EventKind::PhaseStarted,
            MustNotSerialize,
        )
        .await;

        assert_eq!(
            runtime.backend().pool().writer_acquisition_snapshot(),
            before,
            "read-only phase suppression must not enter the writer plane"
        );
    }
}
