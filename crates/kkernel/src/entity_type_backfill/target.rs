//! Side-effect-free selection of the database that serves KG operations.

use std::path::PathBuf;

use anyhow::{ensure, Context, Result};
use khive_mcp::serve::{
    capture_existing_database_target, config_discovery_db_anchor,
    reject_conflicting_db_override_with_source, resolve_pack_backend_config,
    resolve_runtime_config, reverify_reindex_target_identity,
    validate_effective_backend_alias_modes, RuntimeConfigInputs, ValidatedReindexTarget,
};
use khive_runtime::{BackendId, BackendKind, ConfigError, KhiveConfig, Namespace, RuntimeConfig};

use super::EntityTypeBackfillArgs;

pub(super) struct ResolvedTarget {
    pub(super) config: RuntimeConfig,
    pub(super) path: PathBuf,
    pub(super) backend_name: String,
    validated: ValidatedReindexTarget,
}

impl ResolvedTarget {
    pub(super) fn reverify(&self) -> Result<()> {
        ensure!(
            self.path == self.validated.path && self.config.db_path.as_ref() == Some(&self.path),
            "entity-type-backfill target no longer matches the resolved database path"
        );
        ensure!(
            self.path.is_file(),
            "entity-type-backfill target {} is no longer an existing file",
            self.path.display()
        );
        reverify_reindex_target_identity(&self.validated)
            .context("entity-type-backfill target identity verification failed")
    }
}

pub(super) fn resolve_target(args: &EntityTypeBackfillArgs) -> Result<ResolvedTarget> {
    let anchor = config_discovery_db_anchor(args.db.as_deref());
    let loaded =
        KhiveConfig::load_with_home_fallback_and_source(args.config.as_deref(), anchor.as_deref())
            .map_err(target_config_error)?;
    let source = loaded.as_ref().map(|(_, source)| source.as_path());
    let khive_config = loaded
        .as_ref()
        .map(|(config, _)| config.clone())
        .unwrap_or_default();
    reject_conflicting_db_override_with_source(args.db.as_deref(), &khive_config.backends, source)?;

    let namespace = Namespace::parse(args.namespace.as_deref().unwrap_or("local"))?;
    let mut config = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace,
        namespace_explicit: args.namespace.is_some(),
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })?;
    ensure!(
        config.db_path.is_some(),
        "entity-type-backfill requires an existing file-backed database, not :memory:"
    );
    ensure!(
        config.packs.iter().any(|pack| pack == "kg"),
        "entity-type-backfill requires the kg pack"
    );
    validate_effective_backend_alias_modes(&khive_config.backends)?;

    let (backend_name, path) = if khive_config.backends.is_empty() {
        (
            BackendId::MAIN.to_string(),
            config.db_path.clone().expect("checked file-backed config"),
        )
    } else {
        let (backend, _) = resolve_pack_backend_config(&khive_config, "kg").with_context(|| {
            format!(
                "resolve KG backend from {}",
                source
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "discovered configuration".into())
            )
        })?;
        ensure!(
            backend.kind == BackendKind::Sqlite,
            "KG backend {:?} is not file-backed SQLite",
            backend.name
        );
        ensure!(
            !args.apply || !backend.read_only,
            "KG backend {:?} is read_only; --apply requires a writable target",
            backend.name
        );
        (
            backend.name.clone(),
            backend
                .path
                .clone()
                .with_context(|| format!("KG backend {:?} has no database path", backend.name))?,
        )
    };
    let validated = capture_existing_database_target(&path)
        .with_context(|| format!("resolve existing KG database for backend {backend_name:?}"))?;
    let path = validated.path.clone();
    config.db_path = Some(path.clone());
    config.backend_id = BackendId::parse(&backend_name)?;
    config.embedding_model = None;
    config.additional_embedding_models.clear();
    // This detached command must not open the discovery anchor's event plane.
    config.events_split = None;
    let target = ResolvedTarget {
        config,
        path,
        backend_name,
        validated,
    };
    target.reverify()?;
    Ok(target)
}

fn target_config_error(error: ConfigError) -> anyhow::Error {
    let mut cause = &error;
    while let ConfigError::InFile { source, .. } = cause {
        cause = source;
    }
    let context = match cause {
        ConfigError::DuplicateBackendName { .. } => {
            "ambiguous entity-type-backfill target configuration"
        }
        ConfigError::UnknownPackBackend { .. } => "absent entity-type-backfill backend route",
        ConfigError::ExplicitConfigMissing { .. } => {
            "absent entity-type-backfill target configuration"
        }
        _ => "invalid entity-type-backfill target configuration",
    };
    anyhow::Error::new(error).context(context)
}

#[cfg(test)]
mod tests {
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
        assert!(format!("{error:#}")
            .contains("defined backends: main, other, sessions, comm, knowledge"));

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
}
