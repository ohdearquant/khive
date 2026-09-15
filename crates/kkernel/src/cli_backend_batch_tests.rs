mod backend_batch_witnesses {
    use super::*;
    use std::collections::HashMap;

    fn map_with_first(
        entries: &[(String, Arc<KhiveRuntime>)],
        first: &str,
    ) -> HashMap<String, Arc<KhiveRuntime>> {
        // RandomState cannot be replaced in the production helper's type.
        // Find and record an actual adverse order before testing its behavior.
        for _ in 0..1024 {
            let map: HashMap<_, _> = entries.iter().cloned().collect();
            if map.keys().next().is_some_and(|name| name == first) {
                eprintln!("BACKEND_ORDER_WITNESS first={first}");
                return map;
            }
        }
        panic!("BACKEND_SETUP_FAILED could not construct first={first}");
    }

    fn unused_coverage_config() -> KhiveConfig {
        toml::from_str(
            "[[backends]]\nname = 'main'\nkind = 'memory'\nserved_kinds = ['event']\n\
             [[backends]]\nname = 'unused'\nkind = 'memory'\nserved_kinds = ['note', 'entity']\n",
        )
        .expect("valid TOML")
    }

    fn assert_coverage_diagnostic(error: &anyhow::Error) {
        let text = error.to_string().to_lowercase();
        for required in ["main", "note", "entity"] {
            assert!(text.contains(required), "missing {required}: {text}");
        }
    }

    #[test]
    fn unused_backend_declaration_cannot_supply_registry_coverage() {
        let runtime = Arc::new(KhiveRuntime::memory().expect("memory runtime"));
        let runtimes = HashMap::from([("comm".to_string(), runtime)]);
        let valid = single_main_backend_config(BackendKind::Memory, None);
        let control = coordinator_backend_registry(&runtimes, &valid)
            .expect("omitted kinds conservatively serve searches");
        assert!(control.primary().unwrap().serves(khive_types::SubstrateKind::Note));
        assert!(control.primary().unwrap().serves(khive_types::SubstrateKind::Entity));
        let config = unused_coverage_config();
        config.validate().expect("declared union is complete");
        eprintln!("BACKEND_BASELINE unused_registry controls_passed");
        let error = coordinator_backend_registry(&runtimes, &config)
            .err()
            .expect("unused declarations cannot supply loaded registry coverage");
        assert_coverage_diagnostic(&error);
    }

    #[tokio::test]
    #[serial(config_ledger)]
    async fn production_coordinator_boot_rejects_unloaded_search_coverage() {
        let base = RuntimeConfig {
            db_path: None,
            packs: vec!["comm".to_string(), "kg".to_string()],
            actor_id: Some("backend-config-test".to_string()),
            ..RuntimeConfig::no_embeddings()
        };
        let valid = single_main_backend_config(BackendKind::Memory, None);
        let control = build_multi_backend_server_with_coordinator_and_db_anchor(
            base.clone(), &valid, Some(":memory:"), None,
        )
        .await
        .expect("production coordinator boot control");
        drop(control);
        let config = unused_coverage_config();
        config.validate().expect("declared union is complete");
        eprintln!("BACKEND_BASELINE production_boot controls_passed");
        let error = build_multi_backend_server_with_coordinator_and_db_anchor(
            base, &config, Some(":memory:"), None,
        )
        .await
        .err()
        .expect("production boot must reject unloaded search coverage");
        assert_coverage_diagnostic(&error);
    }

    #[test]
    fn main_is_primary_despite_archive_first_iteration() {
        let config: KhiveConfig = toml::from_str(
            "[[backends]]\nname = 'archive'\nkind = 'memory'\nserved_kinds = ['event']\n\
             [[backends]]\nname = 'main'\nkind = 'memory'\n\
             [packs.comm]\nbackend = 'archive'\nno_embed = true\n",
        )
        .unwrap();
        config.validate().expect("event-only secondary is valid");
        let runtime = Arc::new(KhiveRuntime::memory().unwrap());
        let map = map_with_first(
            &[("comm".into(), Arc::clone(&runtime)), ("kg".into(), runtime)],
            "comm",
        );
        let registry = coordinator_backend_registry(&map, &config).unwrap();
        assert_eq!(registry.len(), 2);
        assert!(!registry.get(&BackendId::parse("archive").unwrap()).unwrap()
            .serves(khive_types::SubstrateKind::Note));
        eprintln!("BACKEND_BASELINE main_primary controls_passed");
        assert_eq!(registry.primary().unwrap().id, BackendId::main(), "main must be primary");
    }

    #[test]
    fn equal_model_state_uses_pack_name_tie_break() {
        let config = single_main_backend_config(BackendKind::Memory, None);
        let first = Arc::new(KhiveRuntime::memory().unwrap());
        let second = Arc::new(KhiveRuntime::memory().unwrap());
        assert!(!first.vector_arm_selected() && !second.vector_arm_selected());
        let map = map_with_first(
            &[("kg".into(), Arc::clone(&first)), ("memory".into(), second)],
            "memory",
        );
        let registry = coordinator_backend_registry(&map, &config).unwrap();
        assert_eq!(registry.len(), 1);
        eprintln!("BACKEND_BASELINE pack_tie controls_passed");
        assert!(Arc::ptr_eq(&registry.primary().unwrap().runtime, &first),
            "equal model state must choose lexicographically first pack");
    }

    #[test]
    fn absent_main_uses_first_backend_name() {
        let config: KhiveConfig = toml::from_str(
            "[[backends]]\nname = 'archive'\nkind = 'memory'\n\
             [[backends]]\nname = 'zeta'\nkind = 'memory'\n\
             [packs.comm]\nbackend = 'archive'\n\
             [packs.kg]\nbackend = 'zeta'\n",
        )
        .unwrap();
        config.validate().expect("no pack is routed to implicit main");
        let runtime = Arc::new(KhiveRuntime::memory().unwrap());
        let map = map_with_first(
            &[("comm".into(), Arc::clone(&runtime)), ("kg".into(), runtime)],
            "kg",
        );
        let registry = coordinator_backend_registry(&map, &config).unwrap();
        assert!(registry.get(&BackendId::main()).is_none());
        assert_eq!(registry.ids().iter().map(|id| id.as_str()).collect::<Vec<_>>(),
            vec!["archive", "zeta"]);
        eprintln!("BACKEND_BASELINE absent_main controls_passed");
        assert_eq!(registry.primary().unwrap().id.as_str(), "archive",
            "without main, first loaded backend name must be primary");
    }

    struct BatchEmbedder;

    #[async_trait::async_trait]
    impl lattice_embed::EmbeddingService for BatchEmbedder {
        async fn embed(
            &self, texts: &[String], model: lattice_embed::EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0; model.dimensions()]).collect())
        }

        fn supports_model(&self, model: lattice_embed::EmbeddingModel) -> bool {
            model == lattice_embed::EmbeddingModel::AllMiniLmL6V2
        }

        fn name(&self) -> &'static str { "backend-batch-embedder" }
    }

    #[async_trait::async_trait]
    impl khive_runtime::EmbedderProvider for BatchEmbedder {
        fn name(&self) -> &str { "all-minilm-l6-v2" }

        fn dimensions(&self) -> usize {
            lattice_embed::EmbeddingModel::AllMiniLmL6V2.dimensions()
        }

        async fn build(
            &self,
        ) -> khive_runtime::RuntimeResult<Arc<dyn lattice_embed::EmbeddingService>> {
            Ok(Arc::new(Self))
        }
    }

    async fn mixed_models_witness(extra_only: bool) {
        let model = lattice_embed::EmbeddingModel::AllMiniLmL6V2;
        let mut config = single_main_backend_config(BackendKind::Memory, None);
        config.packs.insert("comm".into(), khive_runtime::PackConfig {
            backend: "main".into(), no_embed: true,
        });
        let multi = khive_mcp::serve::build_registry_for_multi_backend(
            RuntimeConfig {
                db_path: None,
                embedding_model: (!extra_only).then_some(model),
                additional_embedding_models: if extra_only { vec![model] } else { vec![] },
                packs: vec!["comm".into(), "kg".into()],
                actor_id: Some("backend-config-test".into()),
                ..RuntimeConfig::no_embeddings()
            }, &config, Some(":memory:"),
        ).await.expect("mixed runtimes share a real backend");
        let embedded = &multi.per_pack_runtimes["kg"];
        let text_only = &multi.per_pack_runtimes["comm"];
        embedded.register_embedder(BatchEmbedder);
        assert_eq!(embedded.config().embedding_model, (!extra_only).then_some(model));
        assert_eq!(embedded.config().additional_embedding_models, if extra_only { vec![model] } else { vec![] });
        assert!(text_only.config().embedding_model.is_none());
        assert!(text_only.config().additional_embedding_models.is_empty());
        let namespace = khive_runtime::Namespace::local();
        let token = embedded.authorize(namespace.clone()).unwrap();
        let note = embedded.create_note(
            &token, "observation", Some("vector candidate"), "stored content",
            None, None, vec![],
        ).await.expect("write candidate through model-bearing pack");
        let token = text_only.authorize(namespace.clone()).unwrap();
        text_only.create_note(
            &token, "observation", Some("text candidate"), "other content",
            None, None, vec![],
        ).await.expect("write through opt-out pack");
        let request = khive_pack_kg::handlers::ValidatedSearchRequest::from_value(
            serde_json::json!({"kind": "note", "query": "unmatchedneedle", "limit": 10}),
            &multi.registry,
        ).unwrap();
        let (_, control_hits, control_outcomes) = SubstrateCoordinator::single(Arc::clone(text_only))
            .fan_out_search(&request, &namespace).await;
        assert!(control_outcomes.iter().all(|o| o.error.is_none() && o.vector_error.is_none()));
        assert!(control_hits.is_empty(), "text-only control cannot find unmatched text");
        let (_, direct_hits, direct_outcomes) = SubstrateCoordinator::single(Arc::clone(embedded))
            .fan_out_search(&request, &namespace).await;
        assert!(direct_outcomes.iter().all(|o| o.error.is_none() && o.vector_error.is_none()));
        if extra_only {
            assert!(!embedded.vector_arm_selected(), "default-model search policy is unchanged");
            assert!(direct_hits.is_empty(), "extra-only does not activate the default vector arm");
        } else {
            assert_eq!(direct_hits.len(), 1, "direct vector positive control");
            assert_eq!(direct_hits[0].note_id, note.id);
            assert_eq!(direct_hits[0].source, khive_runtime::SearchSource::Vector);
        }
        let map = map_with_first(
            &[("comm".into(), Arc::clone(text_only)), ("kg".into(), Arc::clone(embedded))],
            "comm",
        );
        let registry = coordinator_backend_registry(&map, &config).unwrap();
        assert_eq!(registry.len(), 1);
        eprintln!("BACKEND_BASELINE mixed_models extra_only={extra_only} controls_passed");
        assert!(Arc::ptr_eq(&registry.primary().unwrap().runtime, embedded),
            "model-bearing runtime must survive text-first iteration");
        let (_, hits, outcomes) = SubstrateCoordinator::new(registry)
            .fan_out_search(&request, &namespace).await;
        assert!(outcomes.iter().all(|o| o.error.is_none() && o.vector_error.is_none()));
        if extra_only {
            assert!(hits.is_empty());
        } else {
            assert_eq!(hits.len(), 1, "opt-out write must not contribute a vector row");
            assert_eq!(hits[0].note_id, note.id);
            assert_eq!(hits[0].source, khive_runtime::SearchSource::Vector);
        }
        assert!(!text_only.vector_arm_selected(), "pack write settings remain unchanged");
        assert!(text_only.config().additional_embedding_models.is_empty());
    }

    #[tokio::test]
    #[serial(config_ledger)]
    async fn default_model_survives_text_first_shared_backend() {
        mixed_models_witness(false).await;
    }

    #[tokio::test]
    #[serial(config_ledger)]
    async fn extra_model_only_survives_text_first_shared_backend() {
        mixed_models_witness(true).await;
    }

    #[tokio::test]
    #[serial(config_ledger)]
    async fn all_opt_out_packs_remain_text_only() {
        let mut config = single_main_backend_config(BackendKind::Memory, None);
        for pack in ["comm", "kg"] {
            config.packs.insert(pack.into(), khive_runtime::PackConfig {
                backend: "main".into(), no_embed: true,
            });
        }
        let multi = khive_mcp::serve::build_registry_for_multi_backend(
            RuntimeConfig {
                db_path: None,
                embedding_model: Some(lattice_embed::EmbeddingModel::AllMiniLmL6V2),
                packs: vec!["comm".into(), "kg".into()],
                actor_id: Some("backend-config-test".into()),
                ..RuntimeConfig::no_embeddings()
            }, &config, Some(":memory:"),
        ).await.unwrap();
        for runtime in multi.per_pack_runtimes.values() {
            assert!(!runtime.vector_arm_selected());
            assert!(runtime.config().additional_embedding_models.is_empty());
        }
        let registry = coordinator_backend_registry(&multi.per_pack_runtimes, &config).unwrap();
        let selected = Arc::clone(&registry.primary().unwrap().runtime);
        assert!(!selected.vector_arm_selected());
        let namespace = khive_runtime::Namespace::local();
        let token = selected.authorize(namespace.clone()).unwrap();
        let note = selected.create_note(
            &token, "observation", None, "ordinary searchable content", None, None, vec![],
        ).await.unwrap();
        let request = khive_pack_kg::handlers::ValidatedSearchRequest::from_value(
            serde_json::json!({"kind": "note", "query": "searchable", "limit": 10}),
            &multi.registry,
        ).unwrap();
        let (_, hits, outcomes) = SubstrateCoordinator::new(registry)
            .fan_out_search(&request, &namespace).await;
        assert!(outcomes.iter().all(|o| o.error.is_none() && o.vector_error.is_none()));
        assert_eq!(hits.len(), 1, "all-opt-out retains actual text search");
        assert_eq!(hits[0].note_id, note.id);
        assert_ne!(hits[0].source, khive_runtime::SearchSource::Vector);
    }
}
