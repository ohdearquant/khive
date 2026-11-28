struct RecordPause {
    entered: std::sync::mpsc::Receiver<()>,
    release: Option<std::sync::mpsc::SyncSender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl RecordPause {
    fn start(
        runtime: KhiveRuntime,
        key: &'static str,
        point: crate::config_ledger::TestRecordPoint,
    ) -> Self {
        let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = Mutex::new(release_rx);
        runtime.install_config_lock_test_hook(
            point,
            Arc::new(move || {
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(WATCHDOG)
                    .expect("boundary released");
            }),
        );
        let worker =
            std::thread::spawn(move || runtime.record_config_locked(key, || "locked".into()));
        Self {
            entered,
            release: Some(release),
            worker: Some(worker),
        }
    }

    fn await_boundary(&self) {
        self.entered
            .recv_timeout(WATCHDOG)
            .expect("actual ledger boundary reached");
    }

    fn resume(mut self) {
        self.release.take().unwrap().send(()).unwrap();
        self.worker.take().unwrap().join().expect("record worker");
    }
}

impl Drop for RecordPause {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[tokio::test]
#[serial(config_ledger)]
async fn observe_paused_before_mutex_survives_an_intervening_empty_drain() {
    let f = Fixture::new();
    let pause = RecordPause::start(
        f.runtime.clone(),
        "race.before_lock",
        crate::config_ledger::TestRecordPoint::BeforeLock,
    );
    pause.await_boundary();
    f.dispatch().await;
    assert!(
        f.rows().await.is_empty(),
        "observation has not entered its state transition"
    );
    pause.resume();
    f.dispatch().await;
    f.assert_rows(&[("race.before_lock", "locked")]).await;
    f.runtime
        .record_config_locked("race.before_lock", || panic!("seen name"));
    f.dispatch().await;
    f.assert_rows(&[("race.before_lock", "locked")]).await;
    f.finish().await;
}

#[tokio::test]
#[serial(config_ledger)]
async fn a_new_name_observed_after_take_survives_the_earlier_append() {
    let f = Fixture::new();
    f.runtime
        .record_config_locked("race.taken", || "old".into());
    let gate = f.store.gate_next();
    let registry = f.registry.clone();
    let first = tokio::spawn(async move {
        dispatch_ok(&registry, "local", "first").await;
    });
    f.store.await_gate().await;
    f.runtime
        .record_config_locked("race.later", || "new".into());
    drop(gate);
    tokio::time::timeout(WATCHDOG, first)
        .await
        .unwrap()
        .unwrap();
    f.dispatch().await;
    f.assert_rows(&[("race.taken", "old"), ("race.later", "new")])
        .await;
    f.runtime
        .record_config_locked("race.later", || panic!("later name remains seen"));
    f.dispatch().await;
    assert_eq!(f.rows().await.len(), 2);
    f.finish().await;
}

#[tokio::test]
#[serial(config_ledger)]
async fn same_name_observation_after_the_first_mutex_release_never_duplicates() {
    let f = Fixture::new();
    let pause = RecordPause::start(
        f.runtime.clone(),
        "race.after_unlock",
        crate::config_ledger::TestRecordPoint::AfterUnlock,
    );
    pause.await_boundary();
    f.runtime
        .record_config_locked("race.after_unlock", || "locked".into());
    pause.resume();
    f.dispatch().await;
    f.assert_rows(&[("race.after_unlock", "locked")]).await;
    f.finish().await;
}

#[tokio::test]
#[serial(config_ledger)]
async fn a_blocked_runtime_append_does_not_consume_another_runtime_pending_entry() {
    let a = Fixture::new();
    let b = Fixture::new();
    a.runtime
        .record_config_locked("interleave.a", || "a".into());
    b.runtime
        .record_config_locked("interleave.b", || "b".into());
    let gate = a.store.gate_next();
    let registry = a.registry.clone();
    let a_dispatch = tokio::spawn(async move {
        dispatch_ok(&registry, "local", "a").await;
    });
    a.store.await_gate().await;
    b.dispatch().await;
    b.assert_rows(&[("interleave.b", "b")]).await;
    drop(gate);
    tokio::time::timeout(WATCHDOG, a_dispatch)
        .await
        .unwrap()
        .unwrap();
    a.assert_rows(&[("interleave.a", "a")]).await;
    a.finish().await;
    b.finish().await;
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
async fn admitted_config_timeout_commits_the_original_row_without_a_second_submission() {
    let config = AuditBatchConfig {
        admission_deadline: Duration::from_millis(100),
        resolution_deadline: Duration::from_secs(30),
        ..audit_config()
    };
    let f = Fixture::configured(
        KhiveRuntime::memory().unwrap(),
        "local",
        "timeout",
        config,
        |_| {},
    );
    f.runtime
        .record_config_locked("timeout.config", || "original".into());
    let gate = f.store.gate_next();
    let registry = f.registry.clone();
    let store = f.store.clone();
    let dispatch = tokio::spawn(async move {
        capture_timeout(store, dispatch_ok(&registry, "local", "timeout")).await;
    });
    f.store.await_gate().await;
    tokio::time::timeout(WATCHDOG, f.store.timeout_seen.notified())
        .await
        .expect("actual admission-expiry warning");
    assert!(f.store.timed_out.load(Ordering::SeqCst));
    assert_eq!(f.store.submitted("timeout.config").len(), 1);
    // The successful dispatch's own obligation can wait behind this held
    // generation. Release only after expiry is observed, before awaiting it.
    drop(gate);
    tokio::time::timeout(WATCHDOG, dispatch)
        .await
        .unwrap()
        .unwrap();
    f.runtime.record_config_locked("timeout.config", || {
        panic!("timed-out taken name stays seen")
    });
    f.dispatch().await;
    f.finish().await;
    f.assert_rows(&[("timeout.config", "original")]).await;
    let submitted = f.store.submitted("timeout.config");
    let attempts = f.store.attempts("timeout.config");
    assert_eq!(
        submitted.len(),
        1,
        "no second logical submission after timeout"
    );
    assert_eq!(
        attempts.len(),
        1,
        "the original admitted append finishes once"
    );
    assert_eq!(attempts[0].id, submitted[0].id);
    assert_eq!(
        f.rows().await[0].id,
        submitted[0].id,
        "persisted row is the original event"
    );
}
