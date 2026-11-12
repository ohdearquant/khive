use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::disk_guard::{LeaseClass, WriteClassContext};
use crate::pool::{ConnectionPool, PoolConfig};

/// Holds of at least half a second recorded under `class` so far; see
/// `pool_identity_admission_tests::long_holds`.
fn long_holds(class: LeaseClass) -> u64 {
    let holds = crate::disk_guard::lease_hold_snapshot().expect("lease holds available");
    let (_, histogram) = holds
        .iter()
        .find(|(recorded, _)| *recorded == class)
        .expect("every class is reported");
    histogram.buckets[11..].iter().sum::<u64>() + histogram.overflow_count
}

// With the writer queue off, an attachment write takes its lease on a blocking
// thread; the issuer's class reaches that thread, and an unscoped write keeps
// the direct route's own class.
#[tokio::test]
async fn a_direct_attachment_write_records_its_lease_under_the_issuers_class() {
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

    let before = long_holds(LeaseClass::Migration);
    WriteClassContext::migration()
        .scope_future(store.with_writer("lease_class_test", hold))
        .await
        .unwrap();
    assert!(
        long_holds(LeaseClass::Migration) > before,
        "a direct attachment write issued under a migration context is recorded as migration"
    );

    let before = long_holds(LeaseClass::Pooled);
    store.with_writer("lease_class_test", hold).await.unwrap();
    assert!(
        long_holds(LeaseClass::Pooled) > before,
        "an unscoped direct attachment write keeps the pooled class"
    );
}
