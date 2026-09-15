mod backend_batch_witnesses {
    use super::*;

    fn load_case(text: &str) -> Result<KhiveConfig, ConfigError> {
        let dir = tempfile::tempdir().expect("fixture directory");
        let path = write_toml(&dir, text);
        KhiveConfig::load(Some(&path)).map(|config| config.expect("explicit fixture exists"))
    }

    fn memory_backend(kinds: Option<&str>) -> String {
        let mut text = "[[backends]]\nname = 'main'\nkind = 'memory'\n".to_string();
        if let Some(kinds) = kinds {
            text.push_str(&format!("served_kinds = [{kinds}]\n"));
        }
        text
    }

    #[test]
    fn printed_backend_example_validates_without_editing() {
        load_case(&memory_backend(None)).expect("ordinary backend control");
        let source = include_str!("engine_config.rs");
        let before_struct = source
            .split_once("pub struct BackendConfig {")
            .expect("real BackendConfig declaration")
            .0;
        let block = before_struct
            .rsplit_once("/// ```toml\n")
            .expect("actual BackendConfig TOML example")
            .1
            .split_once("/// ```")
            .expect("example fence closes")
            .0;
        let example = block
            .lines()
            .map(|line| line.strip_prefix("/// ").expect("doc example line"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: KhiveConfig = toml::from_str(&example).expect("printed example is TOML");
        assert_eq!(parsed.backends.len(), 1, "example extraction control");
        eprintln!("BACKEND_BASELINE doc_example parsed_one_backend");
        load_case(&example).expect("printed BackendConfig example must validate unchanged");
    }

    #[test]
    fn unsupported_tuning_stays_explicitly_rejected() {
        load_case(&memory_backend(None)).expect("ordinary backend control");
        for (field, value) in [("cache_mb", "128"), ("journal_mode", "'wal'")] {
            let text = format!("{}{field} = {value}\n", memory_backend(None));
            let error = load_case(&text).expect_err("unsupported tuning must be refused");
            assert!(matches!(
                config_error_root(&error),
                ConfigError::UnsupportedBackendField { name, field: actual }
                    if name == "main" && *actual == field
            ));
        }
    }

    #[test]
    fn implicit_main_accepts_default_and_explicit_main_routes() {
        let default = load_case("").expect("implicit main with no pack overrides");
        assert!(default.backends.is_empty());
        let explicit = load_case("[packs.comm]\nbackend = 'main'\n")
            .expect("explicit reference to implicit main");
        assert_eq!(explicit.packs["comm"].backend, "main");
    }

    #[test]
    fn implicit_main_rejects_unknown_pack_route() {
        load_case("[packs.comm]\nbackend = 'main'\n").expect("known route control");
        let explicit_error = load_case(&format!(
            "{}[packs.comm]\nbackend = 'missing'\n",
            memory_backend(None)
        ))
        .expect_err("explicit topology already rejects unknown routes");
        assert!(matches!(
            config_error_root(&explicit_error),
            ConfigError::UnknownPackBackend { pack, backend, .. }
                if pack == "comm" && backend == "missing"
        ));
        eprintln!("BACKEND_BASELINE implicit_route controls_passed");
        let error = load_case("[packs.comm]\nbackend = 'missing'\n")
            .expect_err("implicit main must reject an unknown pack route");
        assert!(matches!(
            config_error_root(&error),
            ConfigError::UnknownPackBackend { pack, backend, defined }
                if pack == "comm" && backend == "missing" && defined == "main"
        ));
    }

    fn missing_coverage(served: &str, missing: &[&str]) {
        load_case(&memory_backend(Some("'note', 'entity'")))
            .expect("complete searchable coverage control");
        let empty = load_case(&memory_backend(Some("")))
            .expect_err("empty declaration control");
        assert!(matches!(
            config_error_root(&empty),
            ConfigError::EmptyBackendServedKinds { name } if name == "main"
        ));
        eprintln!("BACKEND_BASELINE coverage {served} controls_passed");
        let error = load_case(&memory_backend(Some(served)))
            .expect_err("declared topology must cover note and entity search");
        // Existing-API witness: new enum variants are checked only after the fix.
        let diagnostic = config_error_root(&error).to_string().to_lowercase();
        assert!(diagnostic.contains("main"), "backend identity: {diagnostic}");
        for kind in missing {
            assert!(diagnostic.contains(kind), "missing {kind}: {diagnostic}");
        }
    }

    #[test]
    fn note_only_topology_rejects_missing_entity() {
        missing_coverage("'note'", &["entity"]);
    }

    #[test]
    fn entity_only_topology_rejects_missing_note() {
        missing_coverage("'entity'", &["note"]);
    }

    #[test]
    fn event_only_topology_rejects_both_search_kinds() {
        missing_coverage("'event'", &["note", "entity"]);
    }

    #[test]
    fn split_coverage_and_event_only_secondary_are_valid() {
        let text = "[[backends]]\nname = 'main'\nkind = 'memory'\nserved_kinds = ['entity']\n\
                    [[backends]]\nname = 'notes'\nkind = 'memory'\nserved_kinds = ['note']\n\
                    [[backends]]\nname = 'events'\nkind = 'memory'\nserved_kinds = ['event']\n";
        let config = load_case(text).expect("coverage is the union, not per-backend");
        assert_eq!(config.backends.len(), 3);
    }

    #[test]
    fn omitted_served_kinds_retains_conservative_coverage() {
        let text = format!(
            "{}[[backends]]\nname = 'events'\nkind = 'memory'\nserved_kinds = ['event']\n",
            memory_backend(None)
        );
        let config = load_case(&text).expect("omission conservatively covers searches");
        assert!(config.backends[0].served_kinds.is_none());
    }
}
