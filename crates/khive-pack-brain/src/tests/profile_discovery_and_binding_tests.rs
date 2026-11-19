#[tokio::test]
async fn dispatch_state_returns_snapshot_fields() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.state",
            json!({}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    assert!(result.get("profiles").is_some(), "missing profiles");
    assert!(
        result.get("balanced_recall").is_some(),
        "missing balanced_recall"
    );
    assert!(result.get("bindings").is_some(), "missing bindings");
}

#[tokio::test]
async fn dispatch_profiles_returns_default_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.profiles",
            json!({}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    let profiles = result["profiles"].as_array().unwrap();
    assert!(!profiles.is_empty(), "expected at least one profile");
    assert_eq!(profiles[0]["id"], json!("balanced-recall-v1"));
}

#[tokio::test]
async fn dispatch_profiles_filtered_by_lifecycle() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.profiles",
            json!({"lifecycle": "active"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    let profiles = result["profiles"].as_array().unwrap();
    for p in profiles {
        assert_eq!(p["lifecycle"], json!("active"));
    }
}

#[tokio::test]
async fn dispatch_profile_returns_profile_details() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result["id"], json!("balanced-recall-v1"));
    assert_eq!(result["state_class"], json!("Bayesian"));
    assert_eq!(result["consumer_kind"], json!("recall"));
}

#[tokio::test]
async fn dispatch_profile_not_found_returns_not_found() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let err = pack
        .dispatch(
            "brain.profile",
            json!({"id": "nonexistent"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NotFound(_)));
}

#[tokio::test]
async fn dispatch_resolve_returns_default_profile_for_recall() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let result = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result["resolved_profile_id"], json!("balanced-recall-v1"));
}

#[tokio::test]
async fn dispatch_activate_and_deactivate_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // Deactivate the custom active profile
    let result = pack
        .dispatch(
            "brain.deactivate",
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["lifecycle"], json!("inactive"));

    // Verify via brain.profile
    let state = pack
        .dispatch(
            "brain.profile",
            json!({"id": LIFECYCLE_PROFILE_ID}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(state["lifecycle"], json!("inactive"));

    // Reactivate
    let result = pack
        .dispatch(
            "brain.activate",
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["lifecycle"], json!("active"));
}

#[tokio::test]
async fn dispatch_archive_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    // Lifecycle DAG requires active → inactive before archiving.
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let result = pack
        .dispatch(
            "brain.archive",
            json!({"profile_id": LIFECYCLE_PROFILE_ID}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["lifecycle"], json!("archived"));
}

#[tokio::test]
async fn dispatch_activate_nonexistent_profile_returns_not_found() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let err = pack
        .dispatch(
            "brain.activate",
            json!({"profile_id": "ghost-profile"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NotFound(_)));
}

#[tokio::test]
async fn dispatch_bind_and_resolve_explicit_binding() {
    let (pack, rt) = make_pack_with_actor("agent-x");
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Bind balanced-recall-v1 for actor "agent-x"
    let result = pack
        .dispatch(
            "brain.bind",
            json!({
                "profile_id": "balanced-recall-v1",
                "actor": "agent-x",
                "consumer_kind": "recall"
            }),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["bound"], json!(true));
    assert_eq!(result["actor"], json!("agent-x"));

    // Resolve — should return the explicitly bound profile
    let resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"actor": "agent-x", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(resolved["resolved_profile_id"], json!("balanced-recall-v1"));
}

#[tokio::test]
async fn dispatch_bind_nonexistent_profile_returns_not_found() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let err = pack
        .dispatch(
            "brain.bind",
            json!({"profile_id": "ghost", "consumer_kind": "recall"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NotFound(_)));
}

#[tokio::test]
async fn dispatch_unbind_removes_binding() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Add a binding
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "agent-y", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Remove it
    let result = pack
        .dispatch(
            "brain.unbind",
            json!({"actor": "agent-y"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["removed"], json!(1u64));
    assert_eq!(result["unbound"], json!(1u64));
}

#[tokio::test]
async fn dispatch_unbind_reports_zero_when_selector_matches_nothing() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let result = pack
        .dispatch(
            "brain.unbind",
            json!({"profile_id": "well-formed-but-unbound"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

    assert_eq!(result["removed"], json!(0u64));
    assert_eq!(result["unbound"], json!(0u64));
}

#[tokio::test]
async fn dispatch_bind_team_catalog_resolves_only_in_team_catalog() {
    use core::convert::TryFrom;

    let (pack, rt) = make_pack_with_actor("catalog-match");
    let registry = empty_registry();
    let local = rt.authorize(Namespace::local()).unwrap();
    let team = rt
        .authorize(Namespace::try_from("team-a").unwrap())
        .unwrap();
    let profile_id = "team-catalog-profile";
    let created = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": profile_id, "consumer_kind": "recall"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(created["created"], json!(true));
    assert_eq!(created["profile_id"], json!(profile_id));
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": profile_id}),
        &registry,
        &team,
    )
    .await
    .unwrap();
    let profiles = pack
        .dispatch("brain.profiles", json!({}), &registry, &team)
        .await
        .unwrap();
    assert_eq!(profiles["namespace"], json!("team-a"));
    assert!(profiles["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == json!(profile_id)));
    let profile = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": profile_id}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(profile["namespace"], json!("team-a"));
    assert_eq!(profile["id"], json!(profile_id));
    assert_eq!(profile["lifecycle"], json!("active"));
    let local_profiles = pack
        .dispatch("brain.profiles", json!({}), &registry, &local)
        .await
        .unwrap();
    assert_eq!(local_profiles["namespace"], json!("local"));
    assert!(!local_profiles["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == json!(profile_id)));
    let local_profile = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &local,
        )
        .await
        .unwrap();
    assert_eq!(local_profile["namespace"], json!("local"));
    assert_eq!(local_profile["id"], json!("balanced-recall-v1"));

    let bound = pack
        .dispatch(
            "brain.bind",
            json!({"profile_id": profile_id, "actor": "catalog-match", "namespace": "team-a", "consumer_kind": "recall"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(bound["bound"], json!(true));
    assert_eq!(bound["namespace"], json!("team-a"));
    let bindings = pack
        .dispatch(
            "brain.bindings",
            json!({"actor": "catalog-match", "namespace": "team-a"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(bindings["namespace"], json!("team-a"));
    assert_eq!(bindings["count"], json!(1));
    assert_eq!(bindings["bindings"][0]["profile_id"], json!(profile_id));
    assert_eq!(bindings["bindings"][0]["namespace"], json!("team-a"));
    let resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"actor": "catalog-match", "namespace": "team-a", "consumer_kind": "recall"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(resolved["resolved_profile_id"], json!(profile_id));
    assert_eq!(resolved["matched_binding"], json!(true));
    let local_resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"actor": "catalog-match", "namespace": "local", "consumer_kind": "recall"}),
            &registry,
            &local,
        )
        .await
        .unwrap();
    assert_eq!(local_resolved["matched_binding"], json!(false));
    assert_ne!(local_resolved["resolved_profile_id"], json!(profile_id));
    let local_bindings = pack
        .dispatch(
            "brain.bindings",
            json!({"actor": "catalog-match", "namespace": "local"}),
            &registry,
            &local,
        )
        .await
        .unwrap();
    assert_eq!(local_bindings["namespace"], json!("local"));
    assert_eq!(local_bindings["count"], json!(0));
}

#[tokio::test]
async fn dispatch_bind_missing_profile_names_searched_catalog() {
    use core::convert::TryFrom;

    let (pack, rt) = make_pack_with_actor("catalog-refusal");
    let registry = empty_registry();
    let local = rt.authorize(Namespace::local()).unwrap();
    let team = rt
        .authorize(Namespace::try_from("team-a").unwrap())
        .unwrap();
    let profile_id = "local-only-profile";
    let created = pack
        .dispatch(
            "brain.create_profile",
            json!({"name": profile_id, "consumer_kind": "recall"}),
            &registry,
            &local,
        )
        .await
        .unwrap();
    assert_eq!(created["created"], json!(true));
    assert_eq!(created["profile_id"], json!(profile_id));
    let local_profiles = pack
        .dispatch("brain.profiles", json!({}), &registry, &local)
        .await
        .unwrap();
    assert_eq!(local_profiles["namespace"], json!("local"));
    assert!(local_profiles["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == json!(profile_id)));
    let local_profile = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": profile_id}),
            &registry,
            &local,
        )
        .await
        .unwrap();
    assert_eq!(local_profile["namespace"], json!("local"));
    assert_eq!(local_profile["id"], json!(profile_id));
    let team_profiles = pack
        .dispatch("brain.profiles", json!({}), &registry, &team)
        .await
        .unwrap();
    assert_eq!(team_profiles["namespace"], json!("team-a"));
    assert!(!team_profiles["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == json!(profile_id)));
    let team_profile = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(team_profile["namespace"], json!("team-a"));
    assert_eq!(team_profile["id"], json!("balanced-recall-v1"));
    let before = pack
        .dispatch(
            "brain.bindings",
            json!({"actor": "catalog-refusal", "namespace": "team-a"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(before["namespace"], json!("team-a"));
    assert_eq!(before["count"], json!(0));

    let error = pack
        .dispatch(
            "brain.bind",
            json!({"profile_id": profile_id, "actor": "catalog-refusal", "namespace": "team-a", "consumer_kind": "recall"}),
            &registry,
            &team,
        )
        .await
        .unwrap_err();
    let RuntimeError::NotFound(message) = error else {
        panic!("bind must refuse a profile absent from the selected catalog with NotFound");
    };
    assert_eq!(
        message,
        r#"profile "local-only-profile" not found in namespace "team-a"; a profile created with namespace=X is bound with namespace=X"#,
        "bind_not_found_names_searched_catalog"
    );
    let after = pack
        .dispatch(
            "brain.bindings",
            json!({"actor": "catalog-refusal", "namespace": "team-a"}),
            &registry,
            &team,
        )
        .await
        .unwrap();
    assert_eq!(after["namespace"], json!("team-a"));
    assert_eq!(after["count"], json!(0));
}

async fn create_profile_for_binding_status(
    pack: &BrainPack,
    registry: &khive_runtime::VerbRegistry,
    token: &NamespaceToken,
    profile_id: &str,
) {
    pack.dispatch(
        "brain.create_profile",
        json!({"name": profile_id, "consumer_kind": "recall"}),
        registry,
        token,
    )
    .await
    .expect("create inactive profile");
}

async fn read_profile_binding_status(
    pack: &BrainPack,
    registry: &khive_runtime::VerbRegistry,
    token: &NamespaceToken,
    profile_id: &str,
) -> Value {
    pack.dispatch(
        "brain.profile",
        json!({"profile_id": profile_id}),
        registry,
        token,
    )
    .await
    .expect("read profile binding status")
}

#[tokio::test]
async fn profile_reports_inactive_binding_and_lifecycle_transitions() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = "inactive-bound-status";
    create_profile_for_binding_status(&pack, &registry, &token, id).await;
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": id, "actor": "brain-test", "namespace": "local", "consumer_kind": "recall"}),
        &registry,
        &token,
    ).await.expect("bind without activating");

    let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
    let bindings =
        json!([{"actor": "brain-test", "namespace": "local", "consumer_kind": "recall"}]);
    assert_eq!(profile["lifecycle"], "inactive");
    assert_eq!(profile["resolving_bindings"], bindings);
    assert_eq!(
        profile["serving_through_visible_binding_while_inactive"], true,
        "inactive bound profile must report serving through its visible binding"
    );
    let resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"namespace": "local", "consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .expect("binding resolves while inactive");
    assert_eq!(resolved["resolved_profile_id"], id);
    assert_eq!(resolved["matched_binding"], true);

    for (verb, lifecycle, serving_while_inactive) in [
        ("brain.activate", "active", false),
        ("brain.deactivate", "inactive", true),
    ] {
        pack.dispatch(verb, json!({"profile_id": id}), &registry, &token)
            .await
            .expect("change fallback lifecycle");
        let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
        assert_eq!(profile["lifecycle"], lifecycle);
        assert_eq!(profile["resolving_bindings"], bindings);
        assert_eq!(
            profile["serving_through_visible_binding_while_inactive"],
            serving_while_inactive
        );
    }
}

#[tokio::test]
async fn profile_reports_no_binding_for_unbound_inactive_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = "inactive-unbound-status";
    create_profile_for_binding_status(&pack, &registry, &token, id).await;
    let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
    assert_eq!(profile["lifecycle"], "inactive");
    assert_eq!(profile["resolving_bindings"], json!([]));
    assert_eq!(
        profile["serving_through_visible_binding_while_inactive"],
        false
    );
}

#[tokio::test]
async fn profile_omits_archived_binding_without_removing_stored_binding() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = "archived-bound-status";
    create_profile_for_binding_status(&pack, &registry, &token, id).await;
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": id, "actor": "brain-test", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("bind before archive");
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": id}),
        &registry,
        &token,
    )
    .await
    .expect("archive inactive profile");

    let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
    assert_eq!(profile["lifecycle"], "archived");
    assert_eq!(profile["resolving_bindings"], json!([]));
    assert_eq!(
        profile["serving_through_visible_binding_while_inactive"],
        false
    );
    let stored = pack
        .dispatch(
            "brain.bindings",
            json!({"profile_id": id}),
            &registry,
            &token,
        )
        .await
        .expect("read retained binding");
    assert_eq!(stored["count"], 1);
    assert_eq!(stored["bindings"][0]["profile_id"], id);
    let resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .expect("resolve around archived binding");
    assert_ne!(resolved["resolved_profile_id"], id);
    assert_eq!(resolved["matched_binding"], false);
}

#[tokio::test]
async fn profile_binding_status_respects_actor_visibility() {
    let (pack, rt) = make_pack_with_read_scope(Some("caller-a"), &[], &[]);
    let registry = empty_registry();
    let hidden_token = rt.authorize(Namespace::local()).unwrap();
    let id = "foreign-binding-status";
    create_profile_for_binding_status(&pack, &registry, &hidden_token, id).await;
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": id, "actor": "caller-b", "consumer_kind": "recall"}),
        &registry,
        &hidden_token,
    )
    .await
    .expect("store foreign actor binding");
    let hidden = read_profile_binding_status(&pack, &registry, &hidden_token, id).await;
    assert_eq!(hidden["resolving_bindings"], json!([]));
    assert_eq!(
        hidden["serving_through_visible_binding_while_inactive"],
        false
    );
    assert!(!hidden.to_string().contains("caller-b"));

    let (_, visible_rt) = make_pack_with_read_scope(Some("caller-a"), &["caller-b"], &[]);
    let visible_token = visible_rt
        .authorize_with_visibility(Namespace::local(), visible_rt.visible_namespaces().to_vec())
        .unwrap();
    let visible = read_profile_binding_status(&pack, &registry, &visible_token, id).await;
    assert_eq!(
        visible["resolving_bindings"],
        json!([{"actor": "caller-b", "namespace": "*", "consumer_kind": "recall"}])
    );
    assert_eq!(
        visible["serving_through_visible_binding_while_inactive"],
        true
    );
}

#[tokio::test]
async fn profile_binding_status_uses_selected_catalog() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let local = rt.authorize(Namespace::local()).unwrap();
    let team = rt.authorize(Namespace::parse("team-a").unwrap()).unwrap();
    let id = "same-id-binding-status";
    create_profile_for_binding_status(&pack, &registry, &local, id).await;
    create_profile_for_binding_status(&pack, &registry, &team, id).await;
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": id, "actor": "brain-test", "consumer_kind": "recall"}),
        &registry,
        &local,
    )
    .await
    .expect("bind only in local catalog");

    let team_profile = read_profile_binding_status(&pack, &registry, &team, id).await;
    assert_eq!(team_profile["namespace"], "team-a");
    assert_eq!(team_profile["lifecycle"], "inactive");
    assert_eq!(team_profile["resolving_bindings"], json!([]));
    assert_eq!(
        team_profile["serving_through_visible_binding_while_inactive"],
        false
    );
    let local_profile = read_profile_binding_status(&pack, &registry, &local, id).await;
    assert_eq!(local_profile["namespace"], "local");
    assert_eq!(
        local_profile["resolving_bindings"],
        json!([{"actor": "brain-test", "namespace": "*", "consumer_kind": "recall"}])
    );
    assert_eq!(
        local_profile["serving_through_visible_binding_while_inactive"],
        true
    );
}

#[tokio::test]
async fn profile_binding_status_preserves_wildcard_selectors() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = "wildcard-binding-status";
    create_profile_for_binding_status(&pack, &registry, &token, id).await;
    pack.dispatch("brain.bind", json!({"profile_id": id}), &registry, &token)
        .await
        .expect("store wildcard selectors");
    let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
    assert_eq!(
        profile["resolving_bindings"],
        json!([{"actor": "*", "namespace": "*", "consumer_kind": "*"}])
    );
    assert_eq!(
        profile["serving_through_visible_binding_while_inactive"],
        true
    );
}

struct BindingStatusConsumers;

impl khive_types::Pack for BindingStatusConsumers {
    const NAME: &'static str = "binding-status-consumers";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] = &[
        khive_brain_core::ConsumerKind::Recall.as_str(),
        khive_brain_core::ConsumerKind::KnowledgeCompose.as_str(),
        khive_brain_core::ConsumerKind::Rerank.as_str(),
    ];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait::async_trait]
impl PackRuntime for BindingStatusConsumers {
    khive_runtime::pack_runtime_metadata!();

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &khive_runtime::VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!("no handler for {verb}")))
    }
}

fn binding_status_consumer_registry() -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(BindingStatusConsumers);
    let registry = builder.build().unwrap();
    assert_eq!(registry.all_brain_consumer_kinds().len(), 3);
    registry
}

const STATUS_CONSUMER_KINDS: [khive_brain_core::ConsumerKind; 3] = [
    khive_brain_core::ConsumerKind::Recall,
    khive_brain_core::ConsumerKind::KnowledgeCompose,
    khive_brain_core::ConsumerKind::Rerank,
];

async fn bind_status_selector(
    pack: &BrainPack,
    registry: &khive_runtime::VerbRegistry,
    token: &NamespaceToken,
    profile_id: &str,
    actor: &str,
    namespace: &str,
    consumer_kind: &str,
) {
    let bound = pack
        .dispatch(
            "brain.bind",
            json!({"profile_id":profile_id, "actor":actor, "namespace":namespace, "consumer_kind":consumer_kind}),
            registry,
            token,
        )
        .await
        .unwrap();
    assert_eq!(bound["bound"], true);
}

async fn resolve_status_consumer(
    pack: &BrainPack,
    registry: &khive_runtime::VerbRegistry,
    token: &NamespaceToken,
    kind: khive_brain_core::ConsumerKind,
) -> Value {
    pack.dispatch(
        "brain.resolve",
        json!({"actor":token.actor().binding_id(), "namespace":token.namespace().as_str(), "consumer_kind":kind.as_str()}),
        registry,
        token,
    )
    .await
    .unwrap()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn profile_binding_status_omits_fully_shadowed_wildcard_kind() {
    let (pack, rt) = make_pack();
    let registry = binding_status_consumer_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = "fully-shadowed-kind";
    let winner = "all-kind-winner";
    for profile_id in [id, winner] {
        create_profile_for_binding_status(&pack, &registry, &token, profile_id).await;
    }
    bind_status_selector(&pack, &registry, &token, id, "brain-test", "local", "*").await;
    for kind in STATUS_CONSUMER_KINDS {
        bind_status_selector(
            &pack,
            &registry,
            &token,
            winner,
            "brain-test",
            "local",
            kind.as_str(),
        )
        .await;
        let resolved = resolve_status_consumer(&pack, &registry, &token, kind).await;
        assert_eq!(resolved["resolved_profile_id"], winner);
        assert_eq!(resolved["matched_binding"], true);
    }
    let before = serde_json::to_value(pack.snapshot()).unwrap();
    let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
    assert_eq!(profile["lifecycle"], "inactive");
    assert_eq!(profile["resolving_bindings"], json!([]));
    assert_eq!(
        profile["serving_through_visible_binding_while_inactive"],
        false
    );
    assert_eq!(serde_json::to_value(pack.snapshot()).unwrap(), before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn profile_binding_status_retains_partially_shadowed_wildcard_kind() {
    let (pack, rt) = make_pack();
    let registry = binding_status_consumer_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = "partially-shadowed-kind";
    let winner = "two-kind-winner";
    for profile_id in [id, winner] {
        create_profile_for_binding_status(&pack, &registry, &token, profile_id).await;
    }
    bind_status_selector(&pack, &registry, &token, id, "brain-test", "local", "*").await;
    for kind in &STATUS_CONSUMER_KINDS[..2] {
        bind_status_selector(
            &pack,
            &registry,
            &token,
            winner,
            "brain-test",
            "local",
            kind.as_str(),
        )
        .await;
    }
    for kind in STATUS_CONSUMER_KINDS {
        let resolved = resolve_status_consumer(&pack, &registry, &token, kind).await;
        assert_eq!(
            resolved["resolved_profile_id"],
            if kind == khive_brain_core::ConsumerKind::Rerank {
                id
            } else {
                winner
            }
        );
        assert_eq!(resolved["matched_binding"], true);
    }
    let before = serde_json::to_value(pack.snapshot()).unwrap();
    let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
    assert_eq!(profile["lifecycle"], "inactive");
    assert_eq!(
        profile["resolving_bindings"],
        json!([{"actor":"brain-test", "namespace":"local", "consumer_kind":"*"}])
    );
    assert_eq!(
        profile["serving_through_visible_binding_while_inactive"],
        true
    );
    assert_eq!(serde_json::to_value(pack.snapshot()).unwrap(), before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn profile_binding_status_omits_foreign_namespace_selector() {
    let (pack, rt) = make_pack();
    let registry = binding_status_consumer_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = "foreign-namespace-selector";
    create_profile_for_binding_status(&pack, &registry, &token, id).await;
    bind_status_selector(
        &pack,
        &registry,
        &token,
        id,
        "brain-test",
        "another-catalog",
        "recall",
    )
    .await;
    let stored = pack
        .dispatch(
            "brain.bindings",
            json!({"profile_id":id}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(stored["namespace"], "local");
    assert_eq!(stored["count"], 1);
    assert_eq!(stored["bindings"][0]["namespace"], "another-catalog");
    let resolved = resolve_status_consumer(
        &pack,
        &registry,
        &token,
        khive_brain_core::ConsumerKind::Recall,
    )
    .await;
    assert_ne!(resolved["resolved_profile_id"], id);
    assert_eq!(resolved["matched_binding"], false);
    let before = serde_json::to_value(pack.snapshot()).unwrap();
    let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
    assert_eq!(profile["namespace"], "local");
    assert_eq!(profile["lifecycle"], "inactive");
    assert_eq!(profile["resolving_bindings"], json!([]));
    assert_eq!(
        profile["serving_through_visible_binding_while_inactive"],
        false
    );
    assert_eq!(serde_json::to_value(pack.snapshot()).unwrap(), before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn profile_binding_status_keeps_wildcard_actor_when_caller_is_shadowed() {
    let (pack, rt) = make_pack();
    let registry = binding_status_consumer_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = "anonymous-wildcard-winner";
    let winner = "named-actor-winner";
    for profile_id in [id, winner] {
        create_profile_for_binding_status(&pack, &registry, &token, profile_id).await;
    }
    bind_status_selector(&pack, &registry, &token, id, "*", "local", "*").await;
    for kind in STATUS_CONSUMER_KINDS {
        bind_status_selector(
            &pack,
            &registry,
            &token,
            winner,
            "brain-test",
            "local",
            kind.as_str(),
        )
        .await;
        let resolved = resolve_status_consumer(&pack, &registry, &token, kind).await;
        assert_eq!(resolved["resolved_profile_id"], winner);
        assert_eq!(resolved["matched_binding"], true);
    }
    let (_, anonymous_rt) = make_anonymous_pack();
    let anonymous = anonymous_rt.authorize(Namespace::local()).unwrap();
    assert_eq!(anonymous.actor().binding_id(), None);
    for kind in STATUS_CONSUMER_KINDS {
        let resolved = resolve_status_consumer(&pack, &registry, &anonymous, kind).await;
        assert_eq!(resolved["resolved_profile_id"], id);
        assert_eq!(resolved["matched_binding"], true);
    }
    let before = serde_json::to_value(pack.snapshot()).unwrap();
    let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
    assert_eq!(profile["lifecycle"], "inactive");
    assert_eq!(
        profile["resolving_bindings"],
        json!([{"actor":"*", "namespace":"local", "consumer_kind":"*"}])
    );
    assert_eq!(
        profile["serving_through_visible_binding_while_inactive"],
        true
    );
    assert_eq!(serde_json::to_value(pack.snapshot()).unwrap(), before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn profile_binding_status_omits_shadowed_wildcard_namespace() {
    let (pack, rt) = make_pack();
    let registry = binding_status_consumer_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = "shadowed-namespace";
    let winner = "concrete-namespace-winner";
    for profile_id in [id, winner] {
        create_profile_for_binding_status(&pack, &registry, &token, profile_id).await;
    }
    bind_status_selector(&pack, &registry, &token, id, "brain-test", "*", "recall").await;
    bind_status_selector(
        &pack,
        &registry,
        &token,
        winner,
        "brain-test",
        "local",
        "recall",
    )
    .await;
    let resolved = resolve_status_consumer(
        &pack,
        &registry,
        &token,
        khive_brain_core::ConsumerKind::Recall,
    )
    .await;
    assert_eq!(resolved["resolved_profile_id"], winner);
    assert_eq!(resolved["matched_binding"], true);
    let before = serde_json::to_value(pack.snapshot()).unwrap();
    let profile = read_profile_binding_status(&pack, &registry, &token, id).await;
    assert_eq!(profile["lifecycle"], "inactive");
    assert_eq!(profile["resolving_bindings"], json!([]));
    assert_eq!(
        profile["serving_through_visible_binding_while_inactive"],
        false
    );
    assert_eq!(serde_json::to_value(pack.snapshot()).unwrap(), before);
}
