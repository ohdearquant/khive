use super::*;

// #1781: corpus targets retain their own identity and scalar judgments.
fn fixture(
    actor: Option<&str>,
    profile: Option<&str>,
    brain: bool,
) -> (tempfile::TempDir, KhiveRuntime, khive_runtime::VerbRegistry) {
    let dir = tempfile::tempdir().expect("feedback fixture directory");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("feedback.db")),
        actor_id: actor.map(str::to_owned),
        brain_profile: profile.map(str::to_owned),
        packs: if brain {
            vec!["kg".into(), "knowledge".into(), "brain".into()]
        } else {
            vec!["kg".into(), "knowledge".into()]
        },
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(actor.map(str::to_owned));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    if brain {
        builder.register(BrainPack::new(runtime.clone()));
    }
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    (dir, runtime, registry)
}

async fn knowledge_event_count(runtime: &KhiveRuntime) -> u64 {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .events(&token)
        .unwrap()
        .count_events(khive_storage::EventFilter {
            verbs: vec!["knowledge.feedback".into()],
            ..Default::default()
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn corpus_uuid_and_prefix_matrix_preserves_scalar_and_section_learning() {
    let (_dir, runtime, registry) = fixture(Some("corpus-feedback"), Some("corpus-profile"), true);
    // Equal priors exercise a visible decrease before the weight reaches its floor.
    registry
        .dispatch(
            "brain.create_profile",
            json!({
                "name":"corpus-profile",
                "consumer_kind":"knowledge_compose",
                "seed_priors":{"section_posteriors":{"overview":{"alpha":2.0,"beta":2.0}}}
            }),
        )
        .await
        .unwrap();
    registry
        .dispatch("brain.activate", json!({"profile_id":"corpus-profile"}))
        .await
        .unwrap();
    let atom = make_atom(&registry, "local").await;
    let domain = make_domain(&registry, "local").await;
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut saw_weight_decrease = false;
    let mut saw_pinned_weight = false;
    let weight_floor = khive_brain_core::DEFAULT_SECTION_WEIGHT_FLOOR;
    for (kind, target) in [("atom", atom), ("domain", domain)] {
        for input in [target.clone(), target.replace('-', "")[..8].to_owned()] {
            let before = registry
                .dispatch("brain.profile", json!({"profile_id":"corpus-profile"}))
                .await
                .unwrap();
            let response = registry.dispatch("knowledge.feedback", json!({
                "target_id":input, "signal":"wrong", "section_signals":{"overview":"not_useful"}
            })).await.unwrap();
            assert_eq!(response["target_id"], target);
            assert_eq!(response["target_kind"], kind);
            assert_eq!(response["signal"], "wrong");
            let event = runtime
                .events(&token)
                .unwrap()
                .get_event(response["event_id"].as_str().unwrap().parse().unwrap())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event.verb, "knowledge.feedback");
            assert_eq!(event.target_id.unwrap().to_string(), target);
            assert_eq!(event.payload["signal"], "wrong");
            assert_eq!(event.payload["target_kind"], kind);
            let section_event = runtime
                .events(&token)
                .unwrap()
                .get_event(
                    response["profile_event_id"]
                        .as_str()
                        .unwrap()
                        .parse()
                        .unwrap(),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(section_event.verb, "brain.section_feedback");
            assert!(section_event.target_id.is_none());
            assert!(section_event.payload.get("signal").is_none());
            assert_eq!(
                section_event.payload["target_attribution"],
                format!("{kind}:{target}")
            );
            let after = registry
                .dispatch("brain.profile", json!({"profile_id":"corpus-profile"}))
                .await
                .unwrap();
            assert!(
                after["section_posteriors"]["overview"]["beta"]
                    .as_f64()
                    .unwrap()
                    > before["section_posteriors"]["overview"]["beta"]
                        .as_f64()
                        .unwrap()
            );
            let before_weight = before["section_posteriors"]["overview"]["weight"]
                .as_f64()
                .unwrap();
            let after_weight = after["section_posteriors"]["overview"]["weight"]
                .as_f64()
                .unwrap();
            assert!(after_weight >= weight_floor);
            if before_weight > weight_floor {
                assert!(
                    after_weight < before_weight,
                    "negative evidence must lower a weight that is above the floor"
                );
                saw_weight_decrease = true;
            } else {
                assert_eq!(before_weight, weight_floor);
                assert_eq!(after_weight, weight_floor);
                saw_pinned_weight = true;
            }
            assert_eq!(
                after["total_events"], before["total_events"],
                "section evidence must not invent a recall vote"
            );
        }
    }
    assert!(saw_weight_decrease, "exercise an unfloored weight update");
    assert!(
        saw_pinned_weight,
        "exercise continued learning at the floor"
    );
    assert_eq!(knowledge_event_count(&runtime).await, 4);
}

#[tokio::test]
async fn scalar_only_leaves_sections_unchanged_and_section_only_invents_no_scalar() {
    let (_dir, runtime, registry) =
        fixture(Some("corpus-feedback"), Some("balanced-recall-v1"), true);
    let atom = make_atom(&registry, "local").await;
    let before = registry
        .dispatch("brain.profile", json!({"profile_id":"balanced-recall-v1"}))
        .await
        .unwrap();
    let scalar = registry
        .dispatch(
            "knowledge.feedback",
            json!({"target_id":atom,"signal":"not_useful"}),
        )
        .await
        .unwrap();
    assert_eq!(scalar["signals_applied"], 0);
    assert!(scalar["profile_event_id"].is_null());
    let after = registry
        .dispatch("brain.profile", json!({"profile_id":"balanced-recall-v1"}))
        .await
        .unwrap();
    assert_eq!(after, before);
    let section = registry
        .dispatch(
            "knowledge.feedback",
            json!({"section_signals":{"formalism":"wrong"}}),
        )
        .await
        .unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let event = runtime
        .events(&token)
        .unwrap()
        .get_event(section["event_id"].as_str().unwrap().parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(event.target_id.is_none());
    assert!(event.payload["signal"].is_null());
    assert_eq!(event.payload["section_signals"]["formalism"], "wrong");
}

#[tokio::test]
async fn wrong_substrate_is_not_found_and_brain_auto_feedback_stays_kg_only() {
    let (_dir, runtime, registry) = fixture(Some("corpus-feedback"), None, true);
    let atom = make_atom(&registry, "local").await;
    let note = registry
        .dispatch(
            "create",
            json!({"kind":"observation", "content":"Corpus feedback refuses a KG note target."}),
        )
        .await
        .unwrap();
    let id = note["id"].as_str().unwrap();
    for input in [id.to_owned(), id.replace('-', "")[..8].to_owned()] {
        let error = registry
            .dispatch(
                "knowledge.feedback",
                json!({"target_id":input,"signal":"wrong"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, khive_runtime::RuntimeError::Khive(error) if error.kind() == khive_types::ErrorKind::NotFound),
            "{error:?}"
        );
    }
    assert_eq!(knowledge_event_count(&runtime).await, 0);
    let error = registry.dispatch("brain.auto_feedback", json!({
        "query":"corpus", "results":[{"id":atom,"full_id":atom}], "target_id":atom,"signal":"useful"
    })).await.unwrap_err();
    assert!(
        matches!(
            &error,
            khive_runtime::RuntimeError::NotFound(_) | khive_runtime::RuntimeError::Khive(_)
        ),
        "{error:?}"
    );
    assert!(
        error.to_string().to_lowercase().contains("not found"),
        "{error}"
    );
    // Positive control: the same atom is a valid knowledge judgment target.
    registry
        .dispatch(
            "knowledge.feedback",
            json!({"target_id":atom,"signal":"useful"}),
        )
        .await
        .unwrap();
    assert_eq!(knowledge_event_count(&runtime).await, 1);
}

#[tokio::test]
async fn invalid_judgments_and_anonymous_or_local_training_leave_no_knowledge_event() {
    let (_dir, runtime, registry) = fixture(Some("corpus-feedback"), None, false);
    let atom = make_atom(&registry, "local").await;
    for params in [
        json!({}),
        json!({"target_id":atom}),
        json!({"signal":"useful"}),
        json!({"target_id":atom,"signal":"great"}),
        json!({"section_signals":{}}),
        json!({"section_signals":{"bogus":"useful"}}),
        json!({"target_id":"feedback-target-atom","signal":"useful"}),
    ] {
        assert!(registry
            .dispatch("knowledge.feedback", params)
            .await
            .is_err());
    }
    assert_eq!(knowledge_event_count(&runtime).await, 0);
    // The anonymous actor and the configured "local" pool are both unattributed.
    for actor in [None, Some("local")] {
        for profile in [None, Some("balanced-recall-v1")] {
            let (_dir, runtime, registry) = fixture(actor, profile, true);
            let atom = make_atom(&registry, "local").await;
            let error = registry
                .dispatch(
                    "knowledge.feedback",
                    json!({"target_id":atom,"section_signals":{"overview":"useful"}}),
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("attributed caller"),
                "{actor:?}: {error}"
            );
            assert_eq!(knowledge_event_count(&runtime).await, 0);
        }
    }
}

struct RefusingSectionBrain;
impl khive_types::Pack for RefusingSectionBrain {
    const NAME: &'static str = "brain";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_types::HandlerDef] = &[khive_types::HandlerDef {
        name: "brain.profile",
        description: "Profile preflight fixture",
        visibility: khive_types::Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[],
    }];
}
#[async_trait::async_trait]
impl khive_runtime::PackRuntime for RefusingSectionBrain {
    fn name(&self) -> &str {
        "brain"
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [khive_types::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }
    async fn dispatch(
        &self,
        _: &str,
        _: serde_json::Value,
        _: &khive_runtime::VerbRegistry,
        _: &khive_runtime::NamespaceToken,
    ) -> Result<serde_json::Value, khive_runtime::RuntimeError> {
        Ok(json!({"lifecycle":"active"}))
    }
    async fn apply_profile_section_feedback(
        &self,
        _: &khive_runtime::NamespaceToken,
        _: &str,
        _: serde_json::Value,
        _: Option<String>,
    ) -> Result<serde_json::Value, khive_runtime::RuntimeError> {
        Err(khive_runtime::RuntimeError::Internal(
            "injected profile persistence failure".into(),
        ))
    }
}

#[tokio::test]
async fn later_profile_failure_reports_the_already_committed_knowledge_event() {
    let runtime = make_rt(Some("profile-fixture".into()), false);
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(runtime.config().actor_id.clone());
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    builder.register(RefusingSectionBrain);
    let registry = builder.build().unwrap();
    let atom = make_atom(&registry, "local").await;
    let error = registry
        .dispatch(
            "knowledge.feedback",
            json!({"target_id":atom,"signal":"wrong","section_signals":{"overview":"wrong"}}),
        )
        .await
        .unwrap_err();
    let khive_runtime::RuntimeError::Khive(error) = error else {
        panic!("expected typed partial-commit error: {error:?}")
    };
    assert_eq!(error.kind(), khive_types::ErrorKind::Internal);
    let details = serde_json::to_value(error.details().unwrap()).unwrap();
    assert_eq!(details["knowledge_event_disposition"], "committed");
    assert_eq!(details["profile_update_disposition"], "unknown");
    let token = runtime.authorize(Namespace::local()).unwrap();
    let event = runtime
        .events(&token)
        .unwrap()
        .get_event(
            details["knowledge_event_id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.payload["signal"], "wrong");
    assert_eq!(event.target_id.unwrap().to_string(), atom);
    assert_eq!(knowledge_event_count(&runtime).await, 1);
}

#[tokio::test]
async fn supplied_profile_precedes_config_and_stays_in_the_request_namespace() {
    let (_dir, runtime, registry) =
        fixture(Some("corpus-feedback"), Some("unused-config-profile"), true);
    let namespace = "knowledge-test-arm";
    registry.dispatch("brain.create_profile", json!({"namespace":namespace,"name":"served-in-arm","consumer_kind":"knowledge_compose"})).await.unwrap();
    registry
        .dispatch(
            "brain.activate",
            json!({"namespace":namespace,"profile_id":"served-in-arm"}),
        )
        .await
        .unwrap();
    let domain = make_domain(&registry, namespace).await;
    let response = registry.dispatch("knowledge.feedback", json!({
        "namespace":namespace,"target_id":domain,"served_by_profile_id":"served-in-arm","section_signals":{"overview":"useful"}
    })).await.unwrap();
    assert_eq!(response["brain_profile"], "served-in-arm");
    let profile = registry
        .dispatch(
            "brain.profile",
            json!({"namespace":namespace,"profile_id":"served-in-arm"}),
        )
        .await
        .unwrap();
    assert_eq!(profile["section_posteriors"]["overview"]["alpha"], 3.0);
    let token = runtime
        .authorize(Namespace::parse(namespace).unwrap())
        .unwrap();
    for key in ["event_id", "profile_event_id"] {
        let event = runtime
            .events(&token)
            .unwrap()
            .get_event(response[key].as_str().unwrap().parse().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.namespace, namespace);
    }
}
