#[cfg(any(unix, windows))]
mod file_identity_tests {
    use super::*;
    use khive_db::file_identity::database_file_identity;
    use khive_mcp::serve::{configured_storage_check_targets, migrate_configured_storage_topology};

    fn topology(
        main: &std::path::Path,
        alias: &std::path::Path,
        secondary: &std::path::Path,
    ) -> KhiveConfig {
        toml::from_str(&format!(
            "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n\
             [[backends]]\nname = 'alias'\nkind = 'sqlite'\npath = {:?}\n\
             [[backends]]\nname = 'secondary'\nkind = 'sqlite'\npath = {:?}\n",
            main.display().to_string(),
            alias.display().to_string(),
            secondary.display().to_string(),
        ))
        .expect("valid three-backend topology")
    }

    fn physical_planner_fixture(dir: &std::path::Path) -> KhiveConfig {
        let main = dir.join("main.db");
        let alias = dir.join("main-alias.db");
        let secondary = dir.join("secondary.db");
        std::fs::write(&main, b"identity fixture").expect("main fixture file");
        std::fs::hard_link(&main, &alias).expect("physical main alias");
        std::fs::write(&secondary, b"identity fixture").expect("independent fixture file");
        assert_ne!(
            std::fs::canonicalize(&main).unwrap(),
            std::fs::canonicalize(&alias).unwrap(),
            "hardlink paths must remain distinct after canonicalization"
        );
        assert_eq!(
            database_file_identity(&main).unwrap(),
            database_file_identity(&alias).unwrap()
        );
        assert_ne!(
            database_file_identity(&main).unwrap(),
            database_file_identity(&secondary).unwrap()
        );
        topology(&main, &alias, &secondary)
    }

    fn write_alias_topology(
        dir: &std::path::Path,
        main: &std::path::Path,
        alias: &std::path::Path,
        secondary: &std::path::Path,
    ) -> PathBuf {
        let path = write_topology_config(dir, main, Some(("alias", alias)));
        let mut content = std::fs::read_to_string(&path).expect("existing topology config");
        content.push_str(&format!(
            "\n[[backends]]\nname = 'secondary'\nkind = 'sqlite'\npath = {:?}\n",
            secondary.display().to_string()
        ));
        std::fs::write(&path, content).expect("write independent secondary config");
        path
    }

    fn checkpoint_fixture(path: &std::path::Path) {
        let backend = khive_db::StorageBackend::sqlite_for_test(path).expect("fixture backend");
        {
            let writer = backend.pool().try_writer().expect("checkpoint writer");
            writer
                .conn()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .expect("checkpoint fixture before hardlink inspection");
        }
        drop(backend);
    }

    fn current_main_with_legacy_secondary(
        dir: &std::path::Path,
    ) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let main = dir.join("main.db");
        let alias = dir.join("main-alias.db");
        let secondary = dir.join("secondary.db");
        let backend = khive_db::StorageBackend::sqlite_for_test(&main).expect("main backend");
        backend.prepare_core_schema().expect("current main schema");
        drop(backend);
        create_v20_fixture(&secondary, None);
        checkpoint_fixture(&main);
        checkpoint_fixture(&secondary);
        std::fs::hard_link(&main, &alias).expect("main hardlink alias");
        let config = write_alias_topology(dir, &main, &alias, &secondary);
        (main, alias, secondary, config)
    }

    fn snapshot_bytes(paths: &[PathBuf]) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        paths
            .iter()
            .flat_map(|path| {
                ["", "-wal", "-shm"].into_iter().map(move |suffix| {
                    let mut name = path.as_os_str().to_os_string();
                    name.push(suffix);
                    let file = PathBuf::from(name);
                    let bytes = if file.exists() {
                        Some(std::fs::read(&file).expect("snapshot fixture bytes"))
                    } else {
                        None
                    };
                    (file, bytes)
                })
            })
            .collect()
    }

    #[test]
    fn storage_check_hardlink_main_alias_includes_every_secondary() {
        let tmp = TempDir::new().expect("private planner fixture");
        let config = physical_planner_fixture(tmp.path());
        let all = vec![
            "main".to_string(),
            "alias".to_string(),
            "secondary".to_string(),
        ];
        assert_eq!(
            configured_storage_check_targets(&config, None, None).unwrap(),
            all
        );
        let main_plan = configured_storage_check_targets(&config, None, Some("main")).unwrap();
        assert_eq!(main_plan, all);
        assert_eq!(
            configured_storage_check_targets(&config, None, Some("alias")).unwrap(),
            main_plan,
            "selecting a physical alias of main retains every prerequisite"
        );
        assert_eq!(
            configured_storage_check_targets(&config, None, Some("secondary")).unwrap(),
            vec!["secondary".to_string()],
            "an independent secondary remains a single target"
        );
    }

    #[test]
    fn storage_check_forced_memory_keeps_hardlink_names_independent() {
        let tmp = TempDir::new().expect("private planner fixture");
        let config = physical_planner_fixture(tmp.path());
        assert_eq!(
            configured_storage_check_targets(&config, Some(":memory:"), Some("main")).unwrap(),
            vec![
                "main".to_string(),
                "alias".to_string(),
                "secondary".to_string()
            ]
        );
        for target in ["alias", "secondary"] {
            assert_eq!(
                configured_storage_check_targets(&config, Some(":memory:"), Some(target)).unwrap(),
                vec![target.to_string()],
                "forced memory creates a separate ephemeral database for {target}"
            );
        }
    }

    #[test]
    fn storage_check_explicit_memory_alias_remains_a_single_target() {
        let config: KhiveConfig = toml::from_str(
            "[[backends]]\nname = 'main'\nkind = 'memory'\n\
             [[backends]]\nname = 'alias'\nkind = 'memory'\n\
             [[backends]]\nname = 'secondary'\nkind = 'memory'\n",
        )
        .expect("explicit memory topology");
        assert_eq!(
            configured_storage_check_targets(&config, None, Some("main")).unwrap(),
            vec![
                "main".to_string(),
                "alias".to_string(),
                "secondary".to_string()
            ]
        );
        assert_eq!(
            configured_storage_check_targets(&config, None, Some("alias")).unwrap(),
            vec!["alias".to_string()]
        );
    }

    #[test]
    fn storage_check_missing_paths_keeps_canonical_alias_plan_without_creation() {
        let tmp = TempDir::new().expect("private missing-file planner fixture");
        let main = tmp.path().join("missing-main.db");
        let secondary = tmp.path().join("missing-secondary.db");
        let config = topology(&main, &main, &secondary);
        assert_eq!(
            configured_storage_check_targets(&config, None, Some("alias")).unwrap(),
            vec![
                "main".to_string(),
                "alias".to_string(),
                "secondary".to_string()
            ]
        );
        assert_eq!(
            configured_storage_check_targets(&config, None, Some("secondary")).unwrap(),
            vec!["secondary".to_string()]
        );
        assert!(!main.exists());
        assert!(!secondary.exists());
    }

    #[tokio::test]
    async fn db_check_hardlink_main_alias_rejects_legacy_secondary_without_mutation() {
        let tmp = TempDir::new().expect("private schema fixture");
        let (main, alias, secondary, config) = current_main_with_legacy_secondary(tmp.path());
        let paths = vec![main.clone(), alias.clone(), secondary.clone()];
        #[cfg(unix)]
        for path in &paths {
            khive_storage::test_support::freeze_snapshot_sidecars(path);
        }
        let latest = khive_db::MIGRATIONS
            .last()
            .expect("registered migrations")
            .version;
        assert_eq!(khive_db::inspect_schema_version(&main).unwrap(), latest);
        assert_eq!(khive_db::inspect_schema_version(&alias).unwrap(), latest);
        assert_eq!(khive_db::inspect_schema_version(&secondary).unwrap(), 20);
        for path in [&main, &alias] {
            khive_db::inspect_schema_is_current(path).expect("main and alias must be current");
        }
        let before = snapshot_bytes(&paths);

        let error = cmd_db_check(DbCheckArgs {
            db: None,
            config: Some(config),
            backend: Some("alias".to_string()),
            strict: true,
            human: false,
        })
        .await
        .expect_err("checking a current main alias must include the legacy secondary");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("secondary: V20"), "{rendered}");
        assert!(
            rendered.contains("schema topology is not current"),
            "{rendered}"
        );
        assert_eq!(
            snapshot_bytes(&paths),
            before,
            "schema check must preserve databases and frozen sidecar bytes"
        );
    }

    #[tokio::test]
    async fn db_migrate_hardlink_main_alias_marks_shared_targets_and_secondary_prerequisite() {
        let tmp = TempDir::new().expect("private migration fixture");
        let (main, alias, secondary, config) = current_main_with_legacy_secondary(tmp.path());
        let context =
            resolve_db_command_context(None, Some(config.as_path())).expect("schema admin config");
        let statuses = migrate_configured_storage_topology(
            context.base_config,
            &context.khive_config,
            context.cli_db_override.as_deref(),
            Some("alias"),
        )
        .await
        .expect("physical main alias retains the full migration topology");
        assert_eq!(statuses.len(), 3, "{statuses:?}");
        let latest = khive_db::MIGRATIONS
            .last()
            .expect("registered migrations")
            .version;
        for name in ["main", "alias", "secondary"] {
            let status = statuses
                .iter()
                .find(|status| status.backend == name)
                .expect("configured backend status");
            assert_eq!(status.applied_version, latest, "{status:?}");
            assert_eq!(status.prerequisite, name == "secondary", "{status:?}");
        }
        assert_eq!(
            database_file_identity(&main).unwrap(),
            database_file_identity(&alias).unwrap()
        );
        assert_ne!(
            database_file_identity(&main).unwrap(),
            database_file_identity(&secondary).unwrap()
        );
    }
}
