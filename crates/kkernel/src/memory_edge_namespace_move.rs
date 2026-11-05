//! Offline repair of live memory-to-memory edge namespaces.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use clap::Parser;
use khive_db::backend::NamespaceLiveness;
use khive_mcp::serve::{
    capture_existing_database_target, config_discovery_db_anchor,
    reject_conflicting_db_override_with_source, resolve_pack_backend_config,
    resolve_runtime_config, reverify_reindex_target_identity,
    validate_effective_backend_alias_modes, RuntimeConfigInputs, ValidatedReindexTarget,
};
use khive_runtime::curation::EdgeListFilter;
use khive_runtime::{
    BackendId, BackendKind, ConfigError, KhiveConfig, KhiveRuntime, Namespace, NamespaceToken,
    Resolved, RuntimeConfig,
};
use serde::Serialize;
use uuid::Uuid;

const PAGE_SIZE: u32 = 256;

#[derive(Debug, Parser)]
#[command(
    long_about = "Preview memory-edge namespace repair without changing the source database. Dry run is the default; --apply moves live memory-to-memory edges with per-edge revision guards. Stop other writers for the entire offline repair, including while the database and sidecars are copied. Applying can stop after partial progress; inspect moved and remaining counts before retrying. Soft-deleted edges are not counted or moved."
)]
pub struct MemoryEdgeNamespaceMoveArgs {
    /// Move misplaced live edges. Without this flag, print a dry-run projection.
    #[arg(long)]
    pub apply: bool,
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,
    #[arg(long, env = "KHIVE_CONFIG")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct EdgeNamespaceCounts {
    pub namespace: String,
    pub relation: String,
    pub liveness: &'static str,
    pub before: u64,
    pub after: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct MoveFailure {
    pub id: Option<Uuid>,
    pub error: String,
    pub write_may_have_committed: bool,
}

#[derive(Debug, Serialize)]
pub struct MemoryEdgeNamespaceMoveReport {
    pub mode: &'static str,
    pub source_revision: &'static str,
    pub target: PathBuf,
    pub backend: String,
    pub count_scope: &'static str,
    pub after_basis: &'static str,
    pub atomicity: &'static str,
    pub deleted_edge_policy: &'static str,
    pub counts: Vec<EdgeNamespaceCounts>,
    pub candidates: u64,
    pub collision_groups: u64,
    pub collision_rows: u64,
    pub apply_blocked: bool,
    pub excluded_edges: u64,
    pub moved: u64,
    /// Current misplaced eligible rows: BEFORE for a preview, observed AFTER for apply.
    /// None means the independent AFTER census failed.
    pub remaining: Option<u64>,
    pub candidate_inventory: &'static str,
    pub complete: bool,
    pub failures: Vec<MoveFailure>,
}

struct ResolvedTarget {
    config: RuntimeConfig,
    path: PathBuf,
    backend_name: String,
    validated: ValidatedReindexTarget,
}

impl ResolvedTarget {
    fn reverify(&self) -> Result<()> {
        ensure!(
            self.path == self.validated.path && self.config.db_path.as_ref() == Some(&self.path),
            "memory-edge-namespace-move target no longer matches the resolved database path"
        );
        ensure!(
            self.path.is_file(),
            "memory-edge-namespace-move target {} is no longer an existing file",
            self.path.display()
        );
        reverify_reindex_target_identity(&self.validated)
            .context("memory-edge-namespace-move target identity verification failed")
    }
}

fn resolve_target(args: &MemoryEdgeNamespaceMoveArgs) -> Result<ResolvedTarget> {
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
    let mut config = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace: Namespace::parse("local")?,
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })?;
    ensure!(
        config.db_path.is_some(),
        "memory-edge-namespace-move requires an existing file-backed database, not :memory:"
    );
    ensure!(
        config.packs.iter().any(|pack| pack == "kg"),
        "memory-edge-namespace-move requires the kg pack"
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
            "ambiguous memory-edge-namespace-move target configuration"
        }
        ConfigError::UnknownPackBackend { .. } => "absent memory-edge-namespace-move backend route",
        ConfigError::ExplicitConfigMissing { .. } => {
            "absent memory-edge-namespace-move target configuration"
        }
        _ => "invalid memory-edge-namespace-move target configuration",
    };
    anyhow::Error::new(error).context(context)
}

fn open_snapshot(target: &ResolvedTarget) -> Result<(KhiveRuntime, tempfile::TempDir)> {
    target.reverify()?;
    let (backend, directory) = crate::code_ingest::open_read_only_snapshot(&target.path)?;
    target.reverify()?;
    backend.prepare_core_schema()?;
    let runtime = KhiveRuntime::from_backend(Arc::new(backend), target.config.clone());
    ensure!(runtime.is_read_only(), "dry-run snapshot must be read-only");
    Ok((runtime, directory))
}

type CountKey = (String, String);
type ProjectedKey = (String, Uuid, Uuid, String);

#[derive(Default)]
struct Census {
    counts: BTreeMap<CountKey, u64>,
    candidates: u64,
    excluded_edges: u64,
}

struct ProjectedGroup {
    rows: u64,
    destination_exists: bool,
}

enum EndpointClass {
    MemoryPair { source_namespace: String },
    Excluded,
}

async fn classify_endpoints(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source_id: Uuid,
    target_id: Uuid,
) -> Result<EndpointClass> {
    let source = runtime
        .resolve_by_id_including_deleted(token, source_id)
        .await?;
    let target = runtime
        .resolve_by_id_including_deleted(token, target_id)
        .await?;
    Ok(match (source, target) {
        (Some(Resolved::Note(source)), Some(Resolved::Note(target)))
            if source.kind == "memory" && target.kind == "memory" =>
        {
            EndpointClass::MemoryPair {
                source_namespace: source.namespace.as_str().to_owned(),
            }
        }
        _ => EndpointClass::Excluded,
    })
}

fn increment<K: Ord>(counts: &mut BTreeMap<K, u64>, key: K) -> Result<()> {
    let count = counts.entry(key).or_default();
    *count = count.checked_add(1).context("edge census count overflow")?;
    Ok(())
}

fn count_rows(
    before: &BTreeMap<CountKey, u64>,
    after: Option<&BTreeMap<CountKey, u64>>,
) -> Vec<EdgeNamespaceCounts> {
    let mut keys: BTreeSet<_> = before.keys().cloned().collect();
    if let Some(after) = after {
        keys.extend(after.keys().cloned());
    }
    keys.into_iter()
        .map(|(namespace, relation)| {
            let key = (namespace.clone(), relation.clone());
            EdgeNamespaceCounts {
                namespace,
                relation,
                liveness: "live",
                before: before.get(&key).copied().unwrap_or(0),
                after: after.map(|counts| counts.get(&key).copied().unwrap_or(0)),
            }
        })
        .collect()
}

async fn census(runtime: &KhiveRuntime) -> Result<Census> {
    let mut result = Census::default();
    for namespace in runtime
        .backend()
        .list_edge_namespaces(NamespaceLiveness::Live)
        .await?
    {
        let token = runtime.authorize(Namespace::parse(&namespace)?)?;
        let mut cursor = None;
        loop {
            let (edges, next) = runtime
                .list_edges_after(&token, EdgeListFilter::default(), cursor, PAGE_SIZE)
                .await?;
            for edge in edges {
                if edge.deleted_at.is_some() {
                    continue;
                }
                match classify_endpoints(runtime, &token, edge.source_id, edge.target_id).await? {
                    EndpointClass::MemoryPair { source_namespace } => {
                        increment(
                            &mut result.counts,
                            (edge.namespace.clone(), edge.relation.to_string()),
                        )?;
                        if edge.namespace != source_namespace {
                            result.candidates = result
                                .candidates
                                .checked_add(1)
                                .context("candidate count overflow")?;
                        }
                    }
                    EndpointClass::Excluded => {
                        result.excluded_edges = result
                            .excluded_edges
                            .checked_add(1)
                            .context("excluded-edge count overflow")?;
                    }
                }
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
    }
    Ok(result)
}

async fn move_on_runtime(
    runtime: &KhiveRuntime,
    target: &ResolvedTarget,
    applying: bool,
) -> Result<MemoryEdgeNamespaceMoveReport> {
    let mut before = Census::default();
    let mut projected = BTreeMap::new();
    let mut groups: BTreeMap<ProjectedKey, ProjectedGroup> = BTreeMap::new();
    let mut candidates = Vec::new();

    // Finish every page and every collision check before moving even one edge.
    // The pager revalidates its cursor namespace: mutating a cursor while scanning
    // would invalidate the next page. Retain only candidate rows, not all edges.
    for namespace in runtime
        .backend()
        .list_edge_namespaces(NamespaceLiveness::Live)
        .await?
    {
        let token = runtime.authorize(Namespace::parse(&namespace)?)?;
        let mut cursor = None;
        loop {
            let (edges, next) = runtime
                .list_edges_after(&token, EdgeListFilter::default(), cursor, PAGE_SIZE)
                .await?;
            for edge in edges {
                if edge.deleted_at.is_some() {
                    continue;
                }
                let EndpointClass::MemoryPair { source_namespace } =
                    classify_endpoints(runtime, &token, edge.source_id, edge.target_id).await?
                else {
                    before.excluded_edges = before
                        .excluded_edges
                        .checked_add(1)
                        .context("excluded-edge count overflow")?;
                    continue;
                };
                let relation = edge.relation.to_string();
                increment(
                    &mut before.counts,
                    (edge.namespace.clone(), relation.clone()),
                )?;
                // The source is the persisted canonical source, rather than a
                // caller-provided endpoint spelling. Memory symmetric relations
                // are not admitted by the current endpoint rules.
                increment(&mut projected, (source_namespace.clone(), relation.clone()))?;
                if edge.namespace == source_namespace {
                    continue;
                }
                before.candidates = before
                    .candidates
                    .checked_add(1)
                    .context("candidate count overflow")?;
                let key = (
                    source_namespace.clone(),
                    edge.source_id,
                    edge.target_id,
                    relation,
                );
                if let Some(group) = groups.get_mut(&key) {
                    group.rows = group
                        .rows
                        .checked_add(1)
                        .context("collision-row count overflow")?;
                } else {
                    let destination_token =
                        runtime.authorize(Namespace::parse(&source_namespace)?)?;
                    let destination_exists = runtime
                        .get_edge_by_natural_key_including_deleted(
                            &destination_token,
                            &source_namespace,
                            edge.source_id,
                            edge.target_id,
                            edge.relation,
                        )
                        .await?
                        .is_some();
                    groups.insert(
                        key,
                        ProjectedGroup {
                            rows: 1,
                            destination_exists,
                        },
                    );
                }
                if applying {
                    candidates.push((edge, source_namespace));
                }
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
    }

    let mut collision_groups = 0_u64;
    let mut collision_rows = 0_u64;
    for group in groups.values() {
        if group.rows > 1 || group.destination_exists {
            collision_groups = collision_groups
                .checked_add(1)
                .context("collision-group count overflow")?;
            collision_rows = collision_rows
                .checked_add(group.rows)
                .context("collision-row count overflow")?;
        }
    }
    drop(groups);
    let mut report = MemoryEdgeNamespaceMoveReport {
        mode: if applying { "apply" } else { "dry_run" },
        source_revision: khive_runtime::BUILD_INFO.source_revision,
        target: target.path.clone(),
        backend: target.backend_name.clone(),
        count_scope: "live edges whose endpoints both resolve, including deleted notes, as notes of kind memory; excluded_edges is the BEFORE count of every other live edge",
        after_basis: if applying { "measured" } else { "projected" },
        atomicity: "per-edge revision-guarded updates; complete collision preflight before writes; not one transaction",
        deleted_edge_policy: "soft-deleted edges are not counted or moved",
        counts: count_rows(&before.counts, Some(if applying || collision_groups > 0 { &before.counts } else { &projected })),
        candidates: before.candidates,
        collision_groups,
        collision_rows,
        apply_blocked: collision_groups > 0,
        excluded_edges: before.excluded_edges,
        moved: 0,
        remaining: Some(before.candidates),
        candidate_inventory: "O(C) candidate snapshots on apply and projected-key groups, plus census count groups; C is misplaced eligible live edges; 256-row pages do not cap total memory",
        complete: true,
        failures: Vec::new(),
    };
    if !applying {
        return Ok(report);
    }

    if collision_groups > 0 {
        report.failures.push(MoveFailure {
            id: None,
            error: "collision preflight refused the whole run before the first edge write".into(),
            write_may_have_committed: false,
        });
    } else {
        for (mut edge, source_namespace) in candidates {
            let id = Uuid::from(edge.id);
            let result: Result<()> = async {
                let expected_updated_at = edge.updated_at;
                let expected_deleted_at = edge.deleted_at;
                let token = runtime.authorize(Namespace::parse(&edge.namespace)?)?;
                // Recheck administrative authority for the destination as well.
                runtime.authorize(Namespace::parse(&source_namespace)?)?;
                let graph = runtime.graph(&token)?;
                edge.namespace = source_namespace;
                let minimum = expected_updated_at
                    .timestamp_micros()
                    .checked_add(1)
                    .with_context(|| format!("edge {id} revision cannot advance"))?;
                let now = chrono::Utc::now().timestamp_micros().max(minimum);
                edge.updated_at = chrono::DateTime::from_timestamp_micros(now)
                    .with_context(|| format!("edge {id} replacement timestamp is invalid"))?;
                ensure!(
                    graph
                        .replace_edge_if_unchanged(edge, expected_updated_at, expected_deleted_at)
                        .await?,
                    "edge {id} changed after preflight; stopped without replacing its newer state"
                );
                Ok(())
            }
            .await;
            match result {
                Ok(()) => report.moved += 1,
                Err(error) => {
                    report.failures.push(MoveFailure {
                        id: Some(id),
                        error: error.to_string(),
                        // Treat an ambiguous storage error conservatively. The
                        // independent AFTER census is the remaining-work witness.
                        write_may_have_committed: true,
                    });
                    break;
                }
            }
        }
    }

    // This is an independent observed census of the live store, not a subtraction
    // from the retained cohort and not an atomic before/after transaction.
    match census(runtime).await {
        Ok(after) => {
            report.counts = count_rows(&before.counts, Some(&after.counts));
            report.remaining = Some(after.candidates);
        }
        Err(error) => {
            report.counts = count_rows(&before.counts, None);
            report.remaining = None;
            report.failures.push(MoveFailure {
                id: None,
                error: format!("independent AFTER census failed: {error}"),
                write_may_have_committed: report.moved > 0,
            });
        }
    }
    if report.failures.is_empty() && report.remaining != Some(0) {
        report.failures.push(MoveFailure {
            id: None,
            error: "independent AFTER census still found misplaced eligible edges; rerun required (the run is not one transaction)".into(),
            write_may_have_committed: report.moved > 0,
        });
    }
    report.complete = report.failures.is_empty() && report.remaining == Some(0);
    Ok(report)
}

pub async fn memory_edge_namespace_move(
    args: &MemoryEdgeNamespaceMoveArgs,
) -> Result<MemoryEdgeNamespaceMoveReport> {
    let target = resolve_target(args)?;
    move_resolved(args, &target).await
}

async fn move_resolved(
    args: &MemoryEdgeNamespaceMoveArgs,
    target: &ResolvedTarget,
) -> Result<MemoryEdgeNamespaceMoveReport> {
    let applying = args.apply;
    // Validate the current schema on a private read-only copy before opening any
    // writable handle. The default path uses only this snapshot.
    let (snapshot, snapshot_directory) = open_snapshot(target)?;
    if !applying {
        let result = move_on_runtime(&snapshot, target, false).await;
        drop(snapshot);
        drop(snapshot_directory);
        return result;
    }
    drop(snapshot);
    drop(snapshot_directory);
    target.reverify()?;
    let runtime = KhiveRuntime::new(target.config.clone())?;
    let result = async {
        ensure!(
            !runtime.is_read_only(),
            "--apply requires a writable database"
        );
        move_on_runtime(&runtime, target, true).await
    }
    .await;
    let join = runtime.backend().pool().take_writer_task_join();
    let missing_join = join.is_none()
        && runtime.backend().pool().write_queue_active()
        && runtime.backend().pool().writer_task_join_was_stored();
    drop(runtime);
    let drain_error = if missing_join {
        Some("writer task drain ownership unavailable".to_owned())
    } else if let Some(join) = join {
        match tokio::time::timeout(Duration::from_secs(30), join).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("writer task failed: {error}")),
            Err(_) => Some(
                "writer task did not drain in 30s; database state may still be unsettled".into(),
            ),
        }
    } else {
        None
    };
    match result {
        Ok(mut report) => {
            if let Some(error) = drain_error {
                report.failures.push(MoveFailure {
                    id: None,
                    error,
                    write_may_have_committed: true,
                });
                report.complete = false;
            }
            Ok(report)
        }
        Err(error) => match drain_error {
            Some(drain) => Err(error.context(drain)),
            None => Err(error),
        },
    }
}

pub async fn run_memory_edge_namespace_move(args: MemoryEdgeNamespaceMoveArgs) -> Result<()> {
    use std::io::Write;

    let target = resolve_target(&args)?;
    println!("target: {}", target.path.display());
    println!("soft-deleted edges are not counted or moved");
    std::io::stdout()
        .flush()
        .context("print resolved edge repair target")?;
    let report = move_resolved(&args, &target).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(report.complete, "memory-edge-namespace-move stopped; inspect moved, remaining, and failures before retrying");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    use serde_json::{json, Value};

    const CHILD_ROOT: &str = "KKERNEL_EDGE_MOVE_CHILD_ROOT";
    const CHILD_TOKEN: &str = "KKERNEL_EDGE_MOVE_CHILD_TOKEN";
    const SOURCE: &str = "fixture-source";
    const NAMESPACES: &[&str] = &[SOURCE, "local", "wrong-a", "wrong-b", "clean-wrong"];

    struct ChildGuard(std::process::Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    fn args(root: &Path, apply: bool) -> MemoryEdgeNamespaceMoveArgs {
        MemoryEdgeNamespaceMoveArgs {
            apply,
            db: Some(
                root.join("stores/fixture.db")
                    .to_string_lossy()
                    .into_owned(),
            ),
            config: Some(root.join("fixture.toml")),
        }
    }

    async fn fixture_runtime(root: &Path) -> Result<KhiveRuntime> {
        fs::create_dir_all(root.join("stores"))?;
        fs::write(root.join("fixture.toml"), "engines = []\n")?;
        let input = args(root, false);
        let mut config = resolve_runtime_config(RuntimeConfigInputs {
            db: input.db.as_deref(),
            config: input.config.as_deref(),
            namespace: Namespace::parse("local")?,
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })?;
        config.disable_embedding_models();
        config.events_split = None;
        KhiveRuntime::new(config).map_err(Into::into)
    }

    async fn memory(
        runtime: &KhiveRuntime,
        namespace: &str,
        content: &str,
        seed_paths: &mut Vec<&'static str>,
    ) -> Result<Uuid> {
        let token = runtime.authorize(Namespace::parse(namespace)?)?;
        // This owner-qualified Runtime API bypasses the verb-layer kind hooks.
        // Every error stops setup. There is no fallback or retry after a possible
        // post-commit failure.
        let note = runtime
            .create_note(&token, "memory", None, content, Some(0.7), None, Vec::new())
            .await
            .expect("fixture create_note(memory) after KhiveRuntime::new(resolve_runtime_config with no_embed=true) failed; setup failure, never control credit");
        ensure!(note.kind == "memory", "fixture note kind is not memory");
        ensure!(
            note.namespace.as_str() == namespace,
            "fixture note namespace differs"
        );
        let id = note.id;
        ensure!(
            matches!(runtime.resolve_by_id_including_deleted(&token, id).await?,
                Some(Resolved::Note(stored)) if stored.kind == "memory" && stored.namespace.as_str() == namespace),
            "fixture memory note did not persist with its kind and namespace"
        );
        seed_paths.push("create_note");
        Ok(id)
    }

    async fn legacy_edge(
        runtime: &KhiveRuntime,
        namespace: &str,
        source: Uuid,
        target: Uuid,
        relation: &str,
        deleted: bool,
    ) -> Result<Uuid> {
        let id = Uuid::now_v7();
        let now = chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros())
            .context("fixture timestamp")?;
        // Edge's released serde representation supplies every field; inference
        // from upsert_edge keeps the storage type's public export path irrelevant.
        let edge = serde_json::from_value(json!({
            "id": id,
            "namespace": namespace,
            "source_id": source,
            "target_id": target,
            "relation": relation,
            "weight": 0.75,
            "created_at": now,
            "updated_at": now,
            "deleted_at": deleted.then_some(now),
            "metadata": {"fixture": "legacy-edge", "preserve": [1, 2, 3]},
            "target_backend": null,
        }))?;
        let token = runtime.authorize(Namespace::parse(namespace)?)?;
        runtime.graph(&token)?.upsert_edge(edge).await?;
        Ok(id)
    }

    async fn settle(runtime: KhiveRuntime) -> Result<()> {
        let join = runtime.backend().pool().take_writer_task_join();
        let missing = join.is_none()
            && runtime.backend().pool().write_queue_active()
            && runtime.backend().pool().writer_task_join_was_stored();
        drop(runtime);
        ensure!(!missing, "fixture writer drain ownership unavailable");
        if let Some(join) = join {
            tokio::time::timeout(Duration::from_secs(30), join)
                .await
                .context("fixture writer drain timed out")?
                .context("fixture writer task failed")?;
        }
        Ok(())
    }

    async fn records(root: &Path, ids: &[Uuid]) -> Result<Vec<Value>> {
        let target = resolve_target(&args(root, false))?;
        let (runtime, directory) = open_snapshot(&target)?;
        let mut records = Vec::new();
        for id in ids {
            let mut found = None;
            for namespace in NAMESPACES {
                let token = runtime.authorize(Namespace::parse(namespace)?)?;
                if let Ok(Some(edge)) = runtime.get_edge_including_deleted(&token, *id).await {
                    ensure!(
                        Uuid::from(edge.id) == *id,
                        "fixture edge lookup returned another id"
                    );
                    found = Some(serde_json::to_value(edge)?);
                    break;
                }
            }
            records.push(
                found.with_context(|| format!("fixture edge {id} missing, including deleted"))?,
            );
        }
        drop(runtime);
        drop(directory);
        Ok(records)
    }

    fn store_files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
        fn visit(
            base: &Path,
            directory: &Path,
            result: &mut BTreeMap<String, Vec<u8>>,
        ) -> Result<()> {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                ensure!(
                    !kind.is_symlink(),
                    "fixture store path must not be a symlink"
                );
                if kind.is_dir() {
                    visit(base, &entry.path(), result)?;
                } else if kind.is_file() {
                    result.insert(
                        entry
                            .path()
                            .strip_prefix(base)?
                            .to_string_lossy()
                            .into_owned(),
                        fs::read(entry.path())?,
                    );
                }
            }
            Ok(())
        }
        let base = root.join("stores");
        let mut files = BTreeMap::new();
        visit(&base, &base, &mut files)?;
        ensure!(files.contains_key("fixture.db"), "fixture database missing");
        Ok(files)
    }

    fn compare_store_files(
        before: BTreeMap<String, Vec<u8>>,
        after: BTreeMap<String, Vec<u8>>,
    ) -> Value {
        json!({
            "before": before.keys().collect::<Vec<_>>(),
            "after": after.keys().collect::<Vec<_>>(),
            "unchanged": before == after,
        })
    }

    async fn moving_child(root: &Path, case: &str) -> Result<Value> {
        let runtime = fixture_runtime(root).await?;
        let mut seed_paths = Vec::new();
        let source = memory(&runtime, SOURCE, "source memory", &mut seed_paths).await?;
        let deleted_source = case == "deleted-source";
        if deleted_source {
            let token = runtime.authorize(Namespace::parse(SOURCE)?)?;
            let deleted = runtime.delete_note(&token, source, false).await
                .expect("fixture delete_note(id, false) after KhiveRuntime::new failed; setup failure, never control credit");
            ensure!(deleted, "fixture source soft-delete returned false");
            ensure!(
                runtime.resolve_by_id(&token, source).await?.is_none(),
                "soft-deleted fixture source still resolves as live"
            );
            ensure!(
                matches!(runtime.resolve_by_id_including_deleted(&token, source).await?,
                    Some(Resolved::Note(note)) if note.kind == "memory" && note.namespace.as_str() == SOURCE),
                "soft-deleted fixture source did not retain memory kind and stored namespace"
            );
        }
        let count = if case == "paged" {
            PAGE_SIZE as usize + 1
        } else if deleted_source {
            1
        } else {
            6
        };
        let initially_correct = if case == "six" { 1 } else { 0 };
        let mut ids = Vec::new();
        let mut target_ids = Vec::new();
        for index in 0..count {
            let target = memory(
                &runtime,
                SOURCE,
                &format!("target memory {index}"),
                &mut seed_paths,
            )
            .await?;
            let namespace = if index < initially_correct {
                SOURCE
            } else {
                "local"
            };
            ids.push(legacy_edge(&runtime, namespace, source, target, "supersedes", false).await?);
            target_ids.push(target);
        }
        settle(runtime).await?;
        let before = records(root, &ids).await?;
        ensure!(before.len() == count, "positive baseline lost an edge");
        let correct = before
            .iter()
            .filter(|edge| edge["namespace"] == SOURCE)
            .count();
        ensure!(
            correct == initially_correct,
            "positive baseline source/local distribution is wrong"
        );
        ensure!(
            before
                .iter()
                .filter(|edge| edge["namespace"] == "local")
                .count()
                == count - initially_correct,
            "positive baseline local count is wrong"
        );
        for (edge, target) in before.iter().zip(target_ids) {
            ensure!(
                edge["source_id"] == json!(source)
                    && edge["target_id"] == json!(target)
                    && edge["relation"] == "supersedes",
                "positive baseline endpoints or relation differ"
            );
        }
        ensure!(
            before.iter().all(|edge| edge["deleted_at"].is_null()),
            "positive baseline has deleted edge"
        );
        let before_files = store_files(root)?;
        let dry = memory_edge_namespace_move(&args(root, false)).await?;
        ensure!(
            dry.complete,
            "default invocation did not complete on valid fixture"
        );
        let file_witness = compare_store_files(before_files, store_files(root)?);
        let dry_edges = records(root, &ids).await?;
        let applied = memory_edge_namespace_move(&args(root, true)).await?;
        ensure!(applied.complete, "apply did not complete on valid fixture");
        let after = records(root, &ids).await?;
        let rerun = memory_edge_namespace_move(&args(root, true)).await?;
        ensure!(rerun.complete, "idempotent rerun did not complete");
        Ok(json!({
            "baseline_valid": true,
            "seed_paths": seed_paths,
            "before_edges": before,
            "files": file_witness,
            "dry_edges": dry_edges,
            "dry": dry,
            "applied": applied,
            "after_edges": after,
            "rerun": rerun,
            "expected_candidates": count - initially_correct,
            "source_was_soft_deleted": deleted_source,
        }))
    }

    async fn collision_child(root: &Path, scenario: &str) -> Result<Value> {
        let runtime = fixture_runtime(root).await?;
        let mut seed_paths = Vec::new();
        let source = memory(&runtime, SOURCE, "collision source", &mut seed_paths).await?;
        let target = memory(&runtime, SOURCE, "collision target", &mut seed_paths).await?;
        let clean_source = memory(&runtime, SOURCE, "clean source", &mut seed_paths).await?;
        let clean_target = memory(&runtime, SOURCE, "clean target", &mut seed_paths).await?;
        let mut ids = vec![
            legacy_edge(
                &runtime,
                "clean-wrong",
                clean_source,
                clean_target,
                "supersedes",
                false,
            )
            .await?,
        ];
        ids.push(legacy_edge(&runtime, "wrong-a", source, target, "supersedes", false).await?);
        let expected_rows = if scenario == "projected" {
            ids.push(legacy_edge(&runtime, "wrong-b", source, target, "supersedes", false).await?);
            2
        } else {
            ids.push(
                legacy_edge(
                    &runtime,
                    SOURCE,
                    source,
                    target,
                    "supersedes",
                    scenario == "deleted",
                )
                .await?,
            );
            1
        };
        settle(runtime).await?;
        let before = records(root, &ids).await?;
        ensure!(
            before.len() == 3,
            "collision baseline lost a raw legacy or destination edge"
        );
        ensure!(
            before[0]["namespace"] == "clean-wrong" && before[1]["namespace"] == "wrong-a",
            "collision baseline namespaces differ"
        );
        ensure!(
            before[0]["source_id"] == json!(clean_source)
                && before[0]["target_id"] == json!(clean_target),
            "clean candidate baseline endpoints differ"
        );
        for edge in &before[1..] {
            ensure!(
                edge["source_id"] == json!(source)
                    && edge["target_id"] == json!(target)
                    && edge["relation"] == "supersedes",
                "collision baseline projected key differs"
            );
        }
        ensure!(
            before[2]["namespace"]
                == if scenario == "projected" {
                    "wrong-b"
                } else {
                    SOURCE
                },
            "collision destination namespace differs"
        );
        ensure!(
            before[0]["deleted_at"].is_null() && before[1]["deleted_at"].is_null(),
            "collision candidate baseline is not live"
        );
        if scenario == "deleted" {
            ensure!(
                !before[2]["deleted_at"].is_null(),
                "destination baseline is not deleted"
            );
        }
        let before_files = store_files(root)?;
        let dry = memory_edge_namespace_move(&args(root, false)).await?;
        ensure!(dry.complete, "collision preview did not complete");
        let file_witness = compare_store_files(before_files, store_files(root)?);
        let dry_edges = records(root, &ids).await?;
        let applied = memory_edge_namespace_move(&args(root, true)).await?;
        let after = records(root, &ids).await?;
        Ok(json!({
            "baseline_valid": true, "seed_paths": seed_paths,
            "before_edges": before, "files": file_witness,
            "dry_edges": dry_edges,
            "dry": dry, "applied": applied, "after_edges": after,
            "expected_rows": expected_rows,
        }))
    }

    async fn excluded_child(root: &Path) -> Result<Value> {
        let runtime = fixture_runtime(root).await?;
        let mut seed_paths = Vec::new();
        let source = memory(&runtime, SOURCE, "excluded source", &mut seed_paths).await?;
        let target = memory(&runtime, SOURCE, "deleted edge target", &mut seed_paths).await?;
        let token = runtime.authorize(Namespace::parse("local")?)?;
        let event = khive_storage::event::Event::new(
            "local",
            "fixture",
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Event,
            "edge-move-fixture",
        );
        let event_id = event.id;
        runtime.events(&token)?.append_event(event).await?;
        ensure!(
            matches!(
                runtime.resolve_edge_endpoint(&token, event_id).await?,
                Some(Resolved::Event(_))
            ),
            "event seed did not read back as an actual event"
        );
        let ids = vec![
            legacy_edge(&runtime, "local", source, event_id, "annotates", false).await?,
            legacy_edge(&runtime, "local", source, target, "supersedes", true).await?,
        ];
        settle(runtime).await?;
        let before = records(root, &ids).await?;
        ensure!(
            before[0]["deleted_at"].is_null() && !before[1]["deleted_at"].is_null(),
            "excluded/deleted baseline is invalid"
        );
        ensure!(
            before[0]["source_id"] == json!(source)
                && before[0]["target_id"] == json!(event_id)
                && before[0]["relation"] == "annotates",
            "excluded event baseline differs"
        );
        ensure!(
            before[1]["source_id"] == json!(source)
                && before[1]["target_id"] == json!(target)
                && before[1]["relation"] == "supersedes",
            "deleted edge baseline differs"
        );
        let before_files = store_files(root)?;
        let dry = memory_edge_namespace_move(&args(root, false)).await?;
        ensure!(dry.complete, "excluded preview did not complete");
        let file_witness = compare_store_files(before_files, store_files(root)?);
        let dry_edges = records(root, &ids).await?;
        let applied = memory_edge_namespace_move(&args(root, true)).await?;
        ensure!(applied.complete, "excluded apply did not complete");
        let after = records(root, &ids).await?;
        Ok(json!({
            "baseline_valid": true, "seed_paths": seed_paths,
            "before_edges": before, "files": file_witness,
            "dry_edges": dry_edges,
            "dry": dry, "applied": applied, "after_edges": after,
        }))
    }

    async fn chain_child(root: &Path) -> Result<Value> {
        let runtime = fixture_runtime(root).await?;
        let mut seed_paths = Vec::new();
        let source = memory(&runtime, SOURCE, "chain source memory", &mut seed_paths).await?;
        let mut ids = Vec::new();
        let mut target_ids = Vec::new();
        for index in 0..6 {
            let target = memory(
                &runtime,
                SOURCE,
                &format!("chain target {index}"),
                &mut seed_paths,
            )
            .await?;
            let namespace = if index == 0 { SOURCE } else { "local" };
            ids.push(legacy_edge(&runtime, namespace, source, target, "supersedes", false).await?);
            target_ids.push(target);
        }
        let reader = {
            let mut builder = khive_runtime::VerbRegistryBuilder::new();
            builder.with_actor_id(Some(SOURCE.to_string()));
            builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
            builder.build().expect("chain KgPack reader registry")
        };
        let before_anchor = reader
            .dispatch_with_disposition(
                "neighbors",
                json!({
                    "namespace": SOURCE, "id": source.to_string(), "direction": "both",
                    "relations": ["supersedes"],
                }),
                None,
            )
            .await
            .expect("actual source-only chain anchor read; invalid setup is not control credit");
        let before_default_anchor = reader
            .dispatch_with_disposition(
                "neighbors",
                json!({
                    "id": source.to_string(), "direction": "both", "relations": ["supersedes"],
                }),
                None,
            )
            .await
            .expect("actual default chain anchor contrast");
        let mut before_hidden_targets = Vec::new();
        for target in target_ids.iter().skip(1) {
            let response = reader
                .dispatch_with_disposition(
                    "neighbors",
                    json!({
                        "namespace": SOURCE, "id": target.to_string(), "direction": "in",
                        "relations": ["supersedes"],
                    }),
                    None,
                )
                .await
                .expect("actual source-only chain target read");
            before_hidden_targets.push(json!({"target_id": target, "response": response}));
        }
        drop(reader);
        settle(runtime).await?;
        let before = records(root, &ids).await?;
        ensure!(before.len() == 6, "chain fixture lost an edge");
        ensure!(
            before
                .iter()
                .filter(|edge| edge["namespace"] == SOURCE)
                .count()
                == 1,
            "chain fixture source namespace distribution is wrong"
        );
        ensure!(
            before
                .iter()
                .filter(|edge| edge["namespace"] == "local")
                .count()
                == 5,
            "chain fixture local namespace distribution is wrong"
        );
        for (edge, target) in before.iter().zip(&target_ids) {
            ensure!(
                edge["source_id"] == json!(source)
                    && edge["target_id"] == json!(target)
                    && edge["relation"] == "supersedes"
                    && edge["deleted_at"].is_null(),
                "chain fixture endpoints, relation or liveness differ"
            );
        }
        let before_files = store_files(root)?;
        let dry = memory_edge_namespace_move(&args(root, false)).await?;
        ensure!(dry.complete, "chain dry run did not complete");
        let file_witness = compare_store_files(before_files, store_files(root)?);
        let dry_edges = records(root, &ids).await?;
        let applied = memory_edge_namespace_move(&args(root, true)).await?;
        ensure!(applied.complete, "chain apply did not complete");
        let after = records(root, &ids).await?;
        let runtime = fixture_runtime(root).await?;
        let reader = {
            let mut builder = khive_runtime::VerbRegistryBuilder::new();
            builder.with_actor_id(Some(SOURCE.to_string()));
            builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
            builder.build().expect("chain post-apply reader registry")
        };
        let after_anchor = reader
            .dispatch_with_disposition(
                "neighbors",
                json!({
                    "namespace": SOURCE, "id": source.to_string(), "direction": "both",
                    "relations": ["supersedes"],
                }),
                None,
            )
            .await
            .expect("actual post-apply source-only chain read");
        drop(reader);
        settle(runtime).await?;
        let rerun = memory_edge_namespace_move(&args(root, true)).await?;
        ensure!(rerun.complete, "chain idempotent apply did not complete");
        Ok(json!({
            "baseline_valid": true, "seed_paths": seed_paths,
            "before_edges": before, "files": file_witness, "dry_edges": dry_edges,
            "dry": dry, "applied": applied, "after_edges": after, "rerun": rerun,
            "expected_candidates": 5, "source_was_soft_deleted": false,
            "before_anchor": before_anchor, "before_default_anchor": before_default_anchor,
            "before_hidden_targets": before_hidden_targets, "after_anchor": after_anchor,
            "target_ids": target_ids,
        }))
    }

    fn validate_child(case: &str, test_name: &str) -> Result<Option<PathBuf>> {
        let Some(root) = std::env::var_os(CHILD_ROOT) else {
            ensure!(
                std::env::var_os(CHILD_TOKEN).is_none(),
                "inherited child token without private fixture root"
            );
            return Ok(None);
        };
        let root = PathBuf::from(root)
            .canonicalize()
            .context("private child root must exist")?;
        ensure!(
            std::env::current_dir()?.canonicalize()? == root,
            "child cwd must be the parent's private fixture root"
        );
        let marker: Value = serde_json::from_slice(&fs::read(root.join("child-marker.json"))?)?;
        let token = std::env::var(CHILD_TOKEN).context("child token missing")?;
        ensure!(
            marker["token"].as_str() == Some(token.as_str()),
            "child marker token differs"
        );
        ensure!(
            marker["case"].as_str() == Some(case)
                && marker["test_name"].as_str() == Some(test_name),
            "child marker testcase differs"
        );
        let executable = std::env::current_exe()?.canonicalize()?;
        ensure!(
            marker["executable"].as_str() == Some(executable.to_string_lossy().as_ref()),
            "child marker executable differs"
        );
        ensure!(
            std::env::var_os("HOME").map(PathBuf::from) == Some(root.join("home")),
            "child HOME must be private"
        );
        Ok(Some(root))
    }

    fn assert_no_dry_run_mutation(receipt: &Value) {
        // This is intentionally the first semantic assertion after the parent has
        // qualified normal child exit, receipt, writer settlement and cleanup.
        assert_eq!(
            receipt["before_edges"], receipt["dry_edges"],
            "dry_run_leaves_every_edge_unchanged"
        );
        assert_eq!(
            receipt["files"]["before"], receipt["files"]["after"],
            "dry_run_leaves_every_store_filename_unchanged"
        );
        assert_eq!(
            receipt["files"]["unchanged"], true,
            "dry_run_leaves_every_store_file_unchanged"
        );
    }

    fn assert_applied_identity(receipt: &Value) {
        let before = receipt["before_edges"]
            .as_array()
            .expect("baseline records");
        let after = receipt["after_edges"].as_array().expect("after records");
        assert_eq!(before.len(), after.len());
        for (before, after) in before.iter().zip(after) {
            let moved = before["namespace"] != SOURCE;
            let mut expected = before.clone();
            let mut actual = after.clone();
            expected["namespace"] = json!(SOURCE);
            let expected_revision = expected
                .as_object_mut()
                .expect("edge object")
                .remove("updated_at")
                .expect("before revision");
            let actual_revision = actual
                .as_object_mut()
                .expect("edge object")
                .remove("updated_at")
                .expect("after revision");
            assert_eq!(expected, actual, "same_id_attributes_preserved");
            if moved {
                let before = chrono::DateTime::parse_from_rfc3339(
                    expected_revision.as_str().expect("revision text"),
                )
                .expect("before timestamp");
                let after = chrono::DateTime::parse_from_rfc3339(
                    actual_revision.as_str().expect("revision text"),
                )
                .expect("after timestamp");
                assert!(after > before, "moved_revision_strictly_advances");
            } else {
                assert_eq!(
                    expected_revision, actual_revision,
                    "correct_edge_is_untouched"
                );
            }
        }
    }

    async fn run_case(case: &str, test_name: &str) -> Result<()> {
        if let Some(root) = validate_child(case, test_name)? {
            // The private marker is validated before creating a config, store,
            // runtime, event, or any other fixture file.
            let receipt = match case {
                "six" | "paged" | "deleted-source" => moving_child(&root, case).await?,
                "projected" | "live" | "deleted" => collision_child(&root, case).await?,
                "excluded" => excluded_child(&root).await?,
                "chain" => chain_child(&root).await?,
                _ => anyhow::bail!("unknown private fixture case"),
            };
            fs::write(root.join("receipt.json"), serde_json::to_vec(&receipt)?)?;
            return Ok(());
        }
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        fs::create_dir(root.join("home"))?;
        let executable = std::env::current_exe()?.canonicalize()?;
        let token = Uuid::now_v7().to_string();
        fs::write(
            root.join("child-marker.json"),
            serde_json::to_vec(&json!({
                "token": token, "case": case, "test_name": test_name,
                "executable": executable.to_string_lossy(),
            }))?,
        )?;
        let mut command = Command::new(&executable);
        command
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(root.join("stdout.log"))?))
            .stderr(Stdio::from(fs::File::create(root.join("stderr.log"))?));
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("KHIVE_")
                || name.starts_with("KKERNEL_")
                || name.starts_with("LATTICE_")
                || name.starts_with("XDG_")
            {
                command.env_remove(key);
            }
        }
        command
            .env(CHILD_ROOT, &root)
            .env(CHILD_TOKEN, token)
            .env("HOME", root.join("home"))
            .env("XDG_CACHE_HOME", root.join("home/cache"))
            .env("LATTICE_MODEL_CACHE", root.join("home/models"));
        let mut child = ChildGuard(command.spawn()?);
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            if start.elapsed() >= Duration::from_secs(120) {
                let _ = child.0.kill();
                child.0.wait().context("reap timed-out fixture child")?;
                anyhow::bail!("fixture child timeout; no semantic control credit");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        drop(child);
        let output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&fs::read(root.join("stdout.log"))?),
            String::from_utf8_lossy(&fs::read(root.join("stderr.log"))?)
        );
        ensure!(
            status.success(),
            "fixture child failed before qualified receipt; no semantic control credit: {output}"
        );
        let receipt: Value = serde_json::from_slice(&fs::read(root.join("receipt.json"))?)?;
        let no_models = !root.join("home/models").exists();
        directory
            .close()
            .context("remove settled private fixture before semantic assertions")?;
        ensure!(
            receipt["baseline_valid"] == true,
            "fixture baseline not qualified"
        );
        let seed_paths = receipt["seed_paths"]
            .as_array()
            .context("fixture seed paths missing")?;
        ensure!(!seed_paths.is_empty(), "fixture seed paths are empty");
        ensure!(
            seed_paths.iter().all(|path| path == "create_note"),
            "unexpected fixture seed path; no fallback is authorized"
        );
        if case == "deleted-source" {
            ensure!(
                receipt["source_was_soft_deleted"] == true,
                "deleted-source fixture baseline missing"
            );
        }
        ensure!(
            no_models,
            "fixture unexpectedly initialized model storage; no semantic control credit"
        );
        assert_no_dry_run_mutation(&receipt);
        assert_eq!(receipt["dry"]["after_basis"], "projected");
        assert_eq!(receipt["applied"]["after_basis"], "measured");
        match case {
            "six" | "paged" | "deleted-source" => {
                let candidates = receipt["expected_candidates"]
                    .as_u64()
                    .expect("candidate count");
                assert_eq!(receipt["dry"]["candidates"], candidates);
                assert_eq!(receipt["dry"]["moved"], 0);
                assert_eq!(receipt["dry"]["apply_blocked"], false);
                assert_eq!(receipt["applied"]["moved"], candidates);
                assert_eq!(receipt["applied"]["remaining"], 0);
                assert_eq!(receipt["applied"]["complete"], true);
                let total = receipt["before_edges"]
                    .as_array()
                    .expect("baseline edges")
                    .len() as u64;
                for mode in ["dry", "applied"] {
                    let counts = receipt[mode]["counts"]
                        .as_array()
                        .expect("namespace counts");
                    let source = counts
                        .iter()
                        .find(|row| row["namespace"] == SOURCE && row["relation"] == "supersedes")
                        .expect("source namespace count");
                    let local = counts
                        .iter()
                        .find(|row| row["namespace"] == "local" && row["relation"] == "supersedes")
                        .expect("local namespace count");
                    assert_eq!(source["liveness"], "live");
                    assert_eq!(local["liveness"], "live");
                    assert_eq!(source["before"], total - candidates);
                    assert_eq!(source["after"], total);
                    assert_eq!(local["before"], candidates);
                    assert_eq!(local["after"], 0);
                }
                assert_eq!(receipt["rerun"]["candidates"], 0);
                assert_eq!(receipt["rerun"]["moved"], 0);
                assert_eq!(receipt["rerun"]["remaining"], 0);
                assert_applied_identity(&receipt);
            }
            "projected" | "live" | "deleted" => {
                assert_eq!(
                    receipt["before_edges"], receipt["after_edges"],
                    "collision_preflight_leaves_every_edge_unchanged"
                );
                for mode in ["dry", "applied"] {
                    assert_eq!(receipt[mode]["collision_groups"], 1);
                    assert_eq!(receipt[mode]["collision_rows"], receipt["expected_rows"]);
                    assert_eq!(receipt[mode]["apply_blocked"], true);
                    assert_eq!(receipt[mode]["moved"], 0);
                    for count in receipt[mode]["counts"].as_array().expect("census counts") {
                        assert_eq!(
                            count["before"], count["after"],
                            "collision_after_equals_before"
                        );
                    }
                }
                assert_eq!(receipt["applied"]["complete"], false);
                assert!(!receipt["applied"]["failures"]
                    .as_array()
                    .expect("refusal")
                    .is_empty());
            }
            "excluded" => {
                assert_eq!(
                    receipt["before_edges"], receipt["after_edges"],
                    "event_target_and_deleted_edge_untouched"
                );
                for mode in ["dry", "applied"] {
                    assert_eq!(receipt[mode]["excluded_edges"], 1);
                    assert_eq!(receipt[mode]["counts"], json!([]));
                    assert_eq!(receipt[mode]["candidates"], 0);
                    assert_eq!(receipt[mode]["moved"], 0);
                }
            }
            "chain" => {
                let before = &receipt["before_anchor"];
                assert_eq!(
                    before["neighbors"]
                        .as_array()
                        .expect("before anchor neighbors")
                        .len(),
                    1,
                    "whole_chain_before_anchor_visible_edge_count"
                );
                assert_eq!(
                    before["hidden_incoming_edges"]["total"],
                    json!(0),
                    "whole_chain_before_anchor_incoming_count_zero"
                );
                let default = &receipt["before_default_anchor"];
                let default_neighbors = default["neighbors"]
                    .as_array()
                    .expect("default anchor neighbors");
                assert!(
                    default_neighbors.len() >= 5,
                    "whole_chain_default_view_sees_local_edges"
                );
                let default_bytes =
                    serde_json::to_string(default_neighbors).expect("default neighbor bytes");
                for edge in receipt["before_edges"]
                    .as_array()
                    .expect("original chain edges")
                    .iter()
                    .filter(|edge| edge["namespace"] == "local")
                {
                    assert!(
                        default_bytes
                            .contains(edge["id"].as_str().expect("local edge UUID string")),
                        "whole_chain_default_view_returns_each_local_edge"
                    );
                }
                assert_eq!(
                    default["hidden_incoming_edges"]["total"],
                    json!(0),
                    "whole_chain_default_view_has_zero_hidden_count"
                );
                assert_eq!(
                    receipt["dry"]["candidates"],
                    json!(5),
                    "whole_chain_dry_candidates"
                );
                assert_eq!(
                    receipt["dry"]["moved"],
                    json!(0),
                    "whole_chain_dry_moved_zero"
                );
                assert_eq!(receipt["dry"]["apply_blocked"], json!(false));
                assert_eq!(
                    receipt["applied"]["moved"],
                    json!(5),
                    "whole_chain_apply_moved_five"
                );
                assert_eq!(receipt["applied"]["remaining"], json!(0));
                assert_eq!(receipt["applied"]["complete"], json!(true));
                for mode in ["dry", "applied"] {
                    let counts = receipt[mode]["counts"]
                        .as_array()
                        .expect("whole chain namespace census");
                    let source = counts
                        .iter()
                        .find(|row| row["namespace"] == SOURCE && row["relation"] == "supersedes")
                        .expect("whole chain source census");
                    let local = counts
                        .iter()
                        .find(|row| row["namespace"] == "local" && row["relation"] == "supersedes")
                        .expect("whole chain local census");
                    assert_eq!(source["liveness"], "live");
                    assert_eq!(local["liveness"], "live");
                    assert_eq!(source["before"], json!(1));
                    assert_eq!(source["after"], json!(6));
                    assert_eq!(local["before"], json!(5));
                    assert_eq!(local["after"], json!(0));
                }
                assert_eq!(receipt["rerun"]["candidates"], json!(0));
                assert_eq!(receipt["rerun"]["moved"], json!(0));
                assert_eq!(receipt["rerun"]["remaining"], json!(0));
                assert_applied_identity(&receipt);
                let target_ids = receipt["target_ids"]
                    .as_array()
                    .expect("whole chain target ids");
                assert_eq!(target_ids.len(), 6, "whole_chain_target_ids");
                let after = receipt["after_anchor"]["neighbors"]
                    .as_array()
                    .expect("after anchor neighbors");
                assert_eq!(after.len(), 6, "whole_chain_after_apply_six_visible_edges");
                let bytes = serde_json::to_string(after).expect("after neighbor bytes");
                for target in target_ids {
                    assert!(
                        bytes.contains(target.as_str().expect("target UUID string")),
                        "whole_chain_after_apply_contains_every_target"
                    );
                }
                let targets = receipt["before_hidden_targets"]
                    .as_array()
                    .expect("before target readings");
                assert_eq!(targets.len(), 5, "whole_chain_hidden_target_readings");
                for (target, id) in targets.iter().zip(target_ids.iter().skip(1)) {
                    assert_eq!(&target["target_id"], id, "whole_chain_target_read_is_bound");
                    assert_eq!(
                        target["response"]["hidden_incoming_edges"]["total"],
                        json!(1),
                        "whole_chain_before_target_hidden_count"
                    );
                    assert_eq!(
                        target["response"]["hidden_incoming_edges"]["by_relation"],
                        json!({"supersedes": 1}),
                        "whole_chain_target_relation_count"
                    );
                }
            }
            _ => unreachable!("qualified case"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn dry_run_then_apply_moves_six_edges() -> Result<()> {
        run_case(
            "six",
            "memory_edge_namespace_move::tests::dry_run_then_apply_moves_six_edges",
        )
        .await
    }

    #[tokio::test]
    async fn hidden_incoming_chain_before_and_after_move() -> Result<()> {
        run_case(
            "chain",
            "memory_edge_namespace_move::tests::hidden_incoming_chain_before_and_after_move",
        )
        .await
    }

    #[tokio::test]
    async fn deleted_memory_source_still_moves_live_edge() -> Result<()> {
        run_case(
            "deleted-source",
            "memory_edge_namespace_move::tests::deleted_memory_source_still_moves_live_edge",
        )
        .await
    }

    #[tokio::test]
    async fn preflight_completes_before_moving_paged_cohort() -> Result<()> {
        run_case(
            "paged",
            "memory_edge_namespace_move::tests::preflight_completes_before_moving_paged_cohort",
        )
        .await
    }

    #[tokio::test]
    async fn projected_duplicates_refuse_entire_apply() -> Result<()> {
        run_case(
            "projected",
            "memory_edge_namespace_move::tests::projected_duplicates_refuse_entire_apply",
        )
        .await
    }

    #[tokio::test]
    async fn live_destination_refuses_entire_apply() -> Result<()> {
        run_case(
            "live",
            "memory_edge_namespace_move::tests::live_destination_refuses_entire_apply",
        )
        .await
    }

    #[tokio::test]
    async fn deleted_destination_refuses_entire_apply() -> Result<()> {
        run_case(
            "deleted",
            "memory_edge_namespace_move::tests::deleted_destination_refuses_entire_apply",
        )
        .await
    }

    #[tokio::test]
    async fn event_target_and_deleted_edge_are_excluded_untouched() -> Result<()> {
        run_case("excluded", "memory_edge_namespace_move::tests::event_target_and_deleted_edge_are_excluded_untouched").await
    }
}
