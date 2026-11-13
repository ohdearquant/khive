use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::disk_guard::{LeaseClass, WriteClassContext};
use crate::pool::{ConnectionPool, PoolConfig};
use crate::writer_admission_metrics::long_lease_holds;

// With the writer queue off, an attachment write takes its lease on a blocking
// thread; the issuer's class reaches that thread, and an unscoped write keeps
// the direct route's own class.
#[tokio::test]
async fn a_direct_attachment_write_records_its_lease_under_the_issuers_class() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("attachment_lease_class.db")),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    let store = SqlAttachmentStore::new(Arc::clone(&pool), true);
    let hold = |_conn: &rusqlite::Connection| {
        std::thread::sleep(Duration::from_millis(600));
        Ok::<(), rusqlite::Error>(())
    };

    let before = long_lease_holds(LeaseClass::Migration);
    WriteClassContext::migration()
        .scope_future(store.with_writer("lease_class_test", hold))
        .await
        .unwrap();
    assert_eq!(
        long_lease_holds(LeaseClass::Migration),
        before + 1,
        "a direct attachment write issued under a migration context is recorded as migration"
    );

    let before = long_lease_holds(LeaseClass::Pooled);
    store.with_writer("lease_class_test", hold).await.unwrap();
    assert_eq!(
        long_lease_holds(LeaseClass::Pooled),
        before + 1,
        "an unscoped direct attachment write keeps the pooled class"
    );
}
