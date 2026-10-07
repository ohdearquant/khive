use super::*;
use crate::disk_guard::{observe_close_with_lease, VolumeIdentity};
use std::sync::atomic::Ordering;

#[test]
fn terminal_and_unwinding_writer_close_before_volume_lease_release() {
    for unwind in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("writer-poison.db");
        let locks = fixture.path().join("locks");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(include_str!("../tests/fixtures/disk-admission.sql"))
            .unwrap();
        let identity = VolumeIdentity::resolve(&path).unwrap();
        let admission = Arc::new(
            crate::pool::WriteAdmission::new(Some(path.clone()), 0, 100, Some(locks.clone()))
                .unwrap(),
        );
        let mut owned = BlockingWriterConnection::new(conn, admission);
        owned.volume_lease = Some(
            identity
                .acquire(Duration::from_millis(2000), Some(&locks))
                .unwrap(),
        );
        let closed = observe_close_with_lease(&owned, locks.join(identity.lock_filename()));
        owned
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO admission_payload VALUES (1)")
            .unwrap();
        owned
            .authorizer(Some(
                |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                    rusqlite::hooks::AuthAction::Transaction {
                        operation: rusqlite::hooks::TransactionOperation::Rollback,
                    } => rusqlite::hooks::Authorization::Deny,
                    _ => rusqlite::hooks::Authorization::Allow,
                },
            ))
            .unwrap();
        assert!(owned.execute_batch("ROLLBACK").is_err());
        if unwind {
            assert!(catch_unwind(AssertUnwindSafe(move || {
                let _owned = owned;
                panic!("force blocking-owner unwind");
            }))
            .is_err());
        } else {
            let (returned, state) = owned.finish(Some(WriterTaskRequestState::SideEffectsUnknown));
            assert!(returned.is_none());
            assert_eq!(state, Some(WriterTaskRequestState::SideEffectsUnknown));
        }
        assert_eq!(
            closed.load(Ordering::SeqCst),
            1,
            "connection closes with its OS lease still held, unwind={unwind}"
        );
        let _competitor = identity
            .acquire(Duration::from_millis(2000), Some(&locks))
            .unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM admission_payload", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
        conn.execute_batch("COMMIT").unwrap();
    }
}
