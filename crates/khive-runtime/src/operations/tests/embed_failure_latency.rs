use super::*;

#[test]
fn create_entity_embed_failure_returns_under_single_worker_saturation() {
    let (blocking, controls) = BlockingVecProvider::new("latency-blocking", 4);
    let fail_after_entry =
        FailFastProvider::after_signal("latency-fail-after-entry", Arc::clone(&controls.entered));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);

    let runtime_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("single-worker runtime must build");
        let rt = KhiveRuntime::memory().unwrap();
        rt.register_embedder(blocking);
        rt.register_embedder(fail_after_entry);
        let tok = NamespaceToken::for_namespace(Namespace::parse("embed-failure-latency").unwrap());
        let result = runtime.block_on(rt.create_entity(
            &tok,
            "concept",
            None,
            "blocked sibling entity",
            None,
            None,
            vec![],
        ));
        result_tx
            .send(result.map_err(|error| error.to_string()))
            .expect("test receiver must remain connected");
    });

    // Runtime/store/provider setup has its own generous bound. The three-second
    // failure-return bound starts only after synchronous sibling inference enters.
    let setup_started = std::time::Instant::now();
    while !controls.entered.load(Ordering::Acquire)
        && setup_started.elapsed() < std::time::Duration::from_secs(60)
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let entered = controls.entered.load(Ordering::Acquire);
    let result = entered.then(|| result_rx.recv_timeout(std::time::Duration::from_secs(3)));

    let (released, wake) = &*controls.release;
    *released.lock().expect("release lock must not be poisoned") = true;
    wake.notify_all();
    runtime_thread
        .join()
        .expect("single-worker runtime thread must join after release");

    assert!(
        entered,
        "synchronous sibling must enter after runtime setup"
    );
    let error = result
        .expect("entered sibling starts the failure-return wait")
        .expect("embed failure must return while synchronous inference remains blocked")
        .expect_err("one failed model must fail entity creation");
    assert!(error.contains("injected embed failure"));
}
