use super::*;
use crate::pool::{ConnectionPool, PoolConfig};
use std::fs::File;
use std::sync::atomic::AtomicBool;
use tokio::sync::Notify;

const WATCHDOG: Duration = Duration::from_secs(10);

fn isolated_sink_environment() -> bool {
    crate::test_process::run_in_child(|command| {
        command
            .env(crate::pool::TEST_HARNESS_ENV, "1")
            .env_remove(super::super::SINK_DIR_OVERRIDE_ENV)
            .env_remove(super::super::STARTUP_BARRIER_DIR_ENV)
            .env_remove(super::super::WRITE_DELAY_MS_OVERRIDE_ENV);
    })
}

fn pool(path: PathBuf) -> ConnectionPool {
    ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        ..PoolConfig::default()
    })
    .unwrap()
}

#[tokio::test]
async fn two_pools_share_sink_until_last_owner_drops_and_queue_drains() {
    if isolated_sink_environment() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let first = pool(temp.path().join("first.db"));
    let second = pool(temp.path().join("second.db"));
    let first_db = super::super::db_label(&first);
    let second_db = super::super::db_label(&second);
    let close = first.timeout_sink_close_handle().unwrap();
    let other_close = second.timeout_sink_close_handle().unwrap();
    assert!(Arc::ptr_eq(&close.entry, &other_close.entry));
    drop(first);
    assert_eq!(
        close.close().await,
        Err(TimeoutSinkCloseError::Busy {
            pools: 1,
            writers: 0
        })
    );
    super::super::emit_timeout(&first_db, super::super::Site::PoolAdmission, "first", None);
    super::super::emit_timeout(
        &second_db,
        super::super::Site::PoolAdmission,
        "second",
        None,
    );
    drop(second);
    tokio::time::timeout(WATCHDOG, close.close())
        .await
        .unwrap()
        .unwrap();
    assert!(close.entry.state.lock().join.is_none());
    assert_eq!(other_close.close().await, Ok(()));
    let path = temp
        .path()
        .join(super::super::FALLBACK_LOG_SUBDIR)
        .join(super::super::ndjson_file_name());
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (db, error) in [(&first_db, "first"), (&second_db, "second")] {
        assert_eq!(
            rows.iter()
                .filter(|row| row["kind"] == "timeout"
                    && row["db"] == db.as_str()
                    && row["error"] == error)
                .count(),
            1
        );
    }
    assert!(route(&first_db).is_none());
    assert!(route(&second_db).is_none());
}

async fn held_native_writer(abort_parent: bool) {
    let temp = tempfile::tempdir().unwrap();
    let first = pool(temp.path().join("writer.db"));
    let second = pool(temp.path().join("observer.db"));
    let writer = first.writer_task_handle().unwrap().unwrap();
    let mut writer_join = first.take_writer_task_join().unwrap();
    let close = second.timeout_sink_close_handle().unwrap();
    let started = Arc::new(Notify::new());
    let entered = started.clone();
    let (release, gate) = mpsc::sync_channel(1);
    let submission = tokio::spawn(async move {
        writer
            .send(move |connection| {
                entered.notify_one();
                gate.recv_timeout(WATCHDOG)
                    .expect("native writer gate released");
                connection
                    .execute_batch("CREATE TABLE sink_lifecycle_test(value INTEGER)")
                    .map_err(|error| {
                        khive_storage::StorageError::driver(
                            khive_storage::StorageCapability::Sql,
                            "sink_lifecycle_test",
                            error,
                        )
                    })?;
                Ok(())
            })
            .await
    });
    tokio::time::timeout(WATCHDOG, started.notified())
        .await
        .unwrap();
    // The first pool never requested a close handle. Its ordinary Drop must
    // still leave the route owned by the accepted native write.
    drop(first);
    drop(second);
    assert_eq!(
        close.close().await,
        Err(TimeoutSinkCloseError::Busy {
            pools: 0,
            writers: 1
        })
    );
    if abort_parent {
        writer_join.abort();
        assert!(tokio::time::timeout(WATCHDOG, &mut writer_join)
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled());
        assert_eq!(
            close.close().await,
            Err(TimeoutSinkCloseError::Busy {
                pools: 0,
                writers: 1
            })
        );
    }
    release.send(()).unwrap();
    tokio::time::timeout(WATCHDOG, submission)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    if !abort_parent {
        tokio::time::timeout(WATCHDOG, writer_join)
            .await
            .unwrap()
            .unwrap();
    }
    tokio::time::timeout(WATCHDOG, async {
        loop {
            match close.close().await {
                Err(TimeoutSinkCloseError::Busy {
                    pools: 0,
                    writers: 1,
                }) => tokio::task::yield_now().await,
                result => {
                    result.unwrap();
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ordinary_pool_drop_keeps_sink_alive_until_native_writer_settles() {
    if isolated_sink_environment() {
        return;
    }
    held_native_writer(false).await;
}

#[tokio::test]
async fn aborted_parent_keeps_sink_alive_until_native_connection_drops() {
    if isolated_sink_environment() {
        return;
    }
    held_native_writer(true).await;
}

struct HeldFile {
    file: Option<File>,
    dropped: Arc<AtomicBool>,
}

impl Drop for HeldFile {
    fn drop(&mut self) {
        drop(self.file.take());
        self.dropped.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn cancelled_close_resumes_join_after_file_release_and_refuses_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let registry = Arc::new(Mutex::new(Registry::default()));
    let directory = temp.path().to_path_buf();
    let (sender, receiver) = mpsc::sync_channel(1);
    let (release, gate) = mpsc::sync_channel(1);
    let closing = Arc::new(Notify::new());
    let observed = closing.clone();
    let dropped = Arc::new(AtomicBool::new(false));
    let resource = HeldFile {
        file: Some(File::create(temp.path().join("held-file")).unwrap()),
        dropped: dropped.clone(),
    };
    let join = thread::spawn(move || {
        let _resource = resource;
        while receiver.recv().is_ok() {}
        observed.notify_one();
        gate.recv_timeout(WATCHDOG)
            .expect("file release gate opened");
    });
    let handle = Arc::new(SinkHandle {
        sender,
        dropped: Arc::new(AtomicU64::new(0)),
    });
    let entry = Arc::new(Entry {
        directory: directory.clone(),
        state: Mutex::new(State {
            pools: 0,
            writers: 0,
            handle: Some(handle.clone()),
            join: Some(join),
            result: None,
        }),
    });
    registry
        .lock()
        .directories
        .insert(directory.clone(), entry.clone());
    registry.lock().routes.insert("controlled".into(), handle);
    let close = TimeoutSinkCloseHandle {
        registry: registry.clone(),
        entry,
    };
    {
        let wait = close.close();
        tokio::pin!(wait);
        tokio::time::timeout(WATCHDOG, async {
            tokio::select! {
                biased;
                result = &mut wait => panic!("close returned before file release: {result:?}"),
                () = closing.notified() => {}
            }
        })
        .await
        .unwrap();
        assert!(!dropped.load(Ordering::Acquire));
        // Drop this pending future; the retained join must survive cancellation.
    }
    assert!(close.entry.state.lock().join.is_some());
    assert!(close.entry.state.lock().handle.is_none());
    assert!(matches!(init_in(&registry, directory.clone(), "reopen"),
        Err(SqliteError::InvalidConfig(message)) if message.contains("closing")));
    release.send(()).unwrap();
    tokio::time::timeout(WATCHDOG, close.close())
        .await
        .unwrap()
        .unwrap();
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(close.close().await, Ok(()));
    let reopened = init_in(&registry, directory, "reopen").unwrap().unwrap();
    let reopened_close = reopened.close_handle();
    assert!(!Arc::ptr_eq(&close.entry, &reopened_close.entry));
    drop(reopened);
    tokio::time::timeout(WATCHDOG, reopened_close.close())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn registry_contention_drops_without_wait_and_transfers_loss_count() {
    let temp = tempfile::tempdir().unwrap();
    let registry = Arc::new(Mutex::new(Registry::default()));
    let registration = init_in(&registry, temp.path().into(), "test-db")
        .unwrap()
        .unwrap();
    let pending = Arc::new(AtomicU64::new(0));
    let held = registry.lock();
    let worker_registry = registry.clone();
    let worker_pending = pending.clone();
    let (sent, received) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        sent.send(route_in(&worker_registry, &worker_pending, "test-db").is_none())
            .unwrap();
    });
    let routed = received.recv_timeout(WATCHDOG);
    drop(held);
    worker.join().unwrap();
    assert!(routed.expect("producer routing must not wait for the registry"));
    assert_eq!(pending.load(Ordering::Relaxed), 1);
    let handle = route_in(&registry, &pending, "test-db").unwrap();
    assert_eq!(handle.dropped.load(Ordering::Relaxed), 1);
    assert_eq!(pending.load(Ordering::Relaxed), 0);
    let close = registration.close_handle();
    drop(handle);
    drop(registration);
    tokio::time::timeout(WATCHDOG, close.close())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn different_directories_do_not_share_a_sink_or_its_close() {
    let temp = tempfile::tempdir().unwrap();
    let registry = Arc::new(Mutex::new(Registry::default()));
    let first = init_in(&registry, temp.path().join("a"), "a")
        .unwrap()
        .unwrap();
    let second = init_in(&registry, temp.path().join("b"), "b")
        .unwrap()
        .unwrap();
    let close_first = first.close_handle();
    let close_second = second.close_handle();
    assert!(!Arc::ptr_eq(&close_first.entry, &close_second.entry));
    drop(first);
    tokio::time::timeout(WATCHDOG, close_first.close())
        .await
        .unwrap()
        .unwrap();
    assert!(route_in(&registry, &AtomicU64::new(0), "b").is_some());
    assert_eq!(
        close_second.close().await,
        Err(TimeoutSinkCloseError::Busy {
            pools: 1,
            writers: 0
        })
    );
    drop(second);
    tokio::time::timeout(WATCHDOG, close_second.close())
        .await
        .unwrap()
        .unwrap();
}
