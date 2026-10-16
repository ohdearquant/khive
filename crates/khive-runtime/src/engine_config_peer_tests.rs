mod ordered_peer_engines {
    use super::*;

    const MINILM: &str = "all-minilm-l6-v2";
    const MULTILINGUAL: &str = "paraphrase-multilingual-minilm-l12-v2";
    const BGE: &str = "bge-small-en-v1.5";

    fn load_peers(source: &str) -> Result<KhiveConfig, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, source);
        KhiveConfig::load(Some(&path)).map(|config| config.expect("config file exists"))
    }

    fn names(engines: &[EngineConfig]) -> Vec<&str> {
        engines.iter().map(|engine| engine.name.as_str()).collect()
    }

    #[test]
    fn three_peers_without_default_reach_runtime_in_declaration_order() {
        let parsed = load_peers(&format!(
            "[[engines]]\nname = {BGE:?}\n\n\
             [[engines]]\nname = {MINILM:?}\nweight = 1.0\n\n\
             [[engines]]\nname = {MULTILINGUAL:?}\ndims = 384\n"
        ))
        .unwrap();
        assert!(parsed.engines_declared);
        assert_eq!(names(&parsed.engines), [BGE, MINILM, MULTILINGUAL]);
        assert!(parsed.engines.iter().all(|engine| engine.weight == 1.0));
        assert_eq!(parsed.engines[2].dims, Some(384));

        let runtime = crate::runtime_config_from_khive_config(&parsed, in_memory_runtime_config());
        let peers = runtime.engines.as_ref().expect("ordered peer authority");
        assert_eq!(names(peers), [BGE, MINILM, MULTILINGUAL]);
        assert_eq!(peers[2].dims, Some(384));
    }

    #[test]
    fn nonpositive_and_nonfinite_weights_are_invalid_before_activation_check() {
        for weight in ["0.0", "-1.0", "nan", "+inf", "-inf"] {
            let error = load_peers(&format!(
                "[[engines]]\nname = {MINILM:?}\nweight = {weight}\n"
            ))
            .expect_err("invalid weight must not load");
            assert!(
                matches!(
                    config_error_root(&error),
                    ConfigError::InvalidEngineWeight { name, .. } if name == MINILM
                ),
                "weight {weight}: {error:?}"
            );
        }
    }

    #[test]
    fn positive_nonunit_weights_remain_refused_until_every_reader_consumes_them() {
        for weight in ["0.25", "2.0", "1e300"] {
            let error = load_peers(&format!(
                "[[engines]]\nname = {MINILM:?}\nweight = {weight}\n"
            ))
            .expect_err("unused positive weights must not silently load");
            assert!(
                matches!(
                    config_error_root(&error),
                    ConfigError::UnsupportedEngineWeight { name } if name == MINILM
                ),
                "weight {weight}: {error:?}"
            );
            assert!(
                error.to_string().contains("every activated retrieval path"),
                "the diagnostic must name the activation condition: {error}"
            );
        }
    }

    #[test]
    fn duplicate_names_and_builtin_alias_collisions_have_distinct_errors() {
        let duplicate = load_peers(&format!(
            "[[engines]]\nname = {MINILM:?}\n\n[[engines]]\nname = {MINILM:?}\n"
        ))
        .expect_err("duplicate names must not collapse");
        assert!(matches!(
            config_error_root(&duplicate),
            ConfigError::DuplicateName { name } if name == MINILM
        ));

        let alias = load_peers(&format!(
            "[[engines]]\nname = \"paraphrase\"\n\n\
             [[engines]]\nname = {MULTILINGUAL:?}\n"
        ))
        .expect_err("aliases of one provider must not form two peers");
        assert!(
            matches!(
                config_error_root(&alias),
                ConfigError::AliasCollision { canonical, .. } if canonical == MULTILINGUAL
            ),
            "{alias:?}"
        );
    }

    #[test]
    fn distinct_custom_names_cannot_share_a_sanitized_storage_key() {
        let error = load_peers(
            "[[engines]]\nname = \"team-one\"\n\n\
             [[engines]]\nname = \"team_one\"\n",
        )
        .expect_err("different providers cannot silently share an index");
        assert!(
            matches!(
                config_error_root(&error),
                ConfigError::EngineKeyCollision { key, .. } if key == "team_one"
            ),
            "{error:?}"
        );
    }

    #[test]
    fn zero_and_negative_dimensions_have_a_dimension_error() {
        for dims in [0, -1] {
            let error = load_peers(&format!("[[engines]]\nname = {MINILM:?}\ndims = {dims}\n"))
                .expect_err("nonpositive dimensions must not load");
            assert!(
                matches!(
                    config_error_root(&error),
                    ConfigError::InvalidEngineDimensions { name, value }
                        if name == MINILM && *value == dims
                ),
                "dims {dims}: {error:?}"
            );
        }
    }

    #[test]
    fn legacy_nonfirst_default_preserves_canonical_model_index_identity() {
        let parsed = load_peers(
            "[[engines]]\nname = \"decorative-secondary\"\nmodel = \"paraphrase\"\n\n\
             [[engines]]\nname = \"decorative-primary\"\nmodel = \"all_minilm_l6_v2\"\n\
             default = true\nfusion_weight = 1.0\ndims = 384\n",
        )
        .unwrap();
        assert_eq!(names(&parsed.engines), [MINILM, MULTILINGUAL]);
        assert!(parsed.engines.iter().all(|engine| engine.weight == 1.0));
        assert_eq!(parsed.engines[0].dims, Some(384));
        let runtime = crate::runtime_config_from_khive_config(&parsed, in_memory_runtime_config());
        assert_eq!(
            names(runtime.engines.as_ref().expect("converted peers")),
            [MINILM, MULTILINGUAL]
        );
        assert_eq!(
            runtime.embedding_model,
            Some(lattice_embed::EmbeddingModel::AllMiniLmL6V2)
        );
        assert_eq!(
            runtime.additional_embedding_models,
            [lattice_embed::EmbeddingModel::ParaphraseMultilingualMiniLmL12V2]
        );
    }

    #[test]
    fn legacy_aliases_cannot_collapse_distinct_decorative_names() {
        let error = load_peers(&format!(
            "[[engines]]\nname = \"one\"\nmodel = \"paraphrase\"\ndefault = true\n\n\
             [[engines]]\nname = \"two\"\nmodel = {MULTILINGUAL:?}\n"
        ))
        .expect_err("legacy aliases need an explicit migration diagnostic");
        assert!(
            matches!(
                config_error_root(&error),
                ConfigError::AliasCollision { .. }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn legacy_and_canonical_keys_cannot_mix_in_one_entry_or_across_entries() {
        let same_entry = format!(
            "[[engines]]\nname = \"old\"\nmodel = {MINILM:?}\ndefault = true\nweight = 1.0\n"
        );
        let separate_entries = format!(
            "[[engines]]\nname = \"old\"\nmodel = {MINILM:?}\ndefault = true\n\n\
             [[engines]]\nname = {MULTILINGUAL:?}\n"
        );
        for source in [same_entry, separate_entries] {
            let error = load_peers(&source).expect_err("mixed config authorities must fail");
            assert!(
                matches!(
                    config_error_root(&error),
                    ConfigError::EngineKeyConflict { .. }
                ),
                "{error:?}"
            );
        }
    }

    #[test]
    fn unknown_engine_fields_and_direct_legacy_deserialization_fail_loudly() {
        let error = load_peers(&format!(
            "[[engines]]\nname = {MINILM:?}\nunused_setting = true\n"
        ))
        .expect_err("unused engine settings must not disappear");
        assert!(matches!(
            config_error_root(&error),
            ConfigError::Parse { .. }
        ));

        let direct = format!("name = \"old\"\nmodel = {MINILM:?}\ndefault = true\n");
        assert!(
            toml::from_str::<EngineConfig>(&direct).is_err(),
            "legacy conversion must not become a second canonical authority"
        );
    }

    #[test]
    fn explicit_empty_disables_peers_while_absence_preserves_deployment_fallback() {
        let mut base = in_memory_runtime_config();
        base.embedding_model = Some(lattice_embed::EmbeddingModel::AllMiniLmL6V2);
        base.additional_embedding_models =
            vec![lattice_embed::EmbeddingModel::ParaphraseMultilingualMiniLmL12V2];

        let absent = load_peers("").unwrap();
        assert!(!absent.engines_declared);
        let inherited = crate::runtime_config_from_khive_config(&absent, base.clone());
        assert!(inherited.engines.is_none());
        assert_eq!(inherited.embedding_model, base.embedding_model);
        assert_eq!(
            inherited.additional_embedding_models,
            base.additional_embedding_models
        );

        let empty = load_peers("engines = []\n").unwrap();
        assert!(empty.engines_declared);
        let disabled = crate::runtime_config_from_khive_config(&empty, base);
        assert!(disabled.engines.as_ref().is_some_and(Vec::is_empty));
        assert!(disabled.embedding_model.is_none());
        assert!(disabled.additional_embedding_models.is_empty());
    }

    #[test]
    fn custom_name_parses_but_unregistered_provider_refuses_startup() {
        let parsed = load_peers(&format!(
            "[[engines]]\nname = \"test-custom-v1\"\ndims = 12\n\n\
             [[engines]]\nname = {MINILM:?}\n"
        ))
        .expect("syntax validation must not require a lattice enum variant");
        assert_eq!(names(&parsed.engines), ["test-custom-v1", MINILM]);
        let runtime = crate::runtime_config_from_khive_config(&parsed, in_memory_runtime_config());
        let error = crate::KhiveRuntime::new(runtime)
            .err()
            .expect("unregistered custom provider must refuse startup");
        assert!(error.to_string().contains("test-custom-v1"), "{error}");
    }

    #[test]
    fn dimensions_are_checked_before_writable_or_readonly_database_open() {
        let parsed = load_peers(&format!("[[engines]]\nname = {MINILM:?}\ndims = 385\n"))
            .expect("positive dimensions are checked against the bound provider");
        let dir = tempfile::tempdir().unwrap();
        for (label, constructor) in [
            (
                "writable",
                crate::KhiveRuntime::new
                    as fn(crate::RuntimeConfig) -> crate::RuntimeResult<crate::KhiveRuntime>,
            ),
            ("readonly", crate::KhiveRuntime::new_readonly),
        ] {
            let unopened = dir.path().join(label);
            let mut runtime =
                crate::runtime_config_from_khive_config(&parsed, in_memory_runtime_config());
            runtime.db_path = Some(unopened.join("khive.db"));
            runtime.volume_lock_dir = Some(dir.path().join("volume-locks"));
            let error = constructor(runtime)
                .err()
                .expect("provider dimensions must reject a mismatched assertion");
            let message = error.to_string();
            assert!(message.contains(MINILM), "{label}: {message}");
            assert!(message.contains("dimension"), "{label}: {message}");
            assert!(!unopened.exists(), "{label} opened storage before binding");
        }
    }

    #[test]
    fn failed_peer_preparation_leaves_names_and_legacy_projections_unchanged() {
        let mut runtime = in_memory_runtime_config();
        runtime.embedding_model = Some(lattice_embed::EmbeddingModel::AllMiniLmL6V2);
        runtime.additional_embedding_models =
            vec![lattice_embed::EmbeddingModel::ParaphraseMultilingualMiniLmL12V2];
        runtime.engines = Some(vec![
            EngineConfig {
                name: "paraphrase".into(),
                weight: 1.0,
                dims: Some(384),
            },
            EngineConfig {
                name: MINILM.into(),
                weight: 1.0,
                dims: Some(385),
            },
        ]);
        let before = runtime.clone();
        runtime
            .prepare_engines()
            .expect_err("the later peer's dimensions must fail");
        assert_eq!(runtime.engines, before.engines);
        assert_eq!(runtime.embedding_model, before.embedding_model);
        assert_eq!(
            runtime.additional_embedding_models,
            before.additional_embedding_models
        );
    }

    #[test]
    fn preparing_explicit_empty_peers_overrides_stale_legacy_models() {
        let mut runtime = in_memory_runtime_config();
        runtime.embedding_model = Some(lattice_embed::EmbeddingModel::AllMiniLmL6V2);
        runtime.additional_embedding_models =
            vec![lattice_embed::EmbeddingModel::ParaphraseMultilingualMiniLmL12V2];
        runtime.engines = Some(Vec::new());
        runtime.prepare_engines().unwrap();
        assert_eq!(runtime.engines, Some(Vec::new()));
        assert!(runtime.embedding_model.is_none());
        assert!(runtime.additional_embedding_models.is_empty());
    }

    #[test]
    fn metadata_discovery_disables_an_explicit_peer_list() {
        let parsed = load_peers(&format!("[[engines]]\nname = {MINILM:?}\n")).unwrap();
        let runtime = crate::runtime_config_from_khive_config(&parsed, in_memory_runtime_config())
            .for_metadata_registry();
        assert_eq!(runtime.engines, Some(Vec::new()));
        assert!(runtime.embedding_model.is_none());
        assert!(runtime.additional_embedding_models.is_empty());
    }

    #[test]
    fn canonical_serialization_contains_only_peer_fields_after_legacy_conversion() {
        let parsed = load_peers(
            "[[engines]]\nname = \"old-label\"\nmodel = \"paraphrase\"\n\
             default = true\nfusion_weight = 1.0\ndims = 384\n",
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&parsed.engines).unwrap(),
            serde_json::json!([{
                "name": MULTILINGUAL,
                "weight": 1.0,
                "dims": 384
            }])
        );
        let serialized = toml::to_string(&parsed.engines[0]).unwrap();
        let roundtrip: EngineConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(roundtrip, parsed.engines[0]);

        let without_dims = load_peers(&format!("[[engines]]\nname = {MINILM:?}\n")).unwrap();
        assert_eq!(
            serde_json::to_value(&without_dims.engines).unwrap(),
            serde_json::json!([{"name": MINILM, "weight": 1.0}])
        );
    }

    struct DimensionProbeProvider(usize);

    #[async_trait::async_trait]
    impl crate::embedder_registry::EmbedderProvider for DimensionProbeProvider {
        fn name(&self) -> &str {
            MINILM
        }

        fn dimensions(&self) -> usize {
            self.0
        }

        async fn build(
            &self,
        ) -> crate::RuntimeResult<std::sync::Arc<dyn lattice_embed::EmbeddingService>> {
            Err(crate::RuntimeError::Internal(format!(
                "dimension probe provider {}",
                self.0
            )))
        }
    }

    #[tokio::test]
    async fn configured_dimensions_refuse_a_mismatched_cold_provider_replacement() {
        let parsed = load_peers(&format!("[[engines]]\nname = {MINILM:?}\ndims = 384\n")).unwrap();
        let config = crate::runtime_config_from_khive_config(&parsed, in_memory_runtime_config());
        let runtime = crate::KhiveRuntime::new(config).unwrap();
        runtime
            .try_register_embedder(DimensionProbeProvider(384))
            .expect("matching dimensions allow a replacement before serving");
        let before = runtime.registered_embedding_model_names();
        let error = runtime
            .try_register_embedder(DimensionProbeProvider(385))
            .expect_err("cold replacements must satisfy configured dimension assertions");
        let message = error.to_string();
        assert!(message.contains(MINILM), "{message}");
        assert!(message.contains("384"), "{message}");
        assert!(message.contains("385"), "{message}");
        assert_eq!(runtime.registered_embedding_model_names(), before);

        let retained = runtime
            .embedder(MINILM)
            .await
            .err()
            .expect("the retained probe returns a deliberate error without model inference");
        assert!(
            retained
                .to_string()
                .contains("dimension probe provider 384"),
            "the rejected replacement must not overwrite the provider: {retained}"
        );
    }
}
