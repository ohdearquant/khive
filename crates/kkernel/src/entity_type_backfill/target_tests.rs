use super::*;

fn args(config: PathBuf) -> EntityTypeBackfillArgs {
    EntityTypeBackfillArgs {
        dry_run: true,
        apply: false,
        db: None,
        config: Some(config),
        namespace: Some("backfill-test".into()),
        limit: None,
    }
}

fn fleet_config(dir: &std::path::Path, route: Option<&str>) -> PathBuf {
    let main = dir.join("main.db");
    let other = dir.join("other.db");
    std::fs::write(&main, b"main fixture; resolver must not open SQLite").unwrap();
    std::fs::write(&other, b"other fixture; resolver must not open SQLite").unwrap();
    let mut config = format!(
        "[[backends]]\nname = \"main\"\npath = {:?}\n\
             [[backends]]\nname = \"other\"\npath = {:?}\n\
             [[backends]]\nname = \"sessions\"\npath = {:?}\n\
             [[backends]]\nname = \"comm\"\npath = {:?}\n\
             [[backends]]\nname = \"knowledge\"\npath = {:?}\n\
             [packs.session]\nbackend = \"sessions\"\n\
             [packs.comm]\nbackend = \"comm\"\n\
             [packs.knowledge]\nbackend = \"knowledge\"\n",
        main.to_str().unwrap(),
        other.to_str().unwrap(),
        dir.join("unopened/sessions.db").to_str().unwrap(),
        dir.join("unopened/comm.db").to_str().unwrap(),
        dir.join("unopened/knowledge.db").to_str().unwrap(),
    );
    if let Some(route) = route {
        config.push_str(&format!("[packs.kg]\nbackend = {route:?}\n"));
    }
    let path = dir.join("khive.toml");
    std::fs::write(&path, config).unwrap();
    path
}

#[test]
#[serial_test::serial]
fn resolve_target_uses_serving_route_without_opening_unrelated_backends() {
    for (route, expected) in [
        (None, "main"),
        (Some("main"), "main"),
        (Some("other"), "other"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config_path = fleet_config(dir.path(), route);
        let target = resolve_target(&args(config_path)).unwrap();
        assert_eq!(target.backend_name, expected);
        assert_eq!(
            target.path,
            dir.path()
                .join(format!("{expected}.db"))
                .canonicalize()
                .unwrap()
        );
        assert_eq!(target.config.db_path.as_ref(), Some(&target.path));
        assert_eq!(target.config.backend_id.as_str(), expected);
        assert_eq!(target.config.default_namespace.as_str(), "backfill-test");
        assert!(target.config.embedding_model.is_none());
        assert!(target.config.additional_embedding_models.is_empty());
        assert!(target.config.events_split.is_none());
        assert!(!dir.path().join("unopened").exists());
        assert_eq!(
            std::fs::read(dir.path().join("main.db")).unwrap(),
            b"main fixture; resolver must not open SQLite"
        );
        assert_eq!(
            std::fs::read(dir.path().join("other.db")).unwrap(),
            b"other fixture; resolver must not open SQLite"
        );
        target.reverify().unwrap();
    }
}

#[test]
#[serial_test::serial]
fn resolve_target_refuses_missing_ambiguous_and_unowned_targets() {
    let dir = tempfile::tempdir().unwrap();
    let path = fleet_config(dir.path(), Some("missing"));
    let error = resolve_target(&args(path)).err().unwrap();
    assert!(error
        .to_string()
        .starts_with("absent entity-type-backfill backend route"));
    assert!(
        format!("{error:#}").contains("defined backends: main, other, sessions, comm, knowledge")
    );

    let path = fleet_config(dir.path(), None);
    std::fs::remove_file(dir.path().join("main.db")).unwrap();
    assert!(resolve_target(&args(path))
        .err()
        .unwrap()
        .to_string()
        .contains("existing KG database"));
    assert!(!dir.path().join("main.db").exists());

    let path = fleet_config(dir.path(), None);
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str("[[backends]]\nname = \"main\"\npath = \"duplicate.db\"\n");
    std::fs::write(&path, config).unwrap();
    let error = resolve_target(&args(path)).err().unwrap();
    assert!(error
        .to_string()
        .starts_with("ambiguous entity-type-backfill target configuration"));
    assert!(format!("{error:#}").contains("duplicate backend name"));

    let path = fleet_config(dir.path(), None);
    let mut request = args(path);
    request.db = Some(dir.path().join("other.db").to_str().unwrap().into());
    assert!(
        resolve_target(&request).is_err(),
        "ordinary serving override guard must reject a non-main override"
    );
    assert!(!dir.path().join("unopened").exists());
}

#[test]
#[serial_test::serial]
fn resolve_target_single_backend_requires_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("khive.toml");
    std::fs::write(&config, "").unwrap();
    let database = dir.path().join("single.db");
    std::fs::write(&database, b"existing fixture").unwrap();
    let mut request = args(config);
    request.db = Some(database.to_str().unwrap().into());
    let target = resolve_target(&request).unwrap();
    assert_eq!(target.path, database.canonicalize().unwrap());
    assert_eq!(target.backend_name, "main");
    request.db = Some(":memory:".into());
    assert!(resolve_target(&request).is_err());
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn resolve_target_pins_symlink_and_rejects_replaced_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("khive.toml");
    std::fs::write(&config, "").unwrap();
    let database = dir.path().join("first.db");
    let second = dir.path().join("second.db");
    let alias = dir.path().join("alias.db");
    std::fs::write(&database, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();
    std::os::unix::fs::symlink(&database, &alias).unwrap();
    let mut request = args(config);
    request.db = Some(alias.to_str().unwrap().into());
    let target = resolve_target(&request).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&second, &alias).unwrap();
    assert_eq!(target.path, database.canonicalize().unwrap());
    target.reverify().unwrap();
    std::fs::rename(&second, &database).unwrap();
    assert!(format!("{:#}", target.reverify().unwrap_err()).contains("changed identity"));
}
