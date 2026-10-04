use super::*;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

const DIMS: usize = 8;

async fn fixture(
    model: &str,
    count: usize,
) -> (TestRuntime, NamespaceToken, SharedAnn, AnnKey, Vec<Uuid>) {
    let rt = test_runtime_with_hash_embedder(model, DIMS);
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut ids = Vec::with_capacity(count);
    for n in 0..count {
        ids.push(
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("maintenance reader seed {n}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .unwrap()
            .id,
        );
    }
    let ann = new_shared();
    *ann.checkpoint_policy.write().unwrap() = CheckpointPolicy {
        max_dirty_ops: u64::MAX,
        interval: Duration::ZERO,
        consolidate_tau: 40_000,
        rebuild_fraction: 0.20,
    };
    let key = AnnKey::new(model);
    ensure_ann_for_model(&rt, &token, &ann, model)
        .await
        .unwrap();
    assert_eq!(ann.indexes.read().await[&key].index.live_count(), count);
    (rt, token, ann, key, ids)
}

async fn update(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    key: &AnnKey,
    id: Uuid,
) {
    rt.update_note_with_embedding_report(
        token,
        id,
        khive_runtime::NotePatch::new(
            None,
            Some("maintenance final replacement vector".into()),
            None,
            None,
            None,
        ),
    )
    .await
    .map(|(row, _report)| row)
    .unwrap();
    bump_generation(ann, key).await;
}

async fn run(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
) -> Result<InstalledMaintenance, RuntimeError> {
    let lock = model_warm_lock(ann, key).await;
    let _guard = lock.lock().await;
    maintain_installed(
        rt,
        ann,
        key,
        &key.model,
        current_generation(ann, key).await,
        durable_epoch(rt).await,
        &mut AnnWarmDetails::default(),
    )
    .await
}

fn assert_same_index(actual: &AnnBridge, expected: &AnnBridge) {
    assert_eq!(actual.id_map, expected.id_map);
    assert_eq!(actual.index.graph(), expected.index.graph());
    assert_eq!(
        actual.index.vectors().unwrap(),
        expected.index.vectors().unwrap()
    );
    assert_eq!(
        actual.index.last_applied_seq(),
        expected.index.last_applied_seq()
    );
    assert_eq!(
        actual.index.tombstone_count(),
        expected.index.tombstone_count()
    );
    assert_eq!(
        actual.index.ops_since_consolidation(),
        expected.index.ops_since_consolidation()
    );
    for ordinal in 0..actual.index.num_vectors() {
        assert_eq!(
            actual.index.is_tombstoned(ordinal as u32),
            expected.index.is_tombstoned(ordinal as u32)
        );
    }
    for query in [
        fnv_to_vec("maintenance reader seed 0", DIMS),
        fnv_to_vec("maintenance final replacement vector", DIMS),
    ] {
        assert_eq!(
            actual.search(&query, 20).unwrap(),
            expected.search(&query, 20).unwrap()
        );
    }
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn reader_acquires_during_reverse_map_tail_replay() {
    const MODEL: &str = "maintenance-off-lock-large-map";
    let (rt, token, ann, key, ids) = fixture(MODEL, 1024).await;
    let mut expected = ann.indexes.read().await[&key].fork_for_maintenance();
    assert!(
        expected.reverse_map.is_none(),
        "mapped adoption leaves the reverse map lazy"
    );
    update(&rt, &token, &ann, &key, ids[0]).await;
    let tail = protected_tail(
        &rt,
        &ann,
        MODEL,
        expected.index.last_applied_seq().unwrap(),
        205,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(tail.raw_count, 1);
    expected
        .apply_final_ops(tail.ops.clone(), tail.applied)
        .unwrap();
    expected.record_delta_batch(tail.ops, tail.applied, tail.raw_count);
    expected.dirty_ops += tail.raw_count;
    expected.namespace_set.clear();
    expected.generation = current_generation(&ann, &key).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let readers = Arc::new(AtomicUsize::new(0));
    let weak = Arc::downgrade(&ann);
    let observed_key = key.clone();
    let observed_calls = Arc::clone(&calls);
    let observed_readers = Arc::clone(&readers);
    let old_seq = bridge_applied_seq(&ann, &key).await.unwrap();
    ann.indexes
        .write()
        .await
        .get_mut(&key)
        .unwrap()
        .reverse_map_scan_hook = Some(Arc::new(move || {
        observed_calls.fetch_add(1, Ordering::SeqCst);
        let shared = weak.upgrade().unwrap();
        if let Ok(indexes) = shared.indexes.try_read() {
            assert_eq!(
                indexes[&observed_key].index.last_applied_seq(),
                Some(old_seq)
            );
            assert_eq!(indexes[&observed_key].index.live_count(), 1024);
            observed_readers.fetch_add(1, Ordering::SeqCst);
        };
    }));
    assert!(matches!(
        run(&rt, &ann, &key).await.unwrap(),
        InstalledMaintenance::Complete
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the witness must run inside the actual reverse-map scan"
    );
    assert_eq!(readers.load(Ordering::SeqCst), 1, "MAINTENANCE_READ_LOCK: a reader must acquire the installed index lock while reverse-map replay is in progress");
    let indexes = ann.indexes.read().await;
    let actual = &indexes[&key];
    assert_same_index(actual, &expected);
    assert_eq!(actual.reverse_map, expected.reverse_map);
    assert_eq!(actual.dirty_ops, expected.dirty_ops);
    assert_eq!(actual.published_seq, expected.published_seq);
    assert_eq!(actual.delta_raw_ops, expected.delta_raw_ops);
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn consolidation_keeps_readers_and_matches_sequential_path() {
    const MODEL: &str = "maintenance-off-lock-consolidation";
    let (rt, token, ann, key, ids) = fixture(MODEL, 128).await;
    let mut expected = ann.indexes.read().await[&key].fork_for_maintenance();
    rt.delete_note(&token, ids[0], true).await.unwrap();
    bump_generation(&ann, &key).await;
    let tail = protected_tail(
        &rt,
        &ann,
        MODEL,
        expected.index.last_applied_seq().unwrap(),
        26,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(tail.raw_count, 1);
    expected.apply_final_ops(tail.ops, tail.applied).unwrap();
    assert!(expected.consolidate_if_needed(1).unwrap());
    let dir = tempfile::tempdir().unwrap();
    expected.save_atomic(dir.path()).unwrap();
    let expected = AnnBridge::load(dir.path()).unwrap();
    ann.checkpoint_policy.write().unwrap().consolidate_tau = 1;
    let reads = Arc::new(AtomicUsize::new(0));
    let observed_reads = Arc::clone(&reads);
    let weak = Arc::downgrade(&ann);
    {
        let mut indexes = ann.indexes.write().await;
        let bridge = indexes.get_mut(&key).unwrap();
        bridge.delta_chunks = delta::MAX_RETIRED_CHAIN;
        bridge.reverse_map_scan_hook = Some(Arc::new(move || {
            let shared = weak.upgrade().unwrap();
            if shared.indexes.try_read().is_ok() {
                observed_reads.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    assert!(matches!(
        run(&rt, &ann, &key).await.unwrap(),
        InstalledMaintenance::Complete
    ));
    assert_eq!(
        reads.load(Ordering::SeqCst),
        2,
        "both initial and consolidated reverse-map scans permit readers"
    );
    assert_same_index(&ann.indexes.read().await[&key], &expected);
}

#[derive(Default)]
struct ScanPause {
    reached: tokio::sync::Notify,
    released: Mutex<bool>,
    condition: Condvar,
}

impl ScanPause {
    fn pause(&self) {
        self.reached.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.condition.wait(released).unwrap();
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.condition.notify_all();
    }
}

struct ReleaseOnDrop(Arc<ScanPause>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

async fn pause_next_scan(ann: &SharedAnn, key: &AnnKey) -> (Arc<ScanPause>, ReleaseOnDrop) {
    let pause = Arc::new(ScanPause::default());
    let hook_pause = Arc::clone(&pause);
    ann.indexes
        .write()
        .await
        .get_mut(key)
        .unwrap()
        .reverse_map_scan_hook = Some(Arc::new(move || hook_pause.pause()));
    (Arc::clone(&pause), ReleaseOnDrop(pause))
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn cancelled_maintenance_after_copy_keeps_incumbent_results() {
    const MODEL: &str = "maintenance-cancelled-copy";
    let (rt, token, ann, key, ids) = fixture(MODEL, 80).await;
    let query = fnv_to_vec("maintenance reader seed 0", DIMS);
    let before = ann.indexes.read().await[&key].search(&query, 20).unwrap();
    let identity = Arc::clone(&ann.indexes.read().await[&key].incarnation);
    update(&rt, &token, &ann, &key, ids[0]).await;
    let (pause, release) = pause_next_scan(&ann, &key).await;
    let (cancel, cancellation) = tokio::sync::watch::channel(false);
    let task_rt = rt.runtime.clone();
    let task_ann = Arc::clone(&ann);
    let task_key = key.clone();
    let task = tokio::spawn(async move {
        khive_storage::scope_request_read_cancellation(
            cancellation,
            run(&task_rt, &task_ann, &task_key),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(30), pause.reached.notified())
        .await
        .expect("actual copied candidate enters reverse-map scan");
    cancel.send(true).unwrap();
    drop(release);
    let result = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(RuntimeError::Storage(StorageError::Timeout { .. }))
    ));
    let indexes = ann.indexes.read().await;
    assert!(Arc::ptr_eq(&indexes[&key].incarnation, &identity));
    assert_eq!(indexes[&key].search(&query, 20).unwrap(), before);
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn failed_maintenance_after_copy_keeps_incumbent_results() {
    const MODEL: &str = "maintenance-failed-copy";
    let (rt, token, ann, key, ids) = fixture(MODEL, 80).await;
    let query = fnv_to_vec("maintenance reader seed 0", DIMS);
    let before = ann.indexes.read().await[&key].search(&query, 20).unwrap();
    let identity = Arc::clone(&ann.indexes.read().await[&key].incarnation);
    update(&rt, &token, &ann, &key, ids[0]).await;
    {
        let mut indexes = ann.indexes.write().await;
        let bridge = indexes.get_mut(&key).unwrap();
        let wrong = bridge.id_map.iter().position(|id| *id != ids[0]).unwrap();
        bridge.reverse_map = Some(HashMap::from([(ids[0], wrong as u32)]));
    }
    assert!(matches!(
        run(&rt, &ann, &key).await.unwrap(),
        InstalledMaintenance::Rebuild
    ));
    let indexes = ann.indexes.read().await;
    assert!(Arc::ptr_eq(&indexes[&key].incarnation, &identity));
    assert_eq!(indexes[&key].search(&query, 20).unwrap(), before);
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn maintenance_does_not_resurrect_evicted_or_replaced_bridge() {
    for replace in [false, true] {
        let model = format!("maintenance-fence-{replace}");
        let (rt, token, ann, key, ids) = fixture(&model, 80).await;
        update(&rt, &token, &ann, &key, ids[0]).await;
        let (pause, release) = pause_next_scan(&ann, &key).await;
        let task_rt = rt.runtime.clone();
        let task_ann = Arc::clone(&ann);
        let task_key = key.clone();
        let task = tokio::spawn(async move { run(&task_rt, &task_ann, &task_key).await });
        tokio::time::timeout(Duration::from_secs(30), pause.reached.notified())
            .await
            .expect("candidate must enter actual scan");
        let replacement_identity = if replace {
            let replacement = ann.indexes.read().await[&key].fork_for_maintenance();
            let identity = Arc::clone(&replacement.incarnation);
            assert!(install_replacing(&ann, &key, replacement).await);
            Some(identity)
        } else {
            ann.indexes.write().await.remove(&key);
            None
        };
        drop(release);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            InstalledMaintenance::Absent
        ));
        let indexes = ann.indexes.read().await;
        if let Some(identity) = replacement_identity {
            assert!(
                Arc::ptr_eq(&indexes[&key].incarnation, &identity),
                "same-progress replacement must survive the older candidate"
            );
        } else {
            assert!(
                !indexes.contains_key(&key),
                "eviction must not be undone by a completed candidate"
            );
        }
    }
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn empty_tail_refresh_keeps_incumbent_without_fork() {
    const MODEL: &str = "maintenance-empty-tail-no-copy";
    let (rt, _token, ann, key, _ids) = fixture(MODEL, 80).await;
    let query = fnv_to_vec("maintenance reader seed 0", DIMS);
    let (identity, applied, before) = {
        let indexes = ann.indexes.read().await;
        let bridge = &indexes[&key];
        (
            Arc::clone(&bridge.incarnation),
            bridge.index.last_applied_seq(),
            bridge.search(&query, 20).unwrap(),
        )
    };
    bump_generation(&ann, &key).await;
    assert!(matches!(
        run(&rt, &ann, &key).await.unwrap(),
        InstalledMaintenance::Complete
    ));
    let indexes = ann.indexes.read().await;
    let bridge = &indexes[&key];
    assert!(
        Arc::ptr_eq(&bridge.incarnation, &identity),
        "empty-tail refresh must not fork and swap the corpus"
    );
    assert_eq!(bridge.index.last_applied_seq(), applied);
    assert_eq!(bridge.search(&query, 20).unwrap(), before);
    assert!(bridge.reverse_map.is_none());
    assert_eq!(bridge.generation, current_generation(&ann, &key).await);
}
