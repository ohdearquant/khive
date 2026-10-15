// ── engine_config integration ─────────────────────────────────────────────────

/// Write a fake config.toml with 3 engines, build a KhiveRuntime from it, and
/// confirm that `registered_embedding_model_names()` returns all 3 model names.
///
/// This test verifies the full pipeline:
///   KhiveConfig::load  →  runtime_config_from_khive_config  →  KhiveRuntime::new
///   →  registered_embedding_model_names
#[test]
fn engine_config_three_engines_all_registered() {
    use khive_runtime::{
        runtime_config_from_khive_config, KhiveConfig, KhiveRuntime, RuntimeConfig,
    };
    use std::io::Write;

    // Write a config.toml with 3 engines.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        r#"
[[engines]]
name = "primary"
model = "all-minilm-l6-v2"
default = true

[[engines]]
name = "para"
model = "paraphrase-multilingual-minilm-l12-v2"

[[engines]]
name = "bge-small"
model = "bge-small-en-v1.5"
"#
    )
    .unwrap();

    let khive_cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file should be found");
    assert_eq!(khive_cfg.engines.len(), 3);

    // Build RuntimeConfig from the KhiveConfig.
    let base = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let config = runtime_config_from_khive_config(&khive_cfg, base);
    assert!(
        config.embedding_model.is_some(),
        "default engine should set embedding_model"
    );
    assert_eq!(
        config.additional_embedding_models.len(),
        2,
        "two non-default engines should appear in additional_embedding_models"
    );

    // Create runtime and verify all 3 are registered.
    let rt = KhiveRuntime::new(config).expect("runtime should build");
    let mut names = rt.registered_embedding_model_names();
    names.sort();

    // The canonical to_string() forms of the models.
    let expected_substring_check = [
        "all-minilm-l6-v2",
        "bge-small-en-v1.5",
        "paraphrase-multilingual-minilm-l12-v2",
    ];
    assert_eq!(
        names.len(),
        3,
        "all 3 engines should be registered; got {names:?}"
    );
    for expected in &expected_substring_check {
        assert!(
            names.iter().any(|n| n.contains(expected)),
            "expected a registered model containing {expected:?}; registered: {names:?}"
        );
    }
}
