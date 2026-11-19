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
    config.disable_embedding_models();
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
#[path = "target_tests.rs"]
mod tests;
