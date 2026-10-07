//! Selected-backend migration identity checks at the actual schema leaf.

use super::targeted_migration::migrate_selected_storage_backend_with;
use super::*;
use std::os::unix::fs::symlink;
use std::path::Path;

fn sqlite_config(name: &str, path: &Path) -> BackendConfig {
    BackendConfig {
        name: name.to_string(),
        kind: BackendKind::Sqlite,
        path: Some(path.to_path_buf()),
        cache_mb: None,
        journal_mode: None,
        wal_ceiling_bytes: None,
        disk_reserve_bytes: None,
        disk_guard_deadline_ms: None,
        served_kinds: None,
        read_only: false,
    }
}

fn config(backends: Vec<BackendConfig>) -> KhiveConfig {
    KhiveConfig {
        backends,
        ..KhiveConfig::default()
    }
}

fn base_config() -> RuntimeConfig {
    RuntimeConfig {
        db_path: None,
        wal_ceiling_env_raw: None,
        ..RuntimeConfig::no_embeddings()
    }
}

fn assert_no_core_schema(path: &Path) {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("inspect the file created by the opener without creating another file");
    assert_eq!(
        khive_db::migrations::read_schema_version(&connection).unwrap(),
        0,
        "refusal must happen before any core migration"
    );
    let core_tables: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name IN ('_schema_migrations', 'entities', 'notes', 'graph_edges')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(core_tables, 0, "refusal must not install a core schema");
}

fn assert_current(path: &Path) {
    assert_eq!(
        khive_db::migrations::inspect_schema_is_current(path).unwrap(),
        khive_db::migrations::latest_schema_version()
    );
}

#[tokio::test]
async fn fresh_selected_file_collapse_refuses_before_schema_migration() {
    let directory = tempfile::tempdir().unwrap();
    let main = directory.path().join("main.db");
    let selected_path = directory.path().join("archive.db");
    let topology = config(vec![
        sqlite_config("main", &main),
        sqlite_config("archive", &selected_path),
    ]);
    let plan = plan_configured_storage_targets(&topology, None, Some("archive")).unwrap();
    assert!(
        !plan.full_topology,
        "both paths are absent at planning time"
    );
    assert!(!main.exists() && !selected_path.exists());
    let base = base_config();
    validate_wal_ceiling_topology(&base, &plan.effective_backends, false).unwrap();
    let selected = &plan.effective_backends[1];
    let mut opens = 0;
    let mut collapsed = false;
    let result = migrate_selected_storage_backend_with(
        &base,
        &plan.effective_backends,
        selected,
        |backend_config, max_readers, policy| {
            assert_eq!(backend_config.name, "archive");
            opens += 1;
            let backend = open_backend_with_wal_ceiling(backend_config, max_readers, policy)?;
            assert_eq!(backend.schema_version().unwrap(), 0);
            std::fs::hard_link(&selected_path, &main).unwrap();
            assert_eq!(file_identity(&selected_path), file_identity(&main));
            assert!(file_identity(&main).is_some());
            collapsed = true;
            Ok(backend)
        },
    )
    .await;
    assert_eq!(opens, 1, "only the selected backend may be opened");
    assert!(
        collapsed,
        "the fixture must reach the post-open identity collapse"
    );
    let error = result.expect_err("newly collapsed aliases must refuse targeted migration");
    let message = error.to_string();
    assert!(message.contains("after the topology snapshot"), "{message}");
    assert!(
        message.contains("archive") && message.contains("main"),
        "{message}"
    );
    assert_no_core_schema(&selected_path);
    assert_no_core_schema(&main);
}

#[tokio::test]
async fn targeted_migration_refuses_absent_case_aliases_on_case_insensitive_filesystems() {
    let directory = tempfile::tempdir().unwrap();
    let probe = directory.path().join("CaseProbe");
    std::fs::write(&probe, b"case probe").unwrap();
    if !directory.path().join("caseprobe").exists() {
        eprintln!("NOT APPLICABLE: temporary filesystem is case-sensitive; deterministic hardlink-collapse coverage remains required");
        return;
    }
    std::fs::remove_file(probe).unwrap();
    let main = directory.path().join("Main.db");
    let selected = directory.path().join("main.db");
    let topology = config(vec![
        sqlite_config("main", &main),
        sqlite_config("archive", &selected),
    ]);
    assert!(!main.exists() && !selected.exists());
    assert!(
        !plan_configured_storage_targets(&topology, None, Some("archive"))
            .unwrap()
            .full_topology
    );

    let error =
        migrate_configured_storage_topology(base_config(), &topology, None, Some("archive"))
            .await
            .expect_err("the public targeted path must refuse newly materialized case aliases");
    assert!(
        error.to_string().contains("after the topology snapshot"),
        "{error:#}"
    );
    assert!(file_identity(&main).is_some());
    assert_eq!(file_identity(&main), file_identity(&selected));
    assert_no_core_schema(&selected);
}

#[tokio::test]
async fn targeted_independent_backend_migrates_without_opening_other_targets() {
    let directory = tempfile::tempdir().unwrap();
    let main = directory.path().join("main.db");
    let selected = directory.path().join("archive.db");
    let unused = directory.path().join("unused/nested/other.db");
    let topology = config(vec![
        sqlite_config("main", &main),
        sqlite_config("archive", &selected),
        sqlite_config("unused", &unused),
    ]);
    let statuses =
        migrate_configured_storage_topology(base_config(), &topology, None, Some("archive"))
            .await
            .expect("an independent selected backend must still migrate");
    assert_eq!(statuses.len(), 1, "only the requested backend is migrated");
    assert_eq!(statuses[0].backend, "archive");
    assert!(!statuses[0].prerequisite);
    assert_eq!(
        statuses[0].applied_version,
        khive_db::migrations::latest_schema_version()
    );
    assert_current(&selected);
    assert!(!main.exists(), "an independent main must not be opened");
    assert!(
        !unused.parent().unwrap().exists(),
        "unselected parents must not be created"
    );
}

#[tokio::test]
async fn targeted_known_hardlink_aliases_remain_accepted() {
    let directory = tempfile::tempdir().unwrap();
    let main = directory.path().join("main.db");
    let selected = directory.path().join("archive.db");
    let known_alias = directory.path().join("archive-alias.db");
    drop(rusqlite::Connection::open(&selected).unwrap());
    std::fs::hard_link(&selected, &known_alias).unwrap();
    assert!(file_identity(&selected).is_some());
    assert_eq!(file_identity(&selected), file_identity(&known_alias));
    let topology = config(vec![
        sqlite_config("main", &main),
        sqlite_config("archive", &selected),
        sqlite_config("known-alias", &known_alias),
    ]);
    let plan = plan_configured_storage_targets(&topology, None, Some("archive")).unwrap();
    assert!(
        !plan.full_topology,
        "this fixture must use the selected path"
    );
    let statuses =
        migrate_configured_storage_topology(base_config(), &topology, None, Some("archive"))
            .await
            .expect("aliases known before opening must not be treated as a new collapse");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].backend, "archive");
    assert_current(&selected);
    assert_current(&known_alias);
    assert!(!main.exists());
}

#[tokio::test]
async fn targeted_selected_path_retarget_refuses_before_schema_migration() {
    let directory = tempfile::tempdir().unwrap();
    let main = directory.path().join("main.db");
    let original = directory.path().join("original.db");
    let replacement = directory.path().join("replacement.db");
    let selected_path = directory.path().join("archive.db");
    for path in [&original, &replacement] {
        drop(rusqlite::Connection::open(path).unwrap());
    }
    let original_identity = file_identity(&original).unwrap();
    let replacement_identity = file_identity(&replacement).unwrap();
    assert_ne!(original_identity, replacement_identity);
    symlink(&original, &selected_path).unwrap();
    let topology = config(vec![
        sqlite_config("main", &main),
        sqlite_config("archive", &selected_path),
    ]);
    let plan = plan_configured_storage_targets(&topology, None, Some("archive")).unwrap();
    assert!(!plan.full_topology);
    let mut retargeted = false;
    let result = migrate_selected_storage_backend_with(
        &base_config(),
        &plan.effective_backends,
        &plan.effective_backends[1],
        |backend_config, max_readers, policy| {
            let backend = open_backend_with_wal_ceiling(backend_config, max_readers, policy)?;
            assert_eq!(
                backend.pool().opened_file_identity_record(),
                Some(original_identity)
            );
            std::fs::remove_file(&selected_path).unwrap();
            symlink(&replacement, &selected_path).unwrap();
            assert_eq!(file_identity(&selected_path), Some(replacement_identity));
            retargeted = true;
            Ok(backend)
        },
    )
    .await;
    assert!(
        retargeted,
        "the fixture must retarget after SQLite pins the original file"
    );
    let error = result.expect_err("selected path retargeting must fail before schema migration");
    assert!(error.to_string().contains("identity changed"), "{error:#}");
    assert_no_core_schema(&original);
    assert_no_core_schema(&replacement);
    assert!(!main.exists());
}

#[tokio::test]
async fn targeted_force_memory_override_does_not_create_configured_alias_paths() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing/nested/shared.db");
    let topology = config(vec![
        sqlite_config("main", &path),
        sqlite_config("archive", &path),
    ]);
    let statuses = migrate_configured_storage_topology(
        base_config(),
        &topology,
        Some(":memory:"),
        Some("archive"),
    )
    .await
    .expect("forced-memory names are separate ephemeral backends");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].backend, "archive");
    assert_eq!(
        statuses[0].applied_version,
        khive_db::migrations::latest_schema_version()
    );
    assert!(
        !path.parent().unwrap().exists(),
        "memory override must not materialize file paths"
    );
}

#[tokio::test]
async fn targeted_read_only_missing_backend_does_not_create_its_parent() {
    let directory = tempfile::tempdir().unwrap();
    let main = directory.path().join("main.db");
    let selected = directory.path().join("missing/nested/snapshot.db");
    let mut snapshot = sqlite_config("snapshot", &selected);
    snapshot.read_only = true;
    let topology = config(vec![sqlite_config("main", &main), snapshot]);
    let error =
        migrate_configured_storage_topology(base_config(), &topology, None, Some("snapshot"))
            .await
            .expect_err("a missing read-only selected backend must refuse opening");
    assert!(error.to_string().contains("read-only open"), "{error:#}");
    assert!(!selected.parent().unwrap().exists());
    assert!(!main.exists());
}

#[tokio::test]
async fn targeted_whole_topology_wal_preflight_refuses_before_selected_open() {
    let directory = tempfile::tempdir().unwrap();
    let main = directory.path().join("main.db");
    let selected = directory.path().join("archive.db");
    let mut first = sqlite_config("main", &main);
    let mut alias = sqlite_config("main-alias", &main);
    first.wal_ceiling_bytes = Some(0);
    alias.wal_ceiling_bytes = Some(8 * 1024 * 1024);
    let topology = config(vec![first, sqlite_config("archive", &selected), alias]);
    let error =
        migrate_configured_storage_topology(base_config(), &topology, None, Some("archive"))
            .await
            .expect_err("unselected aliases must still pass whole-topology WAL preflight");
    assert!(
        matches!(
            error.downcast_ref::<khive_runtime::ConfigError>(),
            Some(khive_runtime::ConfigError::WalCeilingAliasConflict { .. })
        ),
        "{error:#}"
    );
    assert!(error.to_string().contains("WAL ceiling"), "{error:#}");
    assert!(
        !selected.exists(),
        "policy refusal must precede the selected opener"
    );
    assert!(!main.exists());
}

#[tokio::test]
async fn targeted_selected_leaf_passes_its_resolved_wal_policy() {
    let directory = tempfile::tempdir().unwrap();
    let main = directory.path().join("main.db");
    let selected_path = directory.path().join("archive.db");
    let mut selected = sqlite_config("archive", &selected_path);
    selected.wal_ceiling_bytes = Some(0);
    let topology = config(vec![sqlite_config("main", &main), selected]);
    let plan = plan_configured_storage_targets(&topology, None, Some("archive")).unwrap();
    let base = base_config();
    validate_wal_ceiling_topology(&base, &plan.effective_backends, false).unwrap();
    let expected = wal_ceiling_policy_for_backend(&base, &plan.effective_backends[1]).unwrap();
    assert_eq!(expected.bytes, 0);
    assert_eq!(expected.source, khive_db::WalCeilingSource::BackendField);
    let mut opens = 0;
    let status = migrate_selected_storage_backend_with(
        &base,
        &plan.effective_backends,
        &plan.effective_backends[1],
        |backend_config, max_readers, policy| {
            opens += 1;
            assert_eq!(backend_config.name, "archive");
            assert_eq!(max_readers, None);
            let backend = open_backend_with_wal_ceiling(backend_config, max_readers, policy)?;
            assert_eq!(
                policy, expected,
                "selected migration must retain its resolved WAL policy"
            );
            assert_eq!(backend.pool().config().wal_ceiling, expected);
            Ok(backend)
        },
    )
    .await
    .unwrap();
    assert_eq!(opens, 1);
    assert_eq!(status.backend, "archive");
    assert_current(&selected_path);
    assert!(!main.exists());
}
