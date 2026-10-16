use super::*;
use khive_runtime::{Namespace, PackRuntime, VerbRegistryBuilder};
use serde_json::json;

#[test]
fn hook_queue_worker_status_tracks_election_and_drain() {
    let queue = HookQueue::new();
    assert!(!queue.worker_running());
    assert!(queue.enqueue("local".to_string(), BrainSignal::Irrelevant));
    assert!(queue.worker_running());
    assert_eq!(queue.take_batch_or_stop().unwrap().len(), 1);
    assert!(
        queue.worker_running(),
        "worker remains elected until empty check"
    );
    assert!(queue.take_batch_or_stop().is_none());
    assert!(!queue.worker_running());
}

#[tokio::test]
async fn hook_queue_caps_pending_signals_and_reports_drops() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = BrainPack::new(runtime);
    for index in 0..(MAX_DEFERRED_HOOK_SIGNALS + 3) {
        brain
            .hook_queue
            .enqueue(format!("namespace-{index}"), BrainSignal::Irrelevant);
    }

    let state = brain.handle_state(json!({})).await.expect("brain state");
    assert_eq!(
        state["dispatch_counters"]["contended_hook_signals_dropped"],
        json!(3)
    );
    let pending = brain
        .hook_queue
        .take_batch_or_stop()
        .expect("pending batch");
    assert_eq!(pending.len(), MAX_DEFERRED_HOOK_SIGNALS);
    assert_eq!(pending.first().unwrap().0, "namespace-3");
    assert_eq!(
        pending.last().unwrap().0,
        format!("namespace-{}", MAX_DEFERRED_HOOK_SIGNALS + 2)
    );
}

#[tokio::test]
async fn brain_dispatch_applies_queued_cold_signal_before_handler_reads_state() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = BrainPack::new(runtime.clone());
    // Model a contended hook after worker election but before the worker
    // acquires the dispatch gate. The brain dispatch wins that gate.
    assert!(brain
        .hook_queue
        .enqueue("local".into(), BrainSignal::RecallMiss));

    let registry = VerbRegistryBuilder::new()
        .build()
        .expect("minimal registry");
    let token = runtime.authorize(Namespace::local()).expect("local token");
    brain
        .dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .expect("promote local namespace");

    assert_eq!(brain.snapshot().balanced_recall.total_events, 1);
    assert!(brain.hook_queue.drain_pending().is_empty());
}
