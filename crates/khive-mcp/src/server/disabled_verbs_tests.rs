fn disabled_verbs_runtime_config() -> RuntimeConfig {
    let mut config = RuntimeConfig::default().for_metadata_registry();
    config.wal_ceiling_env_raw = None;
    config.disk_guard_environment = khive_db::DiskGuardEnvironment::default();
    config.volume_lock_dir = None;
    config.credentials.clear();
    config.visibility_receipts = None;
    config.mounts.clear();
    config.events_split = None;
    config.actor_id = None;
    config.visible_namespaces.clear();
    config.allowed_outbound_namespaces.clear();
    config.brain_profile = None;
    config.brain = Default::default();
    config.default_namespace = Namespace::local();
    config.gate = Arc::new(khive_runtime::AllowAllGate);
    config.packs = vec!["kg".into(), "memory".into()];
    config
}

fn disabled_verbs_config(policy: &str) -> khive_runtime::KhiveConfig {
    toml::from_str(&format!(
        "[[backends]]\nname = 'main'\nkind = 'memory'\n{policy}"
    ))
    .expect("memory deployment policy")
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn disabled_verbs_hide_capabilities_refuse_dispatch_and_keep_operator_metadata() {
    use khive_runtime::{DomainDisposition, InterceptedDispatchResult, RequestIdentity};
    let config = disabled_verbs_config("[packs.memory]\nverbs_disabled = ['memory.remember']\n");
    assert_eq!(config.packs["memory"].backend, "main");
    config.validate().unwrap();
    let built = crate::serve::build_registry_for_multi_backend(
        disabled_verbs_runtime_config(),
        &config,
        None,
    )
    .await
    .unwrap();
    let registry = &built.registry;
    assert!(!registry.has_verb("memory.remember"));
    assert!(registry.has_verb("memory.recall"));
    assert!(registry
        .all_verbs()
        .iter()
        .all(|h| h.name != "memory.remember"));
    assert!(registry
        .all_verbs_with_names()
        .iter()
        .all(|(_, h)| h.name != "memory.remember"));
    assert!(registry
        .all_handlers_with_names()
        .iter()
        .any(|(p, h)| *p == "memory" && h.name == "memory.remember"));
    assert!(registry
        .pack_verbs("memory")
        .unwrap()
        .iter()
        .any(|h| h.name == "memory.remember"));
    assert!(matches!(
        registry.describe_verb("memory.remember"),
        Err(RuntimeError::UnknownVerb(_))
    ));

    let identity = RequestIdentity {
        namespace: "local".into(),
        actor_id: Some("tester".into()),
        ..Default::default()
    };
    let params =
        json!({"content": "must not be written", "salience": 0.8, "memory_type": "semantic"});
    let error = registry
        .dispatch_with_disposition("memory.remember", params.clone(), Some(identity.clone()))
        .await
        .unwrap_err();
    assert!(matches!(error.source(), RuntimeError::UnknownVerb(_)));
    assert_eq!(error.disposition(), DomainDisposition::NotCommitted);
    let unknown = registry
        .dispatch_with_disposition("unknown.verb", json!({}), Some(identity.clone()))
        .await
        .unwrap_err();
    assert!(matches!(unknown.source(), RuntimeError::UnknownVerb(_)));
    assert_eq!(error.disposition(), unknown.disposition());

    let invoked = std::sync::atomic::AtomicBool::new(false);
    let intercepted = registry
        .dispatch_intercepted_with_token_and_disposition(
            "memory.remember",
            &params,
            Some(&identity),
            |_| async {
                invoked.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(InterceptedDispatchResult::new(json!({}), ()))
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(intercepted.source(), RuntimeError::UnknownVerb(_)));
    assert_eq!(intercepted.disposition(), DomainDisposition::NotCommitted);
    assert!(!invoked.load(std::sync::atomic::Ordering::SeqCst));
    let token = built.default_runtime.authorize(Namespace::local()).unwrap();
    let audits = built
        .default_runtime
        .events(&token)
        .unwrap()
        .query_events(
            EventFilter {
                verbs: vec!["memory.remember".into(), "unknown.verb".into()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(audits.items.len(), 3);
    assert!(audits.items.iter().all(|event| event.namespace == "local"
        && event.actor == "tester"
        && event.outcome == khive_types::EventOutcome::Error));
    let invalid_namespace = registry
        .dispatch_with_disposition("memory.remember", json!({"namespace": 5}), None)
        .await
        .unwrap_err();
    assert!(matches!(
        invalid_namespace.source(),
        RuntimeError::InvalidInput(_)
    ));
    let notes = registry
        .dispatch("list", json!({"kind": "memory"}))
        .await
        .unwrap();
    assert!(notes["items"].as_array().unwrap().is_empty());

    let server = KhiveMcpServer::from_registry(built.registry);
    assert!(!server.verb_catalog().contains("memory.remember"));
    assert!(server.verb_catalog().contains("memory.recall"));
    let raw = server
        .dispatch_request_local(RequestParams {
            ops: r#"memory.remember(content="still refused")"#.into(),
            presentation: Some("verbose".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let response: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(response["results"][0]["ok"], false, "{response}");
    assert_eq!(
        response["results"][0]["reason"], "verb-refused",
        "{response}"
    );
    assert!(
        response["results"][0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown verb"),
        "{response}"
    );
    let notes = server
        .registry
        .dispatch("list", json!({"kind": "memory"}))
        .await
        .unwrap();
    assert!(notes["items"].as_array().unwrap().is_empty());

    let enabled = crate::serve::build_registry_for_multi_backend(
        disabled_verbs_runtime_config(),
        &disabled_verbs_config(""),
        None,
    )
    .await
    .unwrap();
    enabled
        .registry
        .dispatch("memory.remember", params)
        .await
        .unwrap();
    let notes = enabled
        .registry
        .dispatch("list", json!({"kind": "memory"}))
        .await
        .unwrap();
    assert_eq!(notes["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn disabled_verbs_reject_unloaded_foreign_and_unknown_handlers_at_boot() {
    for (policy, expected) in [
        (
            "[packs.gtd]\nverbs_disabled = ['gtd.assign']\n",
            "pack is not loaded",
        ),
        (
            "[packs.memory]\nverbs_disabled = ['create']\n",
            "not a public verb owned by this pack",
        ),
        (
            "[packs.memory]\nverbs_disabled = ['memory.forget']\n",
            "not a public verb owned by this pack",
        ),
        (
            "[packs.memory]\nverbs_disabled = ['memory.recall_embed']\n",
            "not a public verb owned by this pack",
        ),
    ] {
        let result = crate::serve::build_registry_for_multi_backend(
            disabled_verbs_runtime_config(),
            &disabled_verbs_config(policy),
            None,
        )
        .await;
        let error = result
            .err()
            .expect("invalid deployment policy must fail boot");
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}

#[test]
#[serial_test::serial(config_ledger)]
fn disabled_verbs_config_id_tracks_the_sorted_effective_policy() {
    let runtime = disabled_verbs_runtime_config();
    let first = disabled_verbs_config(
        "[packs.memory]\nverbs_disabled = ['memory.remember', 'memory.prune']\n",
    );
    let reordered = disabled_verbs_config(
        "[packs.memory]\nverbs_disabled = ['memory.prune', 'memory.remember', 'memory.prune']\n",
    );
    let different = disabled_verbs_config("[packs.memory]\nverbs_disabled = ['memory.remember']\n");
    let empty = disabled_verbs_config("[packs.memory]\nverbs_disabled = []\n");
    let absent = disabled_verbs_config("[packs.memory]\nbackend = 'main'\n");
    assert_eq!(
        compute_config_id(&runtime, Some(&first)),
        compute_config_id(&runtime, Some(&reordered))
    );
    assert_ne!(
        compute_config_id(&runtime, Some(&first)),
        compute_config_id(&runtime, Some(&different))
    );
    assert_ne!(
        compute_config_id(&runtime, Some(&different)),
        compute_config_id(&runtime, Some(&empty))
    );
    assert_eq!(
        compute_config_id(&runtime, Some(&empty)),
        compute_config_id(&runtime, Some(&absent))
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn disabled_verbs_apply_to_implicit_main_and_its_daemon_identity() {
    let config: khive_runtime::KhiveConfig =
        toml::from_str("[packs.memory]\nverbs_disabled = ['memory.remember']\n").unwrap();
    config.validate().unwrap();
    let runtime = KhiveRuntime::new(disabled_verbs_runtime_config()).unwrap();
    let expected = compute_config_id(runtime.config(), Some(&config));
    assert_ne!(expected, compute_config_id(runtime.config(), None));
    let server = KhiveMcpServer::new_with_mounts_and_config(runtime, Some(&config))
        .await
        .unwrap();
    assert_eq!(server.config_id, expected);
    assert!(!server.verb_catalog().contains("memory.remember"));
    assert!(matches!(
        server
            .registry
            .dispatch("memory.remember", json!({"content": "no write"}))
            .await,
        Err(RuntimeError::UnknownVerb(_))
    ));
}
