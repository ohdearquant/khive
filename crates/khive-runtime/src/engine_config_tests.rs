use super::*;

include!("engine_config_timeout_tests.rs");
include!("engine_config_deadline_tests.rs");
include!("engine_config_disk_guard_tests.rs");

fn write_toml(dir: &tempfile::TempDir, content: &str) -> PathBuf {
    let path = dir.path().join("config.toml");
    std::fs::write(&path, content).unwrap();
    path
}

fn in_memory_runtime_config() -> crate::RuntimeConfig {
    crate::RuntimeConfig {
        db_path: None,
        ..crate::RuntimeConfig::no_embeddings()
    }
}

/// Load errors must name the config file they came from (#1892): the
/// validation and I/O errors gain the loader's `InFile` context, while
/// `Parse` keeps carrying its own path. The message suffix is the
/// user-facing contract.
#[test]
fn load_errors_name_the_config_file() {
    let dir = tempfile::tempdir().unwrap();

    let gate = write_toml(&dir, "[gate]\nmode = \"x\"\n");
    let err = KhiveConfig::load(Some(&gate)).expect_err("unknown gate key must fail");
    assert!(
        err.to_string().contains(&gate.display().to_string()),
        "gate error must name the file, got: {err}"
    );

    let invalid = write_toml(
        &dir,
        "[[engines]]\nname = \"a\"\nmodel = \"all-minilm-l6-v2\"\n",
    );
    let err = KhiveConfig::load(Some(&invalid)).expect_err("validation must fail");
    assert!(
        err.to_string().contains("(config file: "),
        "validation error must name the file, got: {err}"
    );

    let parse = write_toml(&dir, "not = = toml");
    let err = KhiveConfig::load(Some(&parse)).expect_err("parse must fail");
    assert!(
        err.to_string().contains("config.toml"),
        "parse error must name the file, got: {err}"
    );
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "parse errors keep their own variant unwrapped, got: {err:?}"
    );
}

/// Unwrap the loader's `InFile` context (asserting it names a real path)
/// so variant-shape assertions test the underlying error.
fn config_error_root(err: &ConfigError) -> &ConfigError {
    match err {
        ConfigError::InFile { path, source } => {
            assert!(
                !path.as_os_str().is_empty(),
                "InFile must carry the config path"
            );
            source
        }
        other => other,
    }
}

include!("engine_config_env_additional_tests.rs");

#[test]
fn test_load_minimal_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[engines]]
name = "x"
model = "all-minilm-l6-v2"
default = true
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file should be found");
    assert_eq!(cfg.engines.len(), 1);
    assert_eq!(cfg.engines[0].name, "x");
    assert_eq!(cfg.engines[0].model, "all-minilm-l6-v2");
    assert!(cfg.engines[0].default);
}

#[test]
fn test_unknown_engine_model_rejected_before_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        "[[engines]]\nname = \"primary\"\nmodel = \"not-a-model\"\ndefault = true\n",
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("unknown primary model must fail");
    assert!(
        matches!(
            config_error_root(&err),
            ConfigError::UnknownModel { name, model }
                if name == "primary" && model == "not-a-model"
        ),
        "expected UnknownModel for the primary engine, got {err:?}"
    );

    let config: KhiveConfig = toml::from_str(
            "[[engines]]\nname = \"primary\"\nmodel = \"all-minilm-l6-v2\"\ndefault = true\n\n[[engines]]\nname = \"secondary\"\nmodel = \"not-a-model\"\n",
        )
        .unwrap();
    assert!(matches!(
        config.validate(),
        Err(ConfigError::UnknownModel { name, model })
            if name == "secondary" && model == "not-a-model"
    ));
}

#[test]
fn test_recognized_engine_model_validates_and_converts() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        "[[engines]]\nname = \"primary\"\nmodel = \"all-minilm-l6-v2\"\ndefault = true\n",
    );
    let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
    config.validate().unwrap();
    let runtime = crate::runtime_config_from_khive_config(&config, in_memory_runtime_config());
    assert_eq!(
        runtime.embedding_model,
        Some(lattice_embed::EmbeddingModel::AllMiniLmL6V2)
    );
}

#[test]
fn test_default_engine_required_when_engines_present() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[engines]]
name = "a"
model = "all-minilm-l6-v2"
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("should fail with no default flagged");
    assert!(
        matches!(
            config_error_root(&err),
            ConfigError::DefaultCount { found: 0 }
        ),
        "expected DefaultCount {{ found: 0 }}, got {err:?}"
    );
}

#[test]
fn test_multiple_default_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[engines]]
name = "a"
model = "all-minilm-l6-v2"
default = true

[[engines]]
name = "b"
model = "paraphrase-multilingual-minilm-l12-v2"
default = true
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("should fail with two defaults");
    assert!(
        matches!(
            config_error_root(&err),
            ConfigError::DefaultCount { found: 2 }
        ),
        "expected DefaultCount {{ found: 2 }}, got {err:?}"
    );
}

#[test]
fn test_fusion_weight_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[engines]]
name = "a"
model = "all-minilm-l6-v2"
default = true
fusion_weight = -0.5
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("should fail with negative fusion_weight");
    assert!(
        matches!(
            config_error_root(&err),
            ConfigError::InvalidFusionWeight { .. }
        ),
        "expected InvalidFusionWeight, got {err:?}"
    );

    let path2 = write_toml(
        &dir,
        r#"
[[engines]]
name = "a"
model = "all-minilm-l6-v2"
default = true
fusion_weight = 0.0
"#,
    );
    let err2 = KhiveConfig::load(Some(&path2)).expect_err("should fail with zero fusion_weight");
    assert!(
        matches!(
            config_error_root(&err2),
            ConfigError::InvalidFusionWeight { .. }
        ),
        "expected InvalidFusionWeight, got {err2:?}"
    );
}

#[test]
fn test_env_var_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("missing.toml");

    let loaded = KhiveConfig::load(Some(&absent)).unwrap();
    assert!(loaded.is_none());

    // Can't safely set env vars in a parallel test suite, so exercise the
    // direct construction path instead.
    let primary = "all-minilm-l6-v2".to_string();
    let additional = vec!["paraphrase-multilingual-minilm-l12-v2".to_string()];

    let mut engines = vec![EngineConfig {
        name: "default".to_string(),
        model: primary,
        default: true,
        fusion_weight: None,
        dims: None,
    }];
    for (i, model) in additional.into_iter().enumerate() {
        engines.push(EngineConfig {
            name: format!("engine-{}", i + 1),
            model,
            default: false,
            fusion_weight: None,
            dims: None,
        });
    }
    let cfg = KhiveConfig {
        engines,
        ..KhiveConfig::default()
    };
    cfg.validate().expect("env-derived config should be valid");
    assert_eq!(cfg.engines.len(), 2);
    assert!(cfg.default_engine().is_some());
    assert_eq!(cfg.default_engine().unwrap().name, "default");
}

#[test]
fn test_file_overrides_env() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[engines]]
name = "file-engine"
model = "all-minilm-l6-v2"
default = true
"#,
    );

    // KhiveConfig::load returns the file config regardless of env vars;
    // warning-on-conflict is the caller's responsibility.
    let cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file should be present");
    assert_eq!(cfg.engines[0].name, "file-engine");
}

#[test]
fn test_duplicate_engine_names_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[engines]]
name = "shared"
model = "all-minilm-l6-v2"
default = true

[[engines]]
name = "shared"
model = "paraphrase-multilingual-minilm-l12-v2"
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("should fail with duplicate name");
    assert!(
        matches!(config_error_root(&err), ConfigError::DuplicateName { .. }),
        "expected DuplicateName, got {err:?}"
    );
}

#[test]
fn test_empty_config_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "# no engines\n");
    let cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file should be found");
    assert!(cfg.engines.is_empty());
    cfg.validate().expect("empty config should be valid");
}

#[test]
fn runtime_blob_hydration_budget_parses_and_resolves_before_engine_early_return() {
    use crate::runtime::runtime_config_from_khive_config;
    use crate::RuntimeConfig;

    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[runtime]
blob_hydration_bytes = 134217728
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file should be found");

    assert_eq!(cfg.runtime.blob_hydration_bytes, Some(134_217_728));
    let resolved = runtime_config_from_khive_config(&cfg, RuntimeConfig::default());
    assert_eq!(resolved.blob_hydration_bytes, 134_217_728);
}

#[test]
fn runtime_blob_hydration_budget_below_one_whole_blob_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let value = khive_storage::MAX_BLOB_WHOLE_BYTES - 1;
    let path = write_toml(
        &dir,
        &format!("[runtime]\nblob_hydration_bytes = {value}\n"),
    );

    let err = KhiveConfig::load(Some(&path)).expect_err("undersized budget must fail closed");
    assert!(
        matches!(
            config_error_root(&err),
            ConfigError::InvalidBlobHydrationBytes {
                value: actual,
                min,
                ..
            } if *actual == value && *min == khive_storage::MAX_BLOB_WHOLE_BYTES
        ),
        "got {err:?}"
    );
}

#[test]
fn runtime_blob_hydration_budget_accepts_the_inclusive_portable_minimum() {
    let dir = tempfile::tempdir().unwrap();
    let value = khive_storage::MAX_BLOB_WHOLE_BYTES;
    let path = write_toml(
        &dir,
        &format!("[runtime]\nblob_hydration_bytes = {value}\n"),
    );

    let cfg = KhiveConfig::load(Some(&path))
        .expect("the inclusive minimum must be valid")
        .expect("config should exist");
    assert_eq!(cfg.runtime.blob_hydration_bytes, Some(value));
}

#[test]
fn runtime_blob_hydration_budget_above_semaphore_capacity_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let max = tokio::sync::Semaphore::MAX_PERMITS as u64;
    let value = max
        .checked_add(1)
        .expect("tokio maximum fits below u64::MAX");
    let path = write_toml(
        &dir,
        &format!("[runtime]\nblob_hydration_bytes = {value}\n"),
    );

    let err = KhiveConfig::load(Some(&path)).expect_err("oversized budget must fail closed");
    assert!(
        matches!(
            config_error_root(&err),
            ConfigError::InvalidBlobHydrationBytes {
                value: actual,
                max: actual_max,
                ..
            } if *actual == value && *actual_max == max
        ),
        "got {err:?}"
    );
}

#[test]
fn configured_fusion_weight_is_refused_instead_of_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[engines]]
name = "primary"
model = "all-minilm-l6-v2"
default = true
fusion_weight = 0.7

[[engines]]
name = "secondary"
model = "paraphrase-multilingual-minilm-l12-v2"
fusion_weight = 0.3
"#,
    );
    let err = KhiveConfig::load(Some(&path))
        .expect_err("an explicit fusion weight must not be silently ignored");
    assert!(
        matches!(
            config_error_root(&err),
            ConfigError::UnsupportedFusionWeight { name } if name == "primary"
        ),
        "expected UnsupportedFusionWeight for primary, got {err:?}"
    );

    let unweighted_path = write_toml(
        &dir,
        r#"
[[engines]]
name = "primary"
model = "all-minilm-l6-v2"
default = true

[[engines]]
name = "secondary"
model = "paraphrase-multilingual-minilm-l12-v2"
"#,
    );
    let cfg = KhiveConfig::load(Some(&unweighted_path))
        .expect("unweighted multi-engine config remains valid")
        .expect("file should be found");
    assert_eq!(cfg.engines.len(), 2);
    assert!(cfg
        .engines
        .iter()
        .all(|engine| engine.fusion_weight.is_none()));
}

#[test]
fn test_actor_id_parsed() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[actor]
id = "lambda:khive"
display_name = "example actor"
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file should be found");
    assert_eq!(cfg.actor.id.as_deref(), Some("lambda:khive"));
    assert_eq!(cfg.actor.display_name.as_deref(), Some("example actor"));
    assert!(cfg.engines.is_empty());
}

#[test]
fn gate_mailbox_reader_config_loads_exact_labels_and_rejects_bad_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
            &dir,
            "[actor]\nid = \"lambda:owner\"\nmailbox_readers = [\"lambda:helper\", \"助手/审阅者\", \"lambda:helper\"]\n",
        );
    let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
    assert_eq!(
        config.actor.mailbox_readers,
        ["lambda:helper", "助手/审阅者", "lambda:helper"]
    );

    for (owner, readers) in [
        (None, vec!["reader".to_string()]),
        (Some("local"), vec!["reader".to_string()]),
        (Some("lambda:owner"), vec!["local".to_string()]),
        (Some("lambda:owner"), vec![String::new()]),
        (Some("lambda:owner"), vec![" \t".to_string()]),
        (Some("lambda:owner"), vec!["bad\nactor".to_string()]),
        (Some("lambda:owner"), vec!["x".repeat(256)]),
        (Some("lambda:owner"), vec!["reader".to_string(); 257]),
    ] {
        let config = KhiveConfig {
            actor: ActorConfig {
                id: owner.map(str::to_string),
                mailbox_readers: readers,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidMailboxReaders { .. })
        ));
    }
    for source in [
        "[actor]\nmailbox_readers = [\"reader\"]\n",
        "[actor]\nid = \"local\"\nmailbox_readers = [\"reader\"]\n",
        "[actor]\nid = \"lambda:owner\"\nmailbox_readers = [\"\"]\n",
        "[actor]\nid = \"lambda:owner\"\nmailbox_readers = \"reader\"\n",
    ] {
        let path = write_toml(&dir, source);
        assert!(KhiveConfig::load(Some(&path)).is_err());
    }
    let boundary = KhiveConfig {
        actor: ActorConfig {
            id: Some("lambda:owner".into()),
            mailbox_readers: vec!["x".repeat(255); 256],
            ..Default::default()
        },
        ..Default::default()
    };
    boundary.validate().unwrap();
    KhiveConfig::default().validate().unwrap();
}

#[test]
fn test_actor_and_engines_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[actor]
id = "lambda:test"

[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file should be found");
    assert_eq!(cfg.actor.id.as_deref(), Some("lambda:test"));
    assert_eq!(cfg.engines.len(), 1);
}

#[test]
fn test_actor_absent_defaults_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[engines]]
name = "x"
model = "all-minilm-l6-v2"
default = true
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file should be found");
    assert!(
        cfg.actor.id.is_none(),
        "actor.id must be None when [actor] section is absent"
    );
}

#[test]
fn test_load_with_home_fallback_no_files() {
    let project_dir = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    let result = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()), None);
    assert!(
        result.expect("no error expected").is_none(),
        "should return None when no config files exist in the given roots"
    );
}

#[test]
fn home_gate_config_loads_while_explicit_empty_config_is_hermetic() {
    let project_dir = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
    std::fs::write(
        home_dir.path().join(".khive/config.toml"),
        "[gate]\ngranted_actors = [\"lambda:enrolled\"]\ndeny_writes_for = [\"*:duty\"]\n",
    )
    .unwrap();

    let loaded = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()), None)
        .expect("supported home gate policy loads")
        .expect("home config exists");
    let gate = loaded.gate.expect("gate table");
    assert_eq!(gate.granted_actors, vec!["lambda:enrolled"]);
    assert_eq!(gate.deny_writes_for, vec!["*:duty"]);

    let empty = project_dir.path().join("empty-khive-config.toml");
    std::fs::write(&empty, "").unwrap();
    let isolated = KhiveConfig::load_with_home_fallback(Some(&empty), None)
        .expect("an explicit empty fixture must isolate config discovery")
        .expect("the explicit config exists");
    assert!(isolated.engines.is_empty());
    assert!(isolated.actor.id.is_none());
    assert!(isolated.gate.is_none());
}

#[test]
fn test_load_with_home_fallback_explicit_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[actor]
id = "lambda:explicit"
"#,
    );
    let cfg = KhiveConfig::load_with_home_fallback(Some(&path), None)
        .expect("no error expected")
        .expect("file found");
    assert_eq!(cfg.actor.id.as_deref(), Some("lambda:explicit"));
}

#[test]
fn load_with_home_fallback_and_source_names_selected_file() {
    let project_dir = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
    let selected = home_dir.path().join(".khive/config.toml");
    std::fs::write(&selected, "[actor]\nid = \"lambda:home\"\n").unwrap();

    let (config, source) =
        KhiveConfig::load_with_roots_and_source(project_dir.path(), Some(home_dir.path()), None)
            .expect("load should succeed")
            .expect("home fallback should be selected");

    assert_eq!(config.actor.id.as_deref(), Some("lambda:home"));
    assert_eq!(
        source,
        std::fs::canonicalize(selected).expect("canonical selected config path")
    );
}

#[test]
fn test_invalid_actor_id_rejected_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[actor]
id = "bad namespace"
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("should fail with invalid actor.id");
    assert!(
        matches!(config_error_root(&err), ConfigError::InvalidActorId { .. }),
        "expected InvalidActorId, got {err:?}"
    );
}

#[test]
fn test_empty_actor_id_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[actor]
id = ""
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("empty actor.id should be rejected");
    assert!(
        matches!(config_error_root(&err), ConfigError::InvalidActorId { .. }),
        "expected InvalidActorId for empty string, got {err:?}"
    );
}

#[test]
fn test_malformed_actor_id_lambda_colon_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[actor]
id = "lambda:"
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("lambda: with no slug should be rejected");
    assert!(
        matches!(config_error_root(&err), ConfigError::InvalidActorId { .. }),
        "expected InvalidActorId for 'lambda:', got {err:?}"
    );
}

// actor.id must not become default_namespace: writes stay pinned to `local`
// even though a non-local actor.id widens the default read visible-set.
#[test]
fn test_runtime_config_actor_id_does_not_override_namespace() {
    use crate::runtime::runtime_config_from_khive_config;
    use crate::RuntimeConfig;
    use khive_types::namespace::Namespace;

    let cfg = KhiveConfig {
        engines: vec![],
        actor: ActorConfig {
            id: Some("lambda:test-actor".to_string()),
            display_name: None,
            ..Default::default()
        },
        ..KhiveConfig::default()
    };
    cfg.validate().expect("valid config");

    let base = RuntimeConfig::default();
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.default_namespace,
        Namespace::local(),
        "actor.id must NOT become default_namespace (ADR-007 Rev 4 Rule 0); \
             writes stay pinned to local"
    );
    // actor.id must also appear in visible_namespaces: the load-bearing
    // side effect that widens default reads to {local} ∪ {actor namespace}.
    assert!(
        result
            .visible_namespaces
            .contains(&Namespace::parse("lambda:test-actor").unwrap()),
        "actor.id must be folded into visible_namespaces (ADR-007 Rev 4 Rule 3b fold-in); \
             got: {:?}",
        result.visible_namespaces
    );
}

#[test]
fn test_runtime_config_no_actor_preserves_base() {
    use crate::runtime::runtime_config_from_khive_config;
    use crate::RuntimeConfig;
    use khive_types::namespace::Namespace;

    let cfg = KhiveConfig {
        engines: vec![],
        actor: ActorConfig {
            id: None,
            display_name: None,
            ..Default::default()
        },
        ..KhiveConfig::default()
    };
    cfg.validate().expect("valid config");

    let base_ns = Namespace::parse("lambda:base").unwrap();
    let base = RuntimeConfig {
        default_namespace: base_ns.clone(),
        ..RuntimeConfig::default()
    };
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.default_namespace, base_ns,
        "no actor.id must leave base namespace unchanged"
    );
}

#[test]
fn test_load_with_home_fallback_project_root_over_hidden() {
    let dir = tempfile::tempdir().unwrap();

    // Write .khive/config.toml (tier 3).
    std::fs::create_dir_all(dir.path().join(".khive")).unwrap();
    std::fs::write(
        dir.path().join(".khive/config.toml"),
        "[actor]\nid = \"lambda:hidden\"\n",
    )
    .unwrap();

    // Write khive.toml (tier 2) — should win.
    std::fs::write(
        dir.path().join("khive.toml"),
        "[actor]\nid = \"lambda:project-root\"\n",
    )
    .unwrap();

    let cfg = KhiveConfig::load_with_roots(dir.path(), None, None)
        .expect("no error expected")
        .expect("file should be found");
    assert_eq!(
        cfg.actor.id.as_deref(),
        Some("lambda:project-root"),
        "khive.toml (tier 2) must win over .khive/config.toml (tier 3)"
    );
}

#[test]
fn test_load_with_home_fallback_hidden_over_absent_root() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::create_dir_all(dir.path().join(".khive")).unwrap();
    std::fs::write(
        dir.path().join(".khive/config.toml"),
        "[actor]\nid = \"lambda:hidden-config\"\n",
    )
    .unwrap();
    // No khive.toml.

    let cfg = KhiveConfig::load_with_roots(dir.path(), None, None)
        .expect("no error expected")
        .expect("file should be found");
    assert_eq!(
        cfg.actor.id.as_deref(),
        Some("lambda:hidden-config"),
        ".khive/config.toml (tier 3) must be found when khive.toml is absent"
    );
}

#[test]
fn test_load_with_roots_home_tier_found() {
    let project_dir = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();

    std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
    std::fs::write(
        home_dir.path().join(".khive/config.toml"),
        "[actor]\nid = \"lambda:user-global\"\n",
    )
    .unwrap();
    // No project-level files.

    let cfg = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()), None)
        .expect("no error expected")
        .expect("file should be found");
    assert_eq!(
        cfg.actor.id.as_deref(),
        Some("lambda:user-global"),
        "~/.khive/config.toml (tier 4) must be found when project files absent"
    );
}

#[test]
fn test_load_with_roots_project_wins_over_home() {
    let project_dir = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();

    // Home has a config.
    std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
    std::fs::write(
        home_dir.path().join(".khive/config.toml"),
        "[actor]\nid = \"lambda:user-global\"\n",
    )
    .unwrap();

    // Project also has a config — should win.
    std::fs::create_dir_all(project_dir.path().join(".khive")).unwrap();
    std::fs::write(
        project_dir.path().join(".khive/config.toml"),
        "[actor]\nid = \"lambda:project-wins\"\n",
    )
    .unwrap();

    let cfg = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()), None)
        .expect("no error expected")
        .expect("file should be found");
    assert_eq!(
        cfg.actor.id.as_deref(),
        Some("lambda:project-wins"),
        "project .khive/config.toml (tier 3) must win over ~/.khive/config.toml (tier 4)"
    );
}

// ── tier-3 db-dir anchor tests (config discovery canonicalization) ─────

// Two different process working directories, targeting the same database,
// must resolve the identical tier-3 config file. Each cwd also carries its
// own decoy `.khive/config.toml` so the test fails loudly (mismatched
// actor ids) if the resolver ever falls back to the old cwd anchor instead
// of the db-dir anchor.
#[test]
fn test_load_with_roots_same_db_different_cwd_resolves_identical_config() {
    let cwd_a = tempfile::tempdir().unwrap();
    let cwd_b = tempfile::tempdir().unwrap();

    // Decoy cwd-anchored configs — must NOT be picked up once anchoring
    // moves to the db directory.
    std::fs::create_dir_all(cwd_a.path().join(".khive")).unwrap();
    std::fs::write(
        cwd_a.path().join(".khive/config.toml"),
        "[actor]\nid = \"lambda:wrong-cwd-a\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(cwd_b.path().join(".khive")).unwrap();
    std::fs::write(
        cwd_b.path().join(".khive/config.toml"),
        "[actor]\nid = \"lambda:wrong-cwd-b\"\n",
    )
    .unwrap();

    // The database and its co-located config live under a THIRD root,
    // distinct from either simulated cwd.
    let db_root = tempfile::tempdir().unwrap();
    let khive_dir = db_root.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).unwrap();
    let db_path = khive_dir.join("khive.db");
    std::fs::write(&db_path, b"").unwrap(); // must exist for canonicalize to succeed
    std::fs::write(
        khive_dir.join("config.toml"),
        "[actor]\nid = \"lambda:db-anchored\"\n",
    )
    .unwrap();

    let cfg_a = KhiveConfig::load_with_roots(cwd_a.path(), None, Some(&db_path))
        .expect("no error expected")
        .expect("db-anchored config must be found from cwd A");
    let cfg_b = KhiveConfig::load_with_roots(cwd_b.path(), None, Some(&db_path))
        .expect("no error expected")
        .expect("db-anchored config must be found from cwd B");

    assert_eq!(
        cfg_a.actor.id.as_deref(),
        Some("lambda:db-anchored"),
        "cwd A must resolve the db-anchored config, not its own decoy"
    );
    assert_eq!(
        cfg_b.actor.id.as_deref(),
        Some("lambda:db-anchored"),
        "cwd B must resolve the db-anchored config, not its own decoy"
    );
    assert_eq!(
        cfg_a.actor.id, cfg_b.actor.id,
        "two processes at different cwds targeting the same db must resolve \
             identical config, killing config_id drift between client and daemon"
    );
}

// Explicit `--config`/`KHIVE_CONFIG` (tier 1) must still win over the new
// db-dir anchor (tier 3) — precedence is preserved, only the tier-3 anchor
// moved.
#[test]
fn test_load_with_home_fallback_explicit_config_wins_over_db_anchor() {
    let explicit_dir = tempfile::tempdir().unwrap();
    let explicit_path = write_toml(&explicit_dir, "[actor]\nid = \"lambda:explicit-wins\"\n");

    let db_root = tempfile::tempdir().unwrap();
    let khive_dir = db_root.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).unwrap();
    let db_path = khive_dir.join("khive.db");
    std::fs::write(&db_path, b"").unwrap();
    std::fs::write(
        khive_dir.join("config.toml"),
        "[actor]\nid = \"lambda:db-anchor-loses\"\n",
    )
    .unwrap();

    let cfg = KhiveConfig::load_with_home_fallback(Some(&explicit_path), Some(&db_path))
        .expect("no error expected")
        .expect("explicit path must be found");
    assert_eq!(
        cfg.actor.id.as_deref(),
        Some("lambda:explicit-wins"),
        "explicit --config/KHIVE_CONFIG must win over the db-dir anchor"
    );
}

// Tier 4 (`~/.khive/config.toml`) must still be reached when the db-anchored
// tier-3 directory has no `config.toml` alongside it.
#[test]
fn test_load_with_roots_home_fallback_reached_when_db_anchor_has_no_config() {
    let cwd = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
    std::fs::write(
        home_dir.path().join(".khive/config.toml"),
        "[actor]\nid = \"lambda:home-fallback\"\n",
    )
    .unwrap();

    // A real db directory that exists but has no co-located config.toml.
    let db_root = tempfile::tempdir().unwrap();
    let khive_dir = db_root.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).unwrap();
    let db_path = khive_dir.join("khive.db");
    std::fs::write(&db_path, b"").unwrap();

    let cfg = KhiveConfig::load_with_roots(cwd.path(), Some(home_dir.path()), Some(&db_path))
        .expect("no error expected")
        .expect("home-tier config must be found");
    assert_eq!(
        cfg.actor.id.as_deref(),
        Some("lambda:home-fallback"),
        "tier 4 (~/.khive/config.toml) must still be reached when the db-anchored \
             tier-3 directory has no config.toml"
    );
}

// Cold start: the database file does not exist yet (first run). Anchor
// resolution must not panic and must fall through the remaining tiers.
#[test]
fn test_load_with_roots_nonexistent_db_path_does_not_panic_and_falls_through() {
    let cwd = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
    std::fs::write(
        home_dir.path().join(".khive/config.toml"),
        "[actor]\nid = \"lambda:home-cold-start\"\n",
    )
    .unwrap();

    // Absolute path under a directory tree that was never created.
    let nonexistent_db = cwd.path().join("never-created/.khive/khive.db");

    let cfg =
        KhiveConfig::load_with_roots(cwd.path(), Some(home_dir.path()), Some(&nonexistent_db))
            .expect("cold-start db path must not error or panic")
            .expect("home-tier config must still be found");
    assert_eq!(
        cfg.actor.id.as_deref(),
        Some("lambda:home-cold-start"),
        "a nonexistent db path (cold start) must fall through to tier 4, not panic"
    );
}

// Cold start with a *relative* nonexistent db path exercises the
// cwd-join fallback branch specifically (as opposed to the
// already-absolute fallback branch above). Must not panic; no config
// exists anywhere so the result is `Ok(None)`.
#[test]
fn test_load_with_roots_relative_nonexistent_db_path_does_not_panic() {
    let cwd = tempfile::tempdir().unwrap();
    let relative_db = PathBuf::from("never-created/.khive/khive.db");

    let result = KhiveConfig::load_with_roots(cwd.path(), None, Some(&relative_db));
    assert!(
        result.is_ok(),
        "relative cold-start db path must not error or panic: {result:?}"
    );
    assert!(
        result.unwrap().is_none(),
        "no config exists anywhere in this test; result must be None"
    );
}

// ── ADR-028 backend / pack config tests ─────────────────────────────────

#[test]
fn test_no_backends_section_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error")
        .expect("file found");
    assert!(cfg.backends.is_empty());
    assert!(cfg.packs.is_empty());
}

#[test]
fn test_single_sqlite_backend_parses() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "knowledge"
kind = "sqlite"
path = "/tmp/knowledge.db"
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error")
        .expect("file found");
    assert_eq!(cfg.backends.len(), 1);
    let b = &cfg.backends[0];
    assert_eq!(b.name, "knowledge");
    assert!(matches!(b.kind, BackendKind::Sqlite));
    assert_eq!(
        b.path.as_ref().and_then(|p| p.to_str()),
        Some("/tmp/knowledge.db")
    );
}

#[test]
fn test_memory_backend_parses() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "ephemeral"
kind = "memory"
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error")
        .expect("file found");
    assert_eq!(cfg.backends.len(), 1);
    assert!(matches!(cfg.backends[0].kind, BackendKind::Memory));
}

#[test]
fn memory_wal_policy_ignores_environment_but_keeps_its_own_field() {
    for raw in ["8192", "abc"] {
        let resolved = resolve_wal_ceiling(
            None,
            Some(raw),
            "ephemeral",
            BackendKind::Memory,
            true,
            false,
        )
        .unwrap();
        assert_eq!(resolved.configured_bytes, 0, "MEMORY_IGNORES_ENVIRONMENT");
        assert_eq!(resolved.source, khive_db::WalCeilingSource::Default);
        let error = resolve_wal_ceiling(
            Some(8192),
            Some(raw),
            "ephemeral",
            BackendKind::Memory,
            true,
            false,
        )
        .expect_err("DECLARED_MEMORY_FIELD_REFUSAL");
        assert!(matches!(
            error,
            ConfigError::WalCeilingMemoryBackend { value: 8192, .. }
        ));
    }
    let error = resolve_wal_ceiling(
        None,
        Some("credential-secret-marker"),
        "file",
        BackendKind::Sqlite,
        true,
        false,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "KHIVE_SQLITE_WAL_CEILING_BYTES must be an unsigned decimal byte count",
        "RAW_WAL_ENVIRONMENT_NOT_ECHOED"
    );
}

#[test]
fn wal_ceiling_nonzero_memory_backend_fails_config_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        "[[backends]]\nname = 'main'\nkind = 'memory'\nwal_ceiling_bytes = 4152\n",
    );
    let error = KhiveConfig::load(Some(&path)).expect_err("memory has no WAL extent");
    assert!(matches!(
        config_error_root(&error),
        ConfigError::WalCeilingMemoryBackend { name, value }
            if name == "main" && *value == 4152
    ));
}

#[test]
fn wal_ceiling_nonzero_non_wal_backend_is_typed_error() {
    let error = resolve_wal_ceiling(Some(4152), None, "main", BackendKind::Sqlite, false, false)
        .expect_err("non-WAL SQLite cannot enforce a WAL extent ceiling");
    assert!(matches!(
        error,
        ConfigError::WalCeilingNonWalBackend { name, value }
            if name == "main" && value == 4152
    ));
}

#[test]
fn wal_ceiling_offset_overflow_fails_before_backend_kind() {
    let overflow = i64::MAX as u64 + 1;
    let error = resolve_wal_ceiling(
        Some(overflow),
        None,
        "ephemeral",
        BackendKind::Memory,
        false,
        false,
    )
    .expect_err("unsupported SQLite offset must fail before backend checks");
    assert!(matches!(
        error,
        ConfigError::WalCeilingOffsetOverflow { name, value }
            if name == "ephemeral" && value == overflow
    ));
}

#[test]
fn wal_ceiling_field_precedes_environment_and_read_only_disables_enforcement() {
    let resolved = resolve_wal_ceiling(
        Some(4152),
        Some("not-a-byte-count"),
        "archive",
        BackendKind::Sqlite,
        true,
        true,
    )
    .expect("higher-priority field makes lower-priority environment irrelevant");
    assert_eq!(resolved.configured_bytes, 4152);
    assert_eq!(resolved.effective_bytes, 0);
    assert_eq!(resolved.source, khive_db::WalCeilingSource::BackendField);

    let invalid = resolve_wal_ceiling(
        None,
        Some("not-a-byte-count"),
        "archive",
        BackendKind::Sqlite,
        true,
        false,
    )
    .expect_err("a selected malformed environment must fail closed");
    assert!(matches!(
        invalid,
        ConfigError::InvalidWalCeilingEnvironment { .. }
    ));
}

#[test]
fn test_pack_backend_assignment_parses() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "knowledge"
kind = "memory"

[packs.knowledge]
backend = "knowledge"
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error")
        .expect("file found");
    assert_eq!(cfg.packs.len(), 1);
    let pc = cfg.packs.get("knowledge").expect("knowledge pack present");
    assert_eq!(pc.backend, "knowledge");
}

#[test]
fn test_duplicate_backend_name_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "dup"
kind = "memory"

[[backends]]
name = "dup"
kind = "memory"
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("should fail with duplicate name");
    assert!(
        matches!(config_error_root(&err), ConfigError::DuplicateBackendName { ref name } if name == "dup"),
        "expected DuplicateBackendName {{ name: \"dup\" }}, got {err:?}"
    );
}

#[test]
fn test_empty_backend_name_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = ""
kind = "memory"
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("empty backend name must fail");
    assert!(
        matches!(config_error_root(&err), ConfigError::InvalidBackendName { ref name, .. } if name.is_empty()),
        "expected InvalidBackendName for the empty name, got {err:?}"
    );
}

#[test]
fn test_backend_served_kinds_absent_and_declared() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "legacy"
kind = "memory"

[[backends]]
name = "notes"
kind = "memory"
served_kinds = ["note", "event"]
"#,
    );
    let config = KhiveConfig::load(Some(&path))
        .expect("valid served-kind declarations")
        .expect("config file found");

    assert!(config.backends[0].served_kinds.is_none());
    assert_eq!(
        config.backends[1].served_kinds,
        Some(BTreeSet::from([SubstrateKind::Note, SubstrateKind::Event]))
    );
}

#[test]
fn test_empty_backend_served_kinds_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "main"
kind = "memory"
served_kinds = []
"#,
    );
    let error = KhiveConfig::load(Some(&path))
        .expect_err("an explicit empty served-kind declaration must fail closed");

    assert!(matches!(
        config_error_root(&error),
        ConfigError::EmptyBackendServedKinds { name } if name == "main"
    ));
}

#[test]
fn test_unknown_backend_served_kind_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "main"
kind = "memory"
served_kinds = ["asset"]
"#,
    );
    let error = KhiveConfig::load(Some(&path))
        .expect_err("served-kind declarations use a closed vocabulary");

    assert!(matches!(
        config_error_root(&error),
        ConfigError::Parse { .. }
    ));
    assert!(error.to_string().contains("unknown variant `asset`"));
}

#[test]
fn test_pack_referencing_undefined_backend_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "knowledge"
kind = "memory"

[packs.kg]
backend = "nonexistent"
"#,
    );
    let err =
        KhiveConfig::load(Some(&path)).expect_err("should fail with unknown backend reference");
    assert!(
        matches!(config_error_root(&err), ConfigError::UnknownPackBackend { ref pack, ref backend, .. }
                if pack == "kg" && backend == "nonexistent"),
        "expected UnknownPackBackend for kg→nonexistent, got {err:?}"
    );
}

#[test]
fn test_pack_config_without_backends_section_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    // Explicit pack routes may name the implicit main backend.
    let path = write_toml(
        &dir,
        r#"
[packs.kg]
backend = "main"
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error expected")
        .expect("file found");
    assert_eq!(cfg.backends.len(), 0);
    assert_eq!(cfg.packs.len(), 1);
}

#[test]
fn test_implicit_main_rejects_unknown_pack_backend() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "[packs.comm]\nbackend = 'does-not-exist'\n");
    let error = KhiveConfig::load(Some(&path)).expect_err("unknown route must fail");
    assert!(matches!(
        config_error_root(&error),
        ConfigError::UnknownPackBackend { pack, backend, defined }
            if pack == "comm" && backend == "does-not-exist" && defined == "main"
    ));
}

#[test]
fn test_backend_search_coverage_rejects_missing_substrates() {
    for (served, missing) in [
        ("'note'", vec![SubstrateKind::Entity]),
        ("'entity'", vec![SubstrateKind::Note]),
        ("'event'", vec![SubstrateKind::Note, SubstrateKind::Entity]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            &format!("[[backends]]\nname = 'main'\nkind = 'memory'\nserved_kinds = [{served}]\n"),
        );
        let error = KhiveConfig::load(Some(&path)).expect_err("incomplete coverage");
        assert!(
            matches!(
                config_error_root(&error),
                ConfigError::MissingBackendSearchKinds { kinds, defined }
                    if kinds == &missing && defined == "main"
            ),
            "unexpected coverage error: {error}"
        );
    }
}

#[test]
fn test_backend_search_coverage_allows_split_substrates_and_event_only_secondary() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "main"
kind = "memory"
served_kinds = ["entity"]

[[backends]]
name = "notes"
kind = "memory"
served_kinds = ["note"]

[[backends]]
name = "events"
kind = "memory"
served_kinds = ["event"]
"#,
    );
    KhiveConfig::load(Some(&path)).expect("search coverage is the union of backends");
}

#[test]
fn test_backend_cache_mb_rejected_at_validate() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "main"
kind = "memory"
cache_mb = 128
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("cache_mb must be rejected");
    assert!(
        matches!(config_error_root(&err), ConfigError::UnsupportedBackendField { ref name, field: "cache_mb" } if name == "main"),
        "expected UnsupportedBackendField {{ name: \"main\", field: \"cache_mb\" }}, got {err:?}"
    );
}

#[test]
fn test_backend_journal_mode_rejected_at_validate() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[backends]]
name = "main"
kind = "memory"
journal_mode = "wal"
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("journal_mode must be rejected");
    assert!(
            matches!(config_error_root(&err), ConfigError::UnsupportedBackendField { ref name, field: "journal_mode" } if name == "main"),
            "expected UnsupportedBackendField {{ name: \"main\", field: \"journal_mode\" }}, got {err:?}"
        );
}

// A top-level `db` key must be rejected loudly instead of silently
// ignored as an unknown key by serde's forward-compatible default.
#[test]
fn test_top_level_db_rejected_at_validate() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
db = "/tmp/scratch/demo.db"
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("top-level db must be rejected");
    assert!(
        matches!(config_error_root(&err), ConfigError::UnsupportedTopLevelDb { ref value } if value == "/tmp/scratch/demo.db"),
        "expected UnsupportedTopLevelDb {{ value: \"/tmp/scratch/demo.db\" }}, got {err:?}"
    );
}

#[test]
fn gate_caller_enrollment_config_loads_for_runtime_enforcement() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[gate]
granted_actors = ["lambda:enrolled"]
grant_unattributed = false
"#,
    );

    let config = KhiveConfig::load(Some(&path))
        .expect("the supported caller-enrollment policy must parse")
        .expect("config exists");
    let gate = config.gate.expect("gate section");
    assert_eq!(gate.granted_actors, vec!["lambda:enrolled"]);
    assert!(!gate.grant_unattributed);
}

#[test]
fn unknown_actor_key_fails_to_load() {
    let dir = tempfile::tempdir().unwrap();

    // Control first, in the same test: the identical table carrying only
    // supported keys must load, so the refusal below is attributable to the
    // unknown key rather than to the fixture.
    let supported = write_toml(
        &dir,
        r#"
[actor]
id = "lambda:example"
visible_namespaces = ["lambda:other"]
"#,
    );
    KhiveConfig::load(Some(&supported))
        .expect("a config using only supported [actor] keys must parse")
        .expect("config exists");

    // A `[gate]` key written one table too high. Silently discarding it
    // leaves anonymous admission at whatever it already was while the file
    // on disk says otherwise, so it has to fail startup.
    let misplaced = write_toml(
        &dir,
        r#"
[actor]
id = "lambda:example"
grant_unattributed = false
"#,
    );
    let err = KhiveConfig::load(Some(&misplaced))
        .expect_err("a [gate] key written under [actor] must fail startup");
    assert!(
        err.to_string().contains("grant_unattributed"),
        "the refusal must name the offending key, got: {err}"
    );
}

#[test]
fn caller_enrollment_policy_is_enforced_at_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[actor]
id = "lambda:enrolled"

[gate]
granted_actors = ["lambda:enrolled"]
grant_unattributed = false
"#,
    );
    let mut config = KhiveConfig::load(Some(&path))
        .expect("load")
        .expect("config exists");
    let allowed = crate::runtime_config_from_khive_config(&config, in_memory_runtime_config());
    let runtime = crate::KhiveRuntime::new(allowed).expect("runtime");
    runtime
        .authorize(Namespace::local())
        .expect("listed actor is admitted");

    config.actor.id = Some("lambda:other".to_string());
    let denied = crate::runtime_config_from_khive_config(&config, in_memory_runtime_config());
    let runtime = crate::KhiveRuntime::new(denied).expect("runtime");
    assert!(matches!(
        runtime.authorize(Namespace::local()),
        Err(crate::RuntimeError::PermissionDenied { ref verb, ref reason, .. })
            if verb == "authorize" && reason == "actor is not enrolled"
    ));
}

#[test]
fn grant_unattributed_controls_anonymous_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "[gate]\ngrant_unattributed = false\n");
    let mut config = KhiveConfig::load(Some(&path))
        .expect("load")
        .expect("config exists");
    let denied = crate::runtime_config_from_khive_config(&config, in_memory_runtime_config());
    let runtime = crate::KhiveRuntime::new(denied).expect("runtime");
    assert!(matches!(
        runtime.authorize(Namespace::local()),
        Err(crate::RuntimeError::PermissionDenied { ref reason, .. })
            if reason == "unattributed caller is not enrolled"
    ));

    config.gate.as_mut().expect("gate").grant_unattributed = true;
    let allowed = crate::runtime_config_from_khive_config(&config, in_memory_runtime_config());
    crate::KhiveRuntime::new(allowed)
        .expect("runtime")
        .authorize(Namespace::local())
        .expect("anonymous caller is explicitly admitted");
}

#[test]
fn empty_gate_table_is_explicit_deny_all_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "[gate]\n");
    let config = KhiveConfig::load(Some(&path))
        .expect("empty gate table parses")
        .expect("config exists");
    assert_eq!(config.gate, Some(GateSectionConfig::default()));
    let runtime_config =
        crate::runtime_config_from_khive_config(&config, in_memory_runtime_config());
    let runtime = crate::KhiveRuntime::new(runtime_config).expect("runtime");
    assert!(matches!(
        runtime.authorize(Namespace::local()),
        Err(crate::RuntimeError::PermissionDenied { .. })
    ));
}

#[test]
fn unknown_gate_key_fails_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "[gate]\ngranted_actor = [\"lambda:typo\"]\n");
    let err = KhiveConfig::load(Some(&path)).expect_err("unknown gate key must fail");
    assert!(matches!(err, ConfigError::Parse { .. }));
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn write_denials_survive_both_runtime_config_paths() {
    let dir = tempfile::tempdir().unwrap();
    for engines in [
        "",
        "\n[[engines]]\nname = 'main'\nmodel = 'all-minilm-l6-v2'\ndefault = true\n",
    ] {
        let path = write_toml(&dir, &format!(
                "[actor]\nid='seat:duty'\n[gate]\ngranted_actors=['seat:duty','seat:writer']\ndeny_writes_for=['*:duty']\n{engines}"
            ));
        let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
        let runtime = crate::runtime_config_from_khive_config(&config, in_memory_runtime_config());
        for (actor, verb, allowed) in [
            ("seat:duty", "list", true),
            ("seat:duty", "create", false),
            ("seat:writer", "create", true),
            ("unlisted", "list", false),
        ] {
            let req = crate::GateRequest::new(
                crate::ActorRef::new("actor", actor),
                Namespace::local(),
                verb,
                serde_json::Value::Null,
            );
            assert_eq!(
                runtime.gate.check(&req).unwrap().is_allow(),
                allowed,
                "{actor} {verb}"
            );
        }
    }
}

#[test]
fn invalid_write_denials_fail_config_load_and_direct_config_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    for value in [
        "['']".to_string(),
        "['   ']".into(),
        format!("['{}']", "é".repeat(129)),
        format!("[{}]", vec!["'*'"; 257].join(",")),
    ] {
        let path = write_toml(&dir, &format!("[gate]\ndeny_writes_for={value}\n"));
        let error = KhiveConfig::load(Some(&path)).unwrap_err();
        assert!(
            matches!(
                config_error_root(&error),
                ConfigError::InvalidWriteDenyPatterns { .. }
            ),
            "{error}"
        );
    }
    for field in ["deny_write_for=['*']", "deny_writes_for=[17]"] {
        let path = write_toml(&dir, &format!("[gate]\n{field}\n"));
        assert!(KhiveConfig::load(Some(&path)).is_err());
    }
    let path = write_toml(
        &dir,
        "[gate]\ngranted_actors=['writer']\ndeny_writes_for=['用户@*/[?]']\n",
    );
    let mut config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
    config.gate.as_mut().unwrap().deny_writes_for = vec![String::new()];
    let runtime = crate::runtime_config_from_khive_config(&config, in_memory_runtime_config());
    let req = crate::GateRequest::new(
        crate::ActorRef::new("actor", "writer"),
        Namespace::local(),
        "list",
        serde_json::Value::Null,
    );
    assert!(matches!(
        runtime.gate.check(&req),
        Err(crate::GateError::Policy(_))
    ));
}

#[test]
fn absent_gate_preserves_the_programmatic_gate() {
    let mut base = in_memory_runtime_config();
    base.gate = std::sync::Arc::new(crate::CallerEnrollmentGate::new(vec![], false));
    let configured = crate::runtime_config_from_khive_config(&KhiveConfig::default(), base.clone());
    assert!(std::sync::Arc::ptr_eq(&base.gate, &configured.gate));
}

#[test]
fn invalid_granted_actor_fails_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "[gate]\ngranted_actors = [\"not valid\"]\n");
    let err = KhiveConfig::load(Some(&path)).expect_err("invalid actor id must fail");
    assert!(matches!(
        config_error_root(&err),
        ConfigError::InvalidGrantedActorId { id, .. } if id == "not valid"
    ));
}

#[test]
fn unrelated_unknown_top_level_sections_remain_forward_compatible() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "[future_feature]\nenabled = true\n");
    KhiveConfig::load(Some(&path))
        .expect("unrelated future config stays forward compatible")
        .expect("config exists");
}

#[test]
fn brain_fleet_readers_default_to_empty() {
    assert!(KhiveConfig::default().brain.fleet_readers.is_empty());
    assert!(in_memory_runtime_config().brain.fleet_readers.is_empty());

    let dir = tempfile::tempdir().unwrap();
    for engines in [
        "",
        "[[engines]]\nname = \"primary\"\nmodel = \"all-minilm-l6-v2\"\ndefault = true\n",
    ] {
        for brain in ["", "[brain]\n", "[brain]\nfleet_readers = []\n"] {
            let path = write_toml(&dir, &format!("{engines}\n{brain}"));
            let config = KhiveConfig::load(Some(&path))
                .expect("load")
                .expect("config exists");
            assert!(config.brain.fleet_readers.is_empty());

            let mut base = in_memory_runtime_config();
            base.brain.fleet_readers = vec!["lambda:previous".to_string()];
            let resolved = crate::runtime_config_from_khive_config(&config, base);
            assert!(resolved.brain.fleet_readers.is_empty());
        }
    }
}

#[test]
fn brain_fleet_readers_parse_and_resolve_with_or_without_engines() {
    let dir = tempfile::tempdir().unwrap();
    for engines in [
        "",
        "[[engines]]\nname = \"primary\"\nmodel = \"all-minilm-l6-v2\"\ndefault = true\n",
    ] {
        let path = write_toml(
            &dir,
            &format!(
                "{engines}\n[brain]\nfleet_readers = [\"lambda:reader\", \"lambda:auditor\"]\n"
            ),
        );
        let config = KhiveConfig::load(Some(&path))
            .expect("load")
            .expect("config exists");
        assert_eq!(
            config.brain.fleet_readers,
            vec!["lambda:reader", "lambda:auditor"]
        );

        let mut base = in_memory_runtime_config();
        base.brain.fleet_readers = vec!["lambda:previous".to_string()];
        let resolved = crate::runtime_config_from_khive_config(&config, base);
        assert_eq!(
            resolved.brain.fleet_readers,
            vec!["lambda:reader", "lambda:auditor"]
        );
    }
}

#[test]
fn unknown_brain_key_fails_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "[brain]\nfleet_reader = [\"lambda:reader\"]\n");
    let err = KhiveConfig::load(Some(&path)).expect_err("unknown brain key must fail");
    assert!(matches!(err, ConfigError::Parse { .. }));
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn telemetry_missing_default_stays_absent_with_or_without_engines() {
    use crate::{TelemetryCarrier, TelemetryConfig};

    assert_eq!(KhiveConfig::default().telemetry, TelemetryConfig::default());
    assert_eq!(
        in_memory_runtime_config().telemetry,
        TelemetryConfig::default()
    );
    let dir = tempfile::tempdir().unwrap();
    for engines in [
        "",
        "[[engines]]\nname = \"primary\"\nmodel = \"all-minilm-l6-v2\"\ndefault = true\n",
    ] {
        for telemetry in ["", "[telemetry]\n"] {
            let path = write_toml(&dir, &format!("{engines}\n{telemetry}"));
            let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
            let mut base = in_memory_runtime_config();
            base.telemetry.stream = "previous".to_string();
            base.telemetry.default_carrier = Some(TelemetryCarrier::Durable);
            let resolved = crate::runtime_config_from_khive_config(&config, base);
            assert_eq!(resolved.telemetry, TelemetryConfig::default());
            assert_eq!(resolved.telemetry.stream, "telemetry");
            assert_eq!(resolved.telemetry.default_carrier, None);
            let error = resolved
                .telemetry
                .validate_activation()
                .expect_err("activating telemetry requires the declared default");
            assert!(error.to_string().contains("telemetry.default_carrier"));
        }
    }
}

#[test]
fn telemetry_table_loads_and_resolves_with_or_without_engines() {
    use crate::{TelemetryCarrier, TelemetryFailurePosture};

    let dir = tempfile::tempdir().unwrap();
    for engines in [
        "",
        "[[engines]]\nname = \"primary\"\nmodel = \"all-minilm-l6-v2\"\ndefault = true\n",
    ] {
        let path = write_toml(
            &dir,
            &format!(
                r#"{engines}
[telemetry]
stream = "operations"
default_carrier = "durable"
[[telemetry.channels]]
kinds = ["run.started", "run.completed"]
carrier = "durable"
failure_posture = "gap"
[[telemetry.channels]]
kinds = ["turn.delta", "*.heartbeat"]
carrier = "ephemeral"
failure_posture = "stop"
"#
            ),
        );
        let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
        assert_eq!(config.telemetry.channels.len(), 2);
        let resolved = crate::runtime_config_from_khive_config(&config, in_memory_runtime_config());
        assert_eq!(resolved.telemetry, config.telemetry);
        assert_eq!(resolved.telemetry.stream, "operations");
        for kind in ["run.started", "run.completed"] {
            let policy = resolved.telemetry.policy_for_kind(kind).unwrap();
            assert_eq!(policy.carrier, TelemetryCarrier::Durable);
            assert_eq!(policy.failure_posture, TelemetryFailurePosture::Gap);
        }
        for kind in ["turn.delta", "run.heartbeat", "turn.child.heartbeat"] {
            let policy = resolved.telemetry.policy_for_kind(kind).unwrap();
            assert_eq!(policy.carrier, TelemetryCarrier::Ephemeral);
            assert_eq!(policy.failure_posture, TelemetryFailurePosture::Stop);
        }
        for kind in [
            "unclassified",
            "heartbeat",
            "run.notheartbeat",
            "run.heartbeat.extra",
        ] {
            let policy = resolved.telemetry.policy_for_kind(kind).unwrap();
            assert_eq!(policy.carrier, TelemetryCarrier::Durable);
            assert_eq!(policy.failure_posture, TelemetryFailurePosture::Stop);
        }
    }
}

#[test]
fn telemetry_invalid_policy_values_name_the_channel() {
    let dir = tempfile::tempdir().unwrap();
    for (carrier, posture, field, value) in [
        ("disk", "stop", "carrier", "disk"),
        ("Durable", "stop", "carrier", "Durable"),
        ("durable", "ignore", "failure_posture", "ignore"),
        ("durable", "Stop", "failure_posture", "Stop"),
    ] {
        let path = write_toml(
            &dir,
            &format!(
                r#"[[telemetry.channels]]
kinds = ["first"]
carrier = "ephemeral"
failure_posture = "gap"
[[telemetry.channels]]
kinds = ["second"]
carrier = "{carrier}"
failure_posture = "{posture}"
"#
            ),
        );
        let error = KhiveConfig::load(Some(&path)).expect_err("invalid policy must refuse");
        let message = error.to_string();
        for expected in ["telemetry.channels[1]", field, value] {
            assert!(message.contains(expected), "{message}");
        }
    }
    let path = write_toml(&dir, "[telemetry]\ndefault_carrier = \"disk\"\n");
    let error = KhiveConfig::load(Some(&path)).expect_err("unknown fallback must refuse");
    assert!(
        error.to_string().contains("telemetry.default_carrier"),
        "{error}"
    );
}

#[test]
fn telemetry_overlapping_channels_name_both_entries() {
    let dir = tempfile::tempdir().unwrap();
    for (first, second) in [
        ("run.started", "run.started"),
        ("run.heartbeat", "*.heartbeat"),
        ("*.heartbeat", "run.heartbeat"),
        ("*.heartbeat", "*.heartbeat"),
        ("*.heartbeat", "*.child.heartbeat"),
        ("*.child.heartbeat", "*.heartbeat"),
    ] {
        let path = write_toml(
            &dir,
            &format!(
                r#"[[telemetry.channels]]
kinds = ["{first}"]
carrier = "ephemeral"
failure_posture = "gap"
[[telemetry.channels]]
kinds = ["{second}"]
carrier = "durable"
failure_posture = "stop"
"#
            ),
        );
        let error = KhiveConfig::load(Some(&path)).expect_err("overlap must refuse");
        let message = error.to_string();
        for expected in [
            "telemetry.channels[1]",
            "telemetry.channels[0]",
            first,
            second,
        ] {
            assert!(message.contains(expected), "{message}");
        }
    }
}

#[test]
fn telemetry_empty_and_invalid_kind_patterns_name_the_channel() {
    let dir = tempfile::tempdir().unwrap();
    for kinds in [
        "[]",
        "[\"\"]",
        "[\" \"]",
        "[\"two names\"]",
        "[\"*\"]",
        "[\"run.*\"]",
        "[\"*.\"]",
        "[\"**.heartbeat\"]",
        "[\"*.heart*beat\"]",
    ] {
        let path = write_toml(
            &dir,
            &format!(
                r#"[[telemetry.channels]]
kinds = ["first"]
carrier = "ephemeral"
failure_posture = "gap"
[[telemetry.channels]]
kinds = {kinds}
carrier = "durable"
failure_posture = "stop"
"#
            ),
        );
        let error = KhiveConfig::load(Some(&path)).expect_err("invalid kinds must refuse");
        assert!(
            error.to_string().contains("telemetry.channels[1]"),
            "{error}"
        );
    }
}

#[test]
fn telemetry_tables_reject_unknown_keys() {
    let dir = tempfile::tempdir().unwrap();
    for content in [
            "[telemetry]\ndefault_carrrier = \"durable\"\n",
            "[telemetry.ring]\ncapacity = 4096\n",
            "[[telemetry.channels]]\nkinds = [\"run\"]\ncarrier = \"durable\"\nfailure_posture = \"stop\"\ncarrrier = \"ephemeral\"\n",
        ] {
            let path = write_toml(&dir, content);
            let error = KhiveConfig::load(Some(&path)).expect_err("unknown key must refuse");
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
}

// ── [git_write] section (ADR-108 Amendment) ─────────────────────────────

// No [git_write] section at all -> empty allowlist, valid config.
#[test]
fn test_no_git_write_section_is_valid_and_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "# no git_write section\n");
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error")
        .expect("file found");
    assert!(cfg.git_write.allowed.is_empty());
}

fn write_git_program_config(dir: &tempfile::TempDir, program: &Path) -> PathBuf {
    let program = toml::Value::String(program.to_str().unwrap().to_string());
    write_toml(dir, &format!("[git_write]\nprogram = {program}\n"))
}

#[test]
fn git_program_absent_preserves_path_default() {
    let dir = tempfile::tempdir().unwrap();
    for content in ["# no git_write section\n", "[git_write]\n"] {
        let path = write_toml(&dir, content);
        let cfg = KhiveConfig::load(Some(&path)).unwrap().unwrap();
        assert!(cfg.git_write.program.is_none());
        assert_eq!(cfg.git_write.git_program(), Path::new("git"));
    }
    assert!(GitWriteSectionConfig::default().program.is_none());
    assert_eq!(
        GitWriteSectionConfig::default().git_program(),
        Path::new("git")
    );
}

#[cfg(any(unix, windows))]
#[test]
fn git_program_absolute_executable_loads() {
    let dir = tempfile::tempdir().unwrap();
    let program = std::env::current_exe().unwrap();
    let path = write_git_program_config(&dir, &program);
    let cfg = KhiveConfig::load(Some(&path)).unwrap().unwrap();
    assert_eq!(cfg.git_write.program.as_deref(), Some(program.as_path()));
    assert_eq!(cfg.git_write.git_program(), program);
}

#[test]
fn git_program_relative_path_is_rejected_at_load() {
    let dir = tempfile::tempdir().unwrap();
    for program in ["git", "relative/git"] {
        let path = write_git_program_config(&dir, Path::new(program));
        let error = KhiveConfig::load(Some(&path)).expect_err("relative program must fail");
        assert!(
            matches!(config_error_root(&error), ConfigError::InvalidGitWriteConfig { key, reason }
                    if key == "git_write.program" && reason == "must be absolute"),
            "unexpected error: {error}"
        );
        assert!(error.to_string().contains("git_write.program"));
    }
}

#[test]
fn git_program_missing_file_is_rejected_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_git_program_config(&dir, &dir.path().join("missing-git"));
    let error = KhiveConfig::load(Some(&path)).expect_err("missing program must fail");
    assert!(
        matches!(config_error_root(&error), ConfigError::InvalidGitWriteConfig { key, reason }
                if key == "git_write.program" && reason == "does not exist"),
        "unexpected error: {error}"
    );
    assert!(error.to_string().contains("git_write.program"));
}

#[test]
fn git_program_nonexecutable_file_is_rejected_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("git.txt");
    std::fs::write(&program, "not executable\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let path = write_git_program_config(&dir, &program);
    let error = KhiveConfig::load(Some(&path)).expect_err("nonexecutable program must fail");
    assert!(
        matches!(config_error_root(&error), ConfigError::InvalidGitWriteConfig { key, reason }
                if key == "git_write.program" && reason == "is not executable"),
        "unexpected error: {error}"
    );
    assert!(error.to_string().contains("git_write.program"));
}

#[test]
fn git_program_directory_is_rejected_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("git.exe");
    std::fs::create_dir(&program).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = write_git_program_config(&dir, &program);
    let error = KhiveConfig::load(Some(&path)).expect_err("directory program must fail");
    assert!(
        matches!(config_error_root(&error), ConfigError::InvalidGitWriteConfig { key, reason }
                if key == "git_write.program" && reason == "is not executable"),
        "unexpected error: {error}"
    );
    assert!(error.to_string().contains("git_write.program"));
}

// A well-formed [[git_write.allowed]] entry parses correctly.
#[test]
fn test_git_write_entry_parses() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[git_write.allowed]]
repo = "/abs/path/repo"
branches = ["feat/*", "fix/*"]
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error")
        .expect("file found");
    assert_eq!(cfg.git_write.allowed.len(), 1);
    assert_eq!(cfg.git_write.allowed[0].repo, "/abs/path/repo");
    assert_eq!(
        cfg.git_write.allowed[0].branches,
        vec!["feat/*".to_string(), "fix/*".to_string()]
    );
}

// A relative repo path is rejected at validate() time.
#[test]
fn test_git_write_relative_repo_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[git_write.allowed]]
repo = "relative/path"
branches = ["main"]
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("relative repo must be rejected");
    assert!(
        matches!(config_error_root(&err), ConfigError::InvalidGitWriteEntry { ref repo, .. } if repo == "relative/path"),
        "expected InvalidGitWriteEntry, got {err:?}"
    );
}

// ADR-108: a branch pattern with more than one `*` is rejected at
// validate() time -- the ADR authorizes exact-name or single-wildcard
// patterns only.
#[test]
fn test_git_write_multi_star_branch_pattern_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[git_write.allowed]]
repo = "/abs/path"
branches = ["**"]
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("** must be rejected");
    assert!(
        matches!(config_error_root(&err), ConfigError::InvalidGitWriteEntry { ref repo, .. } if repo == "/abs/path"),
        "expected InvalidGitWriteEntry, got {err:?}"
    );

    let dir2 = tempfile::tempdir().unwrap();
    let path2 = write_toml(
        &dir2,
        r#"
[[git_write.allowed]]
repo = "/abs/path"
branches = ["rel-*-*-final"]
"#,
    );
    let err2 = KhiveConfig::load(Some(&path2)).expect_err("rel-*-*-final must be rejected");
    assert!(
        matches!(
            config_error_root(&err2),
            ConfigError::InvalidGitWriteEntry { .. }
        ),
        "expected InvalidGitWriteEntry, got {err2:?}"
    );
}

// Single-wildcard patterns remain accepted.
#[test]
fn test_git_write_single_star_branch_pattern_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[git_write.allowed]]
repo = "/abs/path"
branches = ["a*b", "main"]
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error")
        .expect("file found");
    assert_eq!(cfg.git_write.allowed[0].branches, vec!["a*b", "main"]);
}

// An entry with an empty branches list is rejected at validate() time --
// it would otherwise silently allowlist a repo for no branch at all.
#[test]
fn test_git_write_empty_branches_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[[git_write.allowed]]
repo = "/abs/path"
branches = []
"#,
    );
    let err = KhiveConfig::load(Some(&path)).expect_err("empty branches must be rejected");
    assert!(
        matches!(config_error_root(&err), ConfigError::InvalidGitWriteEntry { ref repo, .. } if repo == "/abs/path"),
        "expected InvalidGitWriteEntry, got {err:?}"
    );
}

#[test]
fn git_actor_mapping_and_resolver_defaults_parse_without_resolution() {
    let cfg: KhiveConfig = toml::from_str(
        r#"
[git_write.actors."lambda:example"]
name = "Example"
email = "example@example.invalid"
credential_ref = "example-reference"
platform_identity = "example-login"
"#,
    )
    .unwrap();
    if cfg!(unix) {
        cfg.validate().unwrap();
    } else {
        assert!(cfg.validate().is_err());
    }
    let identity = &cfg.git_write.actors["lambda:example"];
    assert_eq!(identity.name, "Example");
    assert_eq!(identity.credential_ref, "example-reference");
    assert_eq!(
        cfg.git_write.credential_resolver,
        GitWriteSectionConfig::default().credential_resolver
    );
}

#[test]
fn git_resolver_accepts_only_absolute_argv_with_ref_template() {
    for argv in [
        vec![],
        vec!["relative-resolver", "{ref}"],
        vec!["/bin/sh", "-c", "{ref}"],
        vec!["/usr/bin/env", "sh", "{ref}"],
        vec!["/absolute/resolver", "{token}"],
        vec!["/absolute/resolver", "--service={ref}"],
        vec!["/absolute/resolver"],
        vec!["/absolute/resolver", "{ref}", "bad\0arg"],
    ] {
        let config = GitWriteSectionConfig {
            credential_resolver: argv.into_iter().map(str::to_string).collect(),
            ..Default::default()
        };
        assert!(matches!(
            config.validate_dev_loop(),
            Err(ConfigError::InvalidGitWriteConfig { key, .. }) if key == "credential_resolver"
        ));
    }
    let config = GitWriteSectionConfig {
        credential_resolver: vec![
            std::env::temp_dir()
                .join("not-installed-yet/resolver")
                .to_string_lossy()
                .into_owned(),
            "--reference".to_string(),
            "{ref}".to_string(),
        ],
        ..Default::default()
    };
    config.validate_dev_loop().unwrap();
}

#[test]
fn git_actor_mapping_rejects_invalid_identity_and_unknown_fields() {
    let actor = GitWriteActorConfig {
        name: "Example".to_string(),
        email: "example@example.invalid".to_string(),
        credential_ref: "example-reference".to_string(),
        platform_identity: "example-login".to_string(),
    };
    for field in ["name", "email", "credential_ref", "platform_identity"] {
        let mut invalid = actor.clone();
        match field {
            "name" => invalid.name.clear(),
            "email" => invalid.email = "bad\nemail".to_string(),
            "credential_ref" => invalid.credential_ref.clear(),
            "platform_identity" => invalid.platform_identity.clear(),
            _ => unreachable!(),
        }
        let config = GitWriteSectionConfig {
            actors: BTreeMap::from([("example".to_string(), invalid)]),
            ..Default::default()
        };
        assert!(config.validate_dev_loop().is_err());
    }
    assert!(toml::from_str::<GitWriteActorConfig>(
        r#"name = "Example"
email = "example@example.invalid"
credential_ref = "reference"
platform_identity = "login"
credential = "not-an-accepted-field""#
    )
    .is_err());
}

#[test]
fn git_repository_merge_refusals_accept_only_the_two_named_entries() {
    let row = |refusals: &[&str]| GitWriteSectionConfig {
        repositories: BTreeMap::from([(
            "/repo".to_string(),
            GitWriteRepositoryConfig {
                remote: "https://github.com/example/repo".to_string(),
                slug: "example/repo".to_string(),
                visibility: "private".to_string(),
                merge_refusals: refusals.iter().map(|entry| entry.to_string()).collect(),
            },
        )]),
        ..Default::default()
    };
    for refusals in [
        &[][..],
        &["opener"][..],
        &["last_pusher"][..],
        &["opener", "last_pusher"][..],
    ] {
        row(refusals).validate_dev_loop().unwrap();
    }
    for refusals in [
        &["author"][..],
        &["Opener"][..],
        &["opener", "opener"][..],
        &["last_pusher", "opener", "last_pusher"][..],
    ] {
        assert!(matches!(
            row(refusals).validate_dev_loop(),
            Err(ConfigError::InvalidGitWriteConfig { key, .. })
                if key == "repositories./repo.merge_refusals"
        ));
    }
    let parsed: GitWriteRepositoryConfig = toml::from_str(
        r#"remote = "https://github.com/example/repo"
slug = "example/repo"
visibility = "private""#,
    )
    .unwrap();
    assert!(parsed.merge_refusals.is_empty());
    assert!(toml::from_str::<GitWriteRepositoryConfig>(
        r#"remote = "https://github.com/example/repo"
slug = "example/repo"
visibility = "private"
merge_refusal = ["opener"]"#
    )
    .is_err());
}

#[test]
fn git_contract_faults_are_feature_gated_before_empty_engines_return() {
    let cfg = KhiveConfig {
        git_write: GitWriteSectionConfig {
            contract_faults: true,
            ..Default::default()
        },
        ..Default::default()
    };
    if cfg!(feature = "contract-faults") {
        cfg.validate().unwrap();
    } else {
        let error = cfg.validate().unwrap_err();
        assert!(matches!(error, ConfigError::InvalidGitWriteConfig { .. }));
        assert!(error.to_string().contains("contract-faults"));
    }
}

#[test]
fn git_unmapped_legacy_default_remains_valid_on_every_platform() {
    GitWriteSectionConfig::default()
        .validate_dev_loop()
        .unwrap();
    let config = GitWriteSectionConfig {
        credential_resolver: vec!["relative-resolver".to_string(), "{ref}".to_string()],
        ..Default::default()
    };
    assert!(config.validate_dev_loop().is_err());
}

#[test]
fn git_fault_selectors_require_opt_in() {
    let config = GitWriteSectionConfig {
        fault: Some("git.push:reply-lost-after-effect".to_string()),
        ..Default::default()
    };
    assert!(matches!(
        config.validate_dev_loop(),
        Err(ConfigError::InvalidGitWriteConfig { key, .. }) if key == "fault"
    ));
}

// ── [display] section (ADR-169) ──────────────────────────────────────────

// No [display] section at all -> None, resolved to the host zone downstream.
#[test]
fn test_no_display_section_defaults_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(&dir, "# no display section\n");
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error")
        .expect("file found");
    assert!(cfg.display.timezone.is_none());
}

#[test]
fn test_display_timezone_valid_iana_name_parses() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[display]
timezone = "America/New_York"
"#,
    );
    let cfg = KhiveConfig::load(Some(&path))
        .expect("no error")
        .expect("file found");
    assert_eq!(cfg.display.timezone.as_deref(), Some("America/New_York"));
}

#[test]
fn test_display_timezone_unrecognized_name_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[display]
timezone = "Mars/Olympus_Mons"
"#,
    );
    let err = KhiveConfig::load(Some(&path))
        .expect_err("an unrecognized IANA zone name must fail at load, not silently fall back");
    assert!(
        matches!(config_error_root(&err), ConfigError::InvalidDisplayTimezone { ref timezone } if timezone == "Mars/Olympus_Mons"),
        "expected InvalidDisplayTimezone, got {err:?}"
    );
}

#[test]
fn test_display_timezone_empty_string_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        &dir,
        r#"
[display]
timezone = ""
"#,
    );
    let err =
        KhiveConfig::load(Some(&path)).expect_err("an empty timezone string must be rejected");
    assert!(
        matches!(
            config_error_root(&err),
            ConfigError::InvalidDisplayTimezone { .. }
        ),
        "expected InvalidDisplayTimezone, got {err:?}"
    );
}

#[test]
fn wal_ceiling_alias_conflict_escapes_control_characters_in_the_path() {
    let error = ConfigError::WalCeilingAliasConflict {
        first_backend: "main".to_string(),
        second_backend: "alias".to_string(),
        path: PathBuf::from("/data/line\nforged entry\x1b[31m/archive.db"),
        first_bytes: 0,
        second_bytes: 8192,
    };
    let text = error.to_string();
    assert!(
        !text.chars().any(|c| c == '\n' || c == '\x1b'),
        "a configured path must not put raw control characters in the error text; got {text:?}"
    );
    assert!(
        text.contains("line\\u{000a}forged entry\\u{001b}[31m/archive.db"),
        "control characters must be escaped in place; got {text:?}"
    );
    assert!(text.contains("resolve different WAL ceilings (0 and 8192 bytes)"));
}
include!("engine_config_backend_batch_tests.rs");
include!("engine_config_storage_tests.rs");
