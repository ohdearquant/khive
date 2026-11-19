/// A backend path is caller-controlled data, so the legacy `:read_only`
/// suffix must never be confusable with literal path text. Before this
/// regression, these two distinct archive backends produced the same
/// topology string and could therefore share the wrong warm daemon:
///
/// - read-only `/.../archive.db`
/// - writable `/.../archive.db:read_only`
#[test]
#[serial_test::serial(config_ledger)]
fn config_id_does_not_confuse_read_only_mode_with_a_path_suffix() {
    use khive_runtime::{BackendConfig, BackendId, BackendKind, KhiveConfig, PackConfig};

    let dir = tempfile::tempdir().expect("topology collision tempdir");
    let main_path = dir.path().join("main.db");
    let archive_path = dir.path().join("archive.db");
    let literal_suffix_path = dir.path().join("archive.db:read_only");
    let runtime = RuntimeConfig {
        db_path: Some(main_path.clone()),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        ..RuntimeConfig::no_embeddings()
    };

    let topology = |path, read_only| KhiveConfig {
        backends: vec![
            BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path.clone()),
                cache_mb: None,
                journal_mode: None,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
                served_kinds: None,
                read_only: false,
            },
            BackendConfig {
                name: "archive".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(path),
                cache_mb: None,
                journal_mode: None,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
                served_kinds: None,
                read_only,
            },
        ],
        packs: std::collections::HashMap::from([(
            "knowledge".to_string(),
            PackConfig {
                backend: "archive".to_string(),
                verbs_disabled: Vec::new(),
                no_embed: false,
            },
        )]),
        ..KhiveConfig::default()
    };

    let read_only_archive = topology(archive_path, true);
    let writable_literal_suffix = topology(literal_suffix_path, false);

    assert_ne!(
        compute_config_id(&runtime, Some(&read_only_archive)),
        compute_config_id(&runtime, Some(&writable_literal_suffix)),
        "backend mode must be encoded as a field, not an ambiguous path suffix"
    );
}
