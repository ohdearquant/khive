// khive#1221: with no primary set, the additional list must ADD to the
// built-in default primary, never replace it.
#[test]
fn env_additional_only_keeps_builtin_primary() {
    let cfg = super::config_from_env_parts(
        None,
        vec!["paraphrase-multilingual-minilm-l12-v2".to_string()],
    );
    assert_eq!(cfg.engines.len(), 2);
    let default_engine = cfg.default_engine().expect("a default engine");
    assert_eq!(default_engine.name, "all-minilm-l6-v2");
    assert!(
        cfg.engines
            .iter()
            .skip(1)
            .any(|e| e.name == "paraphrase-multilingual-minilm-l12-v2"),
        "additional model must be a non-default secondary engine"
    );
}

#[test]
fn env_explicit_primary_stays_primary() {
    let cfg = super::config_from_env_parts(
        Some("paraphrase-multilingual-minilm-l12-v2".to_string()),
        vec![],
    );
    assert_eq!(cfg.engines.len(), 1);
    assert_eq!(
        cfg.default_engine().expect("default").name,
        "paraphrase-multilingual-minilm-l12-v2"
    );
}

#[test]
fn env_additional_restating_primary_is_deduped() {
    let cfg = super::config_from_env_parts(None, vec!["all-minilm-l6-v2".to_string()]);
    assert_eq!(cfg.engines.len(), 1);
    assert_eq!(cfg.engines[0].name, "all-minilm-l6-v2");
}
