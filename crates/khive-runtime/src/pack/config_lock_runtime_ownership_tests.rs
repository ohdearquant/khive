#[tokio::test]
#[serial(config_ledger)]
async fn separate_runtimes_keep_destinations_identity_and_first_values() {
    let a = Fixture::from_runtime(KhiveRuntime::memory().unwrap(), "config-owner-a", "owner-a");
    let b = Fixture::from_runtime(KhiveRuntime::memory().unwrap(), "config-owner-b", "owner-b");
    a.runtime
        .record_config_locked("ownership.k", || "a-first".into());
    a.runtime
        .record_config_locked("ownership.k", || panic!("repeat must not format"));
    b.runtime
        .record_config_locked("ownership.k", || "b-first".into());
    a.dispatch().await;
    assert!(
        b.rows().await.is_empty(),
        "A must not route or consume B's pending row"
    );
    a.assert_rows(&[("ownership.k", "a-first")]).await;
    b.dispatch().await;
    b.assert_rows(&[("ownership.k", "b-first")]).await;
    for (fixture, namespace, actor) in [
        (&a, "config-owner-a", "actor:owner-a"),
        (&b, "config-owner-b", "actor:owner-b"),
    ] {
        let rows = fixture.rows().await;
        assert_eq!(rows[0].namespace, namespace);
        assert_eq!(rows[0].actor, actor);
        fixture
            .runtime
            .record_config_locked("ownership.k", || panic!("taken names remain seen"));
        fixture.dispatch().await;
        assert_eq!(fixture.rows().await.len(), 1);
    }
    a.runtime
        .record_config_locked("ownership.l", || "later".into());
    a.dispatch().await;
    a.assert_rows(&[("ownership.k", "a-first"), ("ownership.l", "later")])
        .await;
    let m = Fixture::new();
    m.runtime
        .record_config_locked("ownership.k", || "fresh-m".into());
    m.dispatch().await;
    m.assert_rows(&[("ownership.k", "fresh-m")]).await;
    a.finish().await;
    b.finish().await;
    m.finish().await;
}

#[tokio::test]
#[serial(config_ledger)]
async fn independent_same_backend_runtimes_keep_both_orders_and_pending_unbound_owner() {
    for e_first in [false, true] {
        let backend = prepared_backend();
        let d = Fixture::from_runtime(runtime_on(backend.clone(), "d"), "local", "same-backend");
        let e = Fixture::from_runtime(runtime_on(backend, "e"), "local", "same-backend");
        if e_first {
            e.runtime.record_config_locked("shared.k", || "e".into());
            d.runtime.record_config_locked("shared.k", || "d".into());
        } else {
            d.runtime.record_config_locked("shared.k", || "d".into());
            e.runtime.record_config_locked("shared.k", || "e".into());
        }
        d.dispatch().await;
        d.assert_rows(&[("shared.k", "d")]).await;
        d.dispatch().await;
        assert_eq!(
            d.rows().await.len(),
            1,
            "a second D drain cannot steal E's pending entry"
        );
        e.dispatch().await;
        let rows = e.rows().await;
        assert!(rows
            .iter()
            .all(|row| row.namespace == "local" && row.actor == "actor:same-backend"));
        assert_eq!(
            rows.len(),
            2,
            "independent constructors each submit the same name"
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.payload["value"].as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["d", "e"])
        );
        assert_eq!(d.store.submitted("shared.k").len(), 1);
        assert_eq!(e.store.submitted("shared.k").len(), 1);
        d.finish().await;
        e.finish().await;
    }
    let backend = prepared_backend();
    let d = Fixture::from_runtime(
        runtime_on(backend.clone(), "d2"),
        "local",
        "same-backend-unbound",
    );
    let e_runtime = runtime_on(backend, "e2");
    e_runtime.record_config_locked("shared.unbound", || "e-only".into());
    d.dispatch().await;
    assert!(
        d.rows().await.is_empty(),
        "D cannot consume an unbound E runtime's entry"
    );
    let e = Fixture::from_runtime(e_runtime, "local", "same-backend-unbound");
    e.dispatch().await;
    e.assert_rows(&[("shared.unbound", "e-only")]).await;
    assert!(e
        .rows()
        .await
        .iter()
        .all(|row| row.namespace == "local" && row.actor == "actor:same-backend-unbound"));
    d.finish().await;
    e.finish().await;
}

#[tokio::test]
#[serial(config_ledger)]
async fn six_fresh_names_survive_one_take_and_a_second_take_is_empty() {
    const KEYS: [&str; 6] = ["six.a", "six.b", "six.c", "six.d", "six.e", "six.f"];
    let f = Fixture::new();
    for key in KEYS {
        f.runtime.record_config_locked(key, || key.to_string());
    }
    f.dispatch().await;
    let expected: Vec<_> = KEYS.into_iter().map(|key| (key, key)).collect();
    f.assert_rows(&expected).await;
    for key in KEYS {
        f.runtime.record_config_locked(key, || key.to_string());
    }
    f.dispatch().await;
    f.assert_rows(&expected).await;
    f.finish().await;
}

#[tokio::test]
#[serial(config_ledger)]
async fn an_entry_aged_at_least_one_second_is_not_evicted() {
    let f = Fixture::new();
    f.runtime
        .record_config_locked("aged.entry", || "kept".into());
    // This is elapsed-age coverage, never an operation-ordering mechanism.
    tokio::time::sleep(Duration::from_secs(1)).await;
    f.dispatch().await;
    f.assert_rows(&[("aged.entry", "kept")]).await;
    f.runtime
        .record_config_locked("aged.entry", || "kept".into());
    f.dispatch().await;
    f.assert_rows(&[("aged.entry", "kept")]).await;
    f.finish().await;
}

#[tokio::test]
#[serial(config_ledger)]
async fn clones_unbound_and_explicit_sinks_preserve_the_runtime_pending_entry() {
    let runtime = KhiveRuntime::memory().unwrap();
    let clone = runtime.clone();
    runtime.record_config_locked("clone.pending", || "kept".into());
    clone.record_config_locked("clone.pending", || panic!("clone shares seen names"));
    clone
        .core()
        .record_config_locked("clone.core", || "same-primary".into());
    let mut unbound = VerbRegistryBuilder::new();
    unbound.register(AlphaPack);
    let unbound = unbound.build().unwrap();
    dispatch_ok(&unbound, "local", "unbound").await;
    assert!(unbound.audit_batch_handle().is_none());
    let explicit_store = clean_ledger_event_store();
    let mut explicit = VerbRegistryBuilder::new();
    explicit.register(AlphaPack);
    explicit.with_runtime_event_store(&runtime).unwrap();
    explicit.with_event_store(explicit_store.clone());
    let explicit = explicit.build().unwrap();
    dispatch_ok(&explicit, "local", "explicit").await;
    let event = first_event(&explicit_store).await;
    assert_ne!(event.kind, EventKind::ConfigLocked);
    let f = Fixture::from_runtime(clone, "local", "bound-clone");
    drop(runtime);
    f.dispatch().await;
    f.assert_rows(&[("clone.pending", "kept"), ("clone.core", "same-primary")])
        .await;
    f.finish().await;
    explicit
        .audit_batch_handle()
        .unwrap()
        .close_and_wait_for_physical_completion()
        .await
        .unwrap();
}

#[tokio::test]
#[serial(config_ledger)]
async fn a_registry_retains_the_last_runtime_clone_until_its_pending_entry_is_taken() {
    let f = Fixture::new();
    f.runtime
        .record_config_locked("lifetime.pending", || "retained".into());
    let Fixture {
        runtime,
        registry,
        store,
        namespace,
        actor,
    } = f;
    drop(runtime);
    dispatch_ok(&registry, &namespace, &actor).await;
    registry
        .audit_batch_handle()
        .unwrap()
        .close_and_wait_for_physical_completion()
        .await
        .unwrap();
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let rows: Vec<_> = page
        .items
        .into_iter()
        .filter(|event| event.kind == EventKind::ConfigLocked)
        .collect();
    assert_config_rows(&rows, &[("lifetime.pending", "retained")]);
}

#[tokio::test]
#[serial(config_ledger)]
async fn core_binding_identity_preserves_same_binding_and_rebinds_without_transfer() {
    let own_backend = prepared_backend();
    let core_a = prepared_backend();
    let core_b = prepared_backend();
    let runtime = runtime_on(own_backend, "pack").with_core_backend(core_a.clone());
    runtime.record_config_locked("core.same", || "pack-local".into());
    let old_core = runtime.core();
    old_core.record_config_locked("core.same", || "core-a".into());
    runtime
        .core()
        .record_config_locked("core.same", || panic!("repeated core shares ledger"));
    let same_binding = runtime.clone().with_core_backend(core_a.clone());
    same_binding
        .core()
        .record_config_locked("core.same", || panic!("same Arc preserves binding"));
    let rebound = runtime.with_core_backend(core_b);
    rebound
        .core()
        .record_config_locked("core.same", || "core-b".into());
    old_core.record_config_locked("core.old", || "old-handle".into());
    let local = Fixture::from_runtime(rebound.clone(), "local", "local");
    let old = Fixture::from_runtime(old_core, "local", "old-core");
    let new = Fixture::from_runtime(rebound.core(), "local", "new-core");
    local.dispatch().await;
    local.assert_rows(&[("core.same", "pack-local")]).await;
    old.dispatch().await;
    old.assert_rows(&[("core.same", "core-a"), ("core.old", "old-handle")])
        .await;
    new.dispatch().await;
    new.assert_rows(&[("core.same", "core-b")]).await;
    let independent = Fixture::from_runtime(runtime_on(core_a, "main"), "local", "independent");
    independent
        .runtime
        .record_config_locked("core.same", || "independent".into());
    independent.dispatch().await;
    assert_eq!(
        independent.rows().await.len(),
        3,
        "same backend Arc does not intern core ledgers"
    );
    local.finish().await;
    old.finish().await;
    new.finish().await;
    independent.finish().await;
}

#[test]
fn runtime_census_recognizes_dot_calls_and_lazy_fallback_wrappers() {
    let text = "fn observe(runtime: &Runtime) {\n\
        runtime.record_config_locked(\"key\", || true.to_string());\n\
    }\n\
    fn fallback(runtime: &Runtime) {\n\
        supplied.unwrap_or_else(|| observe(runtime));\n\
    }\n\
    fn process_only() { tracing::info!(target: \"khive::config_lock\", name = \"process\"); }\n";
    let seams = file_seam_names(text, &["record_config_locked"]);
    assert!(seams.iter().any(|name| name == "observe"));
    assert!(seams.iter().any(|name| name == "fallback"));
    assert!(!seams.iter().any(|name| name == "process_only"));
}

#[tokio::test]
#[serial(config_ledger)]
async fn core_first_drain_cannot_take_secondary_and_two_core_handles_emit_once() {
    let main_backend = prepared_backend();
    let secondary = runtime_on(prepared_backend(), "secondary").with_core_backend(main_backend);
    let core_one = secondary.core();
    let core_two = secondary.core();
    let s = Fixture::from_runtime(secondary.clone(), "local", "core-destination");
    let c1 = Fixture::from_runtime(core_one.clone(), "local", "core-destination");
    let c2 = Fixture::from_runtime(core_two.clone(), "local", "core-destination");

    secondary.record_config_locked("core.secondary_only", || "secondary-value".into());
    c1.dispatch().await;
    assert!(
        c1.rows().await.is_empty(),
        "core-first drain cannot route S's pending name to main"
    );
    s.dispatch().await;
    s.assert_rows(&[("core.secondary_only", "secondary-value")])
        .await;
    assert!(
        c1.rows().await.is_empty(),
        "secondary-only name stays absent from main"
    );

    // These are different fresh names and identical healthy values. A mutant
    // giving each core() handle a fresh ledger reaches the persisted two-row
    // count after the second real drain instead of panicking in a formatter.
    core_one.record_config_locked("core.core_only", || "core-value".into());
    core_two.record_config_locked("core.core_only", || "core-value".into());
    c1.dispatch().await;
    c1.assert_rows(&[("core.core_only", "core-value")]).await;
    c2.dispatch().await;
    c2.assert_rows(&[("core.core_only", "core-value")]).await;
    assert!(
        s.rows()
            .await
            .iter()
            .all(|row| row.payload["key"] != "core.core_only"),
        "core-only name is absent from the secondary store"
    );
    s.assert_rows(&[("core.secondary_only", "secondary-value")])
        .await;
    s.finish().await;
    c1.finish().await;
    c2.finish().await;
}
