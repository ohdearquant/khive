//! Offline, registry-driven repair of legacy entity subtype properties.
//!
//! The copied database is the classification population. Apply revalidates each
//! full preimage against the writable store; it never blindly replaces a row
//! changed since the scan. Stop other writers while taking the filesystem copy.

use crate::sql::sql;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use khive_runtime::curation::EntityPatch;
use khive_runtime::pack::{IngestAuditStore, PackRegistry, VerbRegistry};
use khive_runtime::{KhiveRuntime, NamespaceToken};
use khive_storage::entity::{Entity, EntityFilter};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::{to_snake_case, EntityKind, EntityTypeError, EntityTypeRegistry};
use serde::Serialize;
use uuid::Uuid;

const PAGE_SIZE: u32 = 256;
const MAX_SCAN: u64 = 1_000_000;

mod target;

#[derive(Debug, Parser)]
#[command(group(clap::ArgGroup::new("mode").required(true).args(["dry_run", "apply"])))]
#[command(
    long_about = "Repair legacy entity subtype properties using the configured pack registry. Both modes require stopped writers while the database and sidecars are copied; this is an offline maintenance command, not a live-store migration. Dry-run writes nothing to the source. Limited runs report complete=false when the namespace scan is unfinished."
)]
pub struct EntityTypeBackfillArgs {
    /// Classify without modifying the database or its sidecars. Stop other writers first.
    #[arg(long)]
    pub dry_run: bool,
    /// Apply guarded updates. Stop other writers before running this offline command.
    #[arg(long)]
    pub apply: bool,
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,
    #[arg(long, env = "KHIVE_CONFIG")]
    pub config: Option<PathBuf>,
    #[arg(long, env = "KHIVE_NAMESPACE")]
    pub namespace: Option<String>,
    /// Maximum live entities visited, including ineligible rows (default 1000000).
    /// A limited prefix is not a complete namespace repair; inspect complete.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_SCAN))]
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct BackfillFailure {
    pub id: Option<Uuid>,
    pub error: String,
    /// Normal runtime updates can persist before indexing or audit fails.
    pub write_may_have_committed: bool,
}

#[derive(Debug, Serialize)]
pub struct EntityTypeBackfillReport {
    pub mode: &'static str,
    pub namespace: String,
    pub source_revision: &'static str,
    pub loaded_packs: Vec<String>,
    pub registry_types: BTreeMap<String, Vec<String>>,
    pub target: PathBuf,
    pub backend: String,
    pub count_scope: &'static str,
    pub effective_limit: u64,
    pub scanned: u64,
    pub eligible: u64,
    pub promote: u64,
    pub echo: u64,
    pub untouched: u64,
    pub wrong_kind: u64,
    pub nullable_before: u64,
    pub nullable_after: Option<u64>,
    pub projected_nullable_after: u64,
    pub after_count_basis: &'static str,
    pub promoted: u64,
    pub echo_removed: u64,
    pub complete: bool,
    pub failures: Vec<BackfillFailure>,
}

enum Classification<'a> {
    Ineligible,
    Promote(&'a str),
    Echo,
    Untouched { wrong_kind: bool },
}

fn classify<'a>(entity: &'a Entity, registry: &EntityTypeRegistry) -> Classification<'a> {
    if entity.entity_type.is_some() || entity.deleted_at.is_some() {
        return Classification::Ineligible;
    }
    let Some(raw) = entity
        .properties
        .as_ref()
        .and_then(|props| props.get("type"))
        .and_then(serde_json::Value::as_str)
    else {
        return Classification::Ineligible;
    };
    if to_snake_case(raw.trim()) == to_snake_case(entity.kind.trim()) {
        return Classification::Echo;
    }
    let Ok(kind) = entity.kind.parse::<EntityKind>() else {
        return Classification::Untouched { wrong_kind: false };
    };
    match registry.resolve(kind, Some(raw)) {
        // Deliberately pass RAW to the normal write path. Its installed
        // validator, not this read-side classification, must canonicalize it.
        Ok(_) => Classification::Promote(raw),
        Err(EntityTypeError::WrongKind { .. }) => Classification::Untouched { wrong_kind: true },
        Err(_) => Classification::Untouched { wrong_kind: false },
    }
}

fn compose_registry(runtime: &KhiveRuntime) -> Result<(VerbRegistry, EntityTypeRegistry)> {
    ensure!(
        runtime.config().packs.iter().any(|name| name == "kg"),
        "entity-type-backfill requires the kg pack"
    );
    let registry = PackRegistry::build_ingest_registry(runtime, IngestAuditStore::Detach)?;
    registry.call_register_entity_type_validators(runtime);
    let types = EntityTypeRegistry::with_extra(registry.all_entity_types());
    Ok((registry, types))
}

fn validate_args(args: &EntityTypeBackfillArgs) -> Result<()> {
    ensure!(
        args.dry_run != args.apply,
        "specify exactly one of --dry-run or --apply"
    );
    ensure!(
        args.limit
            .is_none_or(|limit| (1..=MAX_SCAN).contains(&limit)),
        "--limit must be between 1 and {MAX_SCAN}"
    );
    Ok(())
}

async fn nullable_count(runtime: &KhiveRuntime, namespace: &str) -> Result<u64> {
    let mut reader = runtime.sql().reader().await?;
    match reader
        .query_scalar(SqlStatement {
            sql: sql!("entities_untyped_count").into(),
            params: vec![SqlValue::Text(namespace.into())],
            label: Some("entity-type-backfill nullable census".into()),
        })
        .await?
    {
        Some(SqlValue::Integer(count)) if count >= 0 => Ok(count as u64),
        _ => bail!("entity-type-backfill census did not return a nonnegative integer"),
    }
}

async fn scan(
    snapshot: &KhiveRuntime,
    token: &NamespaceToken,
    types: &EntityTypeRegistry,
    apply: Option<&KhiveRuntime>,
    report: &mut EntityTypeBackfillReport,
    page_size: u32,
) -> Result<()> {
    let entities = snapshot.entities(token)?;
    let write_token = apply
        .map(|runtime| runtime.authorize(token.namespace().clone()))
        .transpose()?;
    let mut cursor = None;
    loop {
        let remaining = report.effective_limit - report.scanned;
        if remaining == 0 {
            return Ok(());
        }
        let page = entities
            .query_entities_after(
                &report.namespace,
                EntityFilter::default(),
                cursor,
                u32::try_from(remaining.min(u64::from(page_size)))?,
            )
            .await?;
        cursor = page.next_after;
        for entity in page.items {
            report.scanned += 1;
            let class = classify(&entity, types);
            let (patch, removals): (EntityPatch, &[&str]) = match class {
                Classification::Ineligible => continue,
                Classification::Promote(raw) => {
                    report.eligible += 1;
                    report.promote += 1;
                    (
                        EntityPatch {
                            entity_type: Some(Some(raw.to_string())),
                            ..Default::default()
                        },
                        &[],
                    )
                }
                Classification::Echo => {
                    report.eligible += 1;
                    report.echo += 1;
                    (EntityPatch::default(), &["type"])
                }
                Classification::Untouched { wrong_kind } => {
                    report.eligible += 1;
                    report.untouched += 1;
                    report.wrong_kind += u64::from(wrong_kind);
                    continue;
                }
            };
            if let Some((runtime, token)) = apply.zip(write_token.as_ref()) {
                if let Err(error) = runtime
                    .update_entity_if_unchanged(token, &entity, patch, removals)
                    .await
                {
                    report.failures.push(BackfillFailure {
                        id: Some(entity.id),
                        error: error.to_string(),
                        write_may_have_committed: true,
                    });
                    return Ok(());
                }
                if removals.is_empty() {
                    report.promoted += 1;
                } else {
                    report.echo_removed += 1;
                }
            }
        }
        if cursor.is_none() {
            report.complete = true;
            return Ok(());
        }
    }
}

/// Run against a private classification copy, and optionally guarded live writes.
/// `--apply` is an offline operator operation, never a live-store migration.
pub async fn entity_type_backfill(
    args: &EntityTypeBackfillArgs,
) -> Result<EntityTypeBackfillReport> {
    validate_args(args)?;
    let target = target::resolve_target(args)?;
    backfill_resolved(args, &target).await
}

async fn backfill_resolved(
    args: &EntityTypeBackfillArgs,
    target: &target::ResolvedTarget,
) -> Result<EntityTypeBackfillReport> {
    target.reverify()?;
    let config = &target.config;
    let path = &target.path;
    let (backend, _snapshot_dir) = crate::code_ingest::open_read_only_snapshot(path)?;
    target.reverify()?;
    // Validate current schema on the read-only copy before any writable open.
    backend.prepare_core_schema()?;
    let snapshot = KhiveRuntime::from_backend(Arc::new(backend), config.clone());
    let (registry, types) = compose_registry(&snapshot)?;
    let namespace = config.default_namespace.clone();
    let token = snapshot.authorize(namespace.clone())?;
    let before = nullable_count(&snapshot, namespace.as_str()).await?;
    let mut loaded_packs: Vec<_> = registry
        .pack_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    loaded_packs.sort();
    let mut report = EntityTypeBackfillReport {
        mode: if args.apply { "apply" } else { "dry_run" },
        namespace: namespace.as_str().to_owned(),
        source_revision: khive_runtime::BUILD_INFO.source_revision,
        loaded_packs,
        registry_types: EntityKind::ALL.into_iter().map(|kind| {
            let mut names: Vec<_> = types.definitions().iter().filter(|def| def.kind == kind)
                .map(|def| def.type_name.to_owned()).collect();
            names.sort();
            names.dedup();
            (kind.name().to_owned(), names)
        }).collect(),
        target: path.to_path_buf(),
        backend: target.backend_name.clone(),
        count_scope: "live entities in one exact namespace; classification and before count from a private offline copy",
        effective_limit: args.limit.unwrap_or(MAX_SCAN),
        scanned: 0, eligible: 0, promote: 0, echo: 0, untouched: 0, wrong_kind: 0,
        nullable_before: before, nullable_after: Some(before), projected_nullable_after: before,
        after_count_basis: "unchanged dry-run snapshot",
        promoted: 0, echo_removed: 0, complete: false, failures: Vec::new(),
    };
    if !args.apply {
        scan(&snapshot, &token, &types, None, &mut report, PAGE_SIZE).await?;
    } else {
        target.reverify()?;
        let runtime = KhiveRuntime::new(config.clone())?;
        let mut write_registry = None;
        let result: Result<()> = async {
            ensure!(
                !runtime.is_read_only(),
                "--apply requires a writable database"
            );
            write_registry = Some(compose_registry(&runtime)?.0);
            scan(
                &snapshot,
                &token,
                &types,
                Some(&runtime),
                &mut report,
                PAGE_SIZE,
            )
            .await
        }
        .await;
        if let Err(error) = result {
            report.failures.push(BackfillFailure {
                id: None,
                error: error.to_string(),
                write_may_have_committed: report.promoted + report.echo_removed > 0,
            });
        }
        report.after_count_basis =
            "independent observed post-apply census; not an atomic before/after transaction";
        match nullable_count(&runtime, namespace.as_str()).await {
            Ok(after) => report.nullable_after = Some(after),
            Err(error) => {
                report.nullable_after = None;
                report.failures.push(BackfillFailure {
                    id: None,
                    error: format!("after census failed: {error}"),
                    write_may_have_committed: true,
                });
            }
        }
        drop(write_registry);
        let join = runtime.backend().pool().take_writer_task_join();
        let missing_join = join.is_none()
            && runtime.backend().pool().write_queue_active()
            && runtime.backend().pool().writer_task_join_was_stored();
        drop(runtime);
        let drain_error = if missing_join {
            Some("writer task drain ownership unavailable".to_string())
        } else if let Some(join) = join {
            match tokio::time::timeout(Duration::from_secs(30), join).await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("writer task failed: {error}")),
                Err(_) => Some(
                    "writer task did not drain in 30s; database state may still be unsettled"
                        .into(),
                ),
            }
        } else {
            None
        };
        if let Some(error) = drain_error {
            report.failures.push(BackfillFailure {
                id: None,
                error,
                write_may_have_committed: true,
            });
        }
    }
    report.projected_nullable_after = before - report.promote;
    report.complete &= report.failures.is_empty();
    Ok(report)
}

pub async fn run_entity_type_backfill(args: EntityTypeBackfillArgs) -> Result<()> {
    use std::io::Write;

    validate_args(&args)?;
    let target = target::resolve_target(&args)?;
    println!("target: {}", target.path.display());
    std::io::stdout()
        .flush()
        .context("print resolved backfill target")?;
    let report = backfill_resolved(&args, &target).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(report.failures.is_empty(), "entity-type-backfill stopped with failures; inspect the report before retrying (writes may have committed)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::RuntimeConfig;
    use serde_json::json;

    #[test]
    fn classifier_uses_composed_types_and_preserves_raw_promotion_for_validation() {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: Vec::new(),
            packs: vec!["kg".into(), "git".into()],
            ..RuntimeConfig::default()
        })
        .unwrap();
        let (_registry, types) = compose_registry(&runtime).unwrap();
        for (kind, raw, canonical) in [
            ("document", "paper", "paper"),
            ("concept", " ALGO ", "algorithm"),
            ("document", " Architecture--Decision__Record ", "adr"),
        ] {
            let entity =
                Entity::new("local", kind, "fixture").with_properties(json!({"type": raw}));
            assert!(
                matches!(classify(&entity, &types), Classification::Promote(value) if value == raw)
            );
            assert_eq!(
                types
                    .resolve(kind.parse().unwrap(), Some(raw))
                    .unwrap()
                    .entity_type
                    .as_deref(),
                Some(canonical)
            );
        }
        let echo = Entity::new("local", "concept", "echo")
            .with_properties(json!({"type": " __CoNcEpT-- "}));
        assert!(matches!(classify(&echo, &types), Classification::Echo));
        let wrong =
            Entity::new("local", "concept", "wrong").with_properties(json!({"type": "Article"}));
        assert!(matches!(
            classify(&wrong, &types),
            Classification::Untouched { wrong_kind: true }
        ));
        let unknown = Entity::new("local", "document", "unknown")
            .with_properties(json!({"type": "unregistered-backfill-test"}));
        assert!(matches!(
            classify(&unknown, &types),
            Classification::Untouched { wrong_kind: false }
        ));
        for properties in [
            None,
            Some(json!({"type": null})),
            Some(json!({"type": 7})),
            Some(json!({"type": ["paper"]})),
        ] {
            let mut entity = Entity::new("local", "document", "ineligible");
            entity.properties = properties;
            assert!(matches!(
                classify(&entity, &types),
                Classification::Ineligible
            ));
        }
        let typed = Entity::new("local", "document", "typed")
            .with_entity_type(Some("report"))
            .with_properties(json!({"type": "paper"}));
        assert!(matches!(
            classify(&typed, &types),
            Classification::Ineligible
        ));
        let mut deleted =
            Entity::new("local", "document", "deleted").with_properties(json!({"type": "paper"}));
        deleted.deleted_at = Some(1);
        assert!(matches!(
            classify(&deleted, &types),
            Classification::Ineligible
        ));
    }
}
