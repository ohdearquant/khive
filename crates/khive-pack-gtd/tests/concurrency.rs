//! Concurrency and race-condition tests for GTD lifecycle operations.

mod common;

use common::{assign, pack, rt};
use khive_runtime::RuntimeError;
use serde_json::{json, Value};

#[tokio::test]
async fn dsl_parallel_c2_concurrent_complete_one_wins_one_loses() {
    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "concurrent-race", "status": "next"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();
    let id2 = id.clone();

    let pack_a = std::sync::Arc::new(fixture);
    let pack_b = pack_a.clone();

    let (res_a, res_b) = tokio::join!(
        pack_a.dispatch("gtd.complete", json!({"id": id, "result": "op-A"})),
        pack_b.dispatch("gtd.complete", json!({"id": id2, "result": "op-B"})),
    );

    let successes = [res_a.is_ok(), res_b.is_ok()]
        .iter()
        .filter(|&&ok| ok)
        .count();
    let failures = [res_a.is_err(), res_b.is_err()]
        .iter()
        .filter(|&&e| e)
        .count();

    assert_eq!(
        successes, 1,
        "dsl-parallel C2: exactly one concurrent complete() must succeed; got {successes} successes"
    );
    assert_eq!(
        failures, 1,
        "dsl-parallel C2: exactly one concurrent complete() must fail; got {failures} failures"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_complete_two_threads_one_wins_one_loses_atomic() {
    use std::sync::Arc;

    let runtime = rt();
    let fixture = Arc::new(pack(runtime));

    let resp = assign(&fixture, json!({"title": "mt-race-task", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let (tx_a, rx_a) = tokio::sync::oneshot::channel::<Result<Value, RuntimeError>>();
    let (tx_b, rx_b) = tokio::sync::oneshot::channel::<Result<Value, RuntimeError>>();

    let pack_a = fixture.clone();
    let pack_b = fixture.clone();
    let bar_a = barrier.clone();
    let bar_b = barrier.clone();
    let id_a = id.clone();
    let id_b = id.clone();

    tokio::spawn(async move {
        bar_a.wait().await;
        let res = pack_a
            .dispatch("gtd.complete", json!({"id": id_a, "result": "thread-A"}))
            .await;
        let _ = tx_a.send(res);
    });

    tokio::spawn(async move {
        bar_b.wait().await;
        let res = pack_b
            .dispatch("gtd.complete", json!({"id": id_b, "result": "thread-B"}))
            .await;
        let _ = tx_b.send(res);
    });

    let res_a = rx_a.await.expect("thread A result");
    let res_b = rx_b.await.expect("thread B result");

    let successes = [res_a.is_ok(), res_b.is_ok()]
        .iter()
        .filter(|&&ok| ok)
        .count();
    let failures = [res_a.is_err(), res_b.is_err()]
        .iter()
        .filter(|&&e| e)
        .count();

    assert_eq!(
        successes, 1,
        "exactly one complete() must succeed in concurrent race; got {successes} successes"
    );
    assert_eq!(
        failures, 1,
        "exactly one complete() must fail in concurrent race; got {failures} failures"
    );

    let loser_err = match (res_a, res_b) {
        (Err(e), _) => e,
        (_, Err(e)) => e,
        _ => panic!("expected exactly one failure; both succeeded"),
    };
    let msg = loser_err.to_string();
    assert!(
        msg.contains("terminal state") || msg.contains("rows_affected"),
        "losing complete() must fail with terminal-state or rows_affected conflict; got: {msg}"
    );
}
