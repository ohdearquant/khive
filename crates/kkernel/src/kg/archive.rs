//! `kkernel kg export` and `kkernel kg import` — archive round-trip operations.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use khive_runtime::pack::{PackRegistry, VerbRegistryBuilder};
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::EdgeRelation;
use khive_vcs_adapters::{EdgeRecord, EntityRecord, FormatAdapter, JsonFormatAdapter};
use uuid::Uuid;

use super::types::{ExportArgs, ImportArgs, ImportFormat};

pub(super) async fn cmd_export(args: ExportArgs) -> Result<()> {
    let ns = Namespace::parse(&args.namespace)?;

    // Refuse to clobber the source database with the JSON export.
    // Resolve the output's real identity: canonicalize it directly when it
    // already exists (this follows an existing symlink to its target), else
    // canonicalize the parent and rejoin the file name. Compare literally too so
    // `./x.db` vs `x.db` can't slip through.
    let db_canon = std::fs::canonicalize(&args.db).ok();
    let out_canon = std::fs::canonicalize(&args.output).ok().or_else(|| {
        args.output
            .parent()
            .and_then(|p| std::fs::canonicalize(p).ok())
            .map(|p| p.join(args.output.file_name().unwrap_or_default()))
    });
    if args.output == args.db || (db_canon.is_some() && db_canon == out_canon) {
        anyhow::bail!(
            "refusing to export: --output {} resolves to the --db path {} (would overwrite the database)",
            args.output.display(),
            args.db.display(),
        );
    }

    let config = RuntimeConfig {
        db_path: Some(args.db.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config)?;
    let token = runtime.authorize(ns)?;

    let json = runtime
        .export_kg_json(&token)
        .await
        .with_context(|| format!("export namespace {:?}", args.namespace))?;
    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
    }
    // Write through a temp sibling + atomic rename so a symlinked --output is
    // replaced rather than followed into the source DB. The temp is created with
    // O_EXCL (create_new): a pre-existing temp path — including a planted symlink
    // to the DB — fails the create rather than being followed, closing the whole
    // symlink-overwrite class, not just --output itself.
    use std::io::Write as _;
    let mut tmp_name = args.output.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.inprogress", std::process::id()));
    let tmp = args.output.with_file_name(tmp_name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("create temp {}", tmp.display()))?;
    f.write_all(json.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    f.sync_all().ok();
    drop(f);
    std::fs::rename(&tmp, &args.output)
        .with_context(|| format!("finalize {}", args.output.display()))?;
    Ok(())
}

pub(super) async fn cmd_import(args: ImportArgs) -> Result<()> {
    let ns = Namespace::parse(&args.namespace)?;
    let config = RuntimeConfig {
        db_path: Some(args.db.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config)?;
    let valid_entity_kinds = install_import_kind_registry(&runtime)?;
    let token = runtime.authorize(ns)?;

    let source = std::fs::read_to_string(&args.source)
        .with_context(|| format!("read {}", args.source.display()))?;

    let summary = match args.format {
        ImportFormat::Archive => {
            let archive: KgArchive = serde_json::from_str(&source)
                .with_context(|| format!("parse archive {}", args.source.display()))?;
            validate_archive_edge_weights(&archive)?;
            runtime
                .import_kg(&archive, &token)
                .await
                .with_context(|| format!("import archive {}", args.source.display()))?
        }
        ImportFormat::Json | ImportFormat::Ndjson => {
            let input = match args.format {
                ImportFormat::Json => source,
                ImportFormat::Ndjson => ndjson_to_json_array(&source)?,
                ImportFormat::Archive => unreachable!(),
            };
            let mut adapter = JsonFormatAdapter::new_with_valid_kinds(&input, &valid_entity_kinds)
                .with_context(|| format!("parse adapter input {}", args.source.display()))?;
            if args.verbose {
                for warning in adapter.warnings() {
                    eprintln!("warning: {warning}");
                }
            }
            let entities: Vec<EntityRecord> = adapter
                .entities()
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let edges: Vec<EdgeRecord> = adapter
                .edges()
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let archive = adapter_records_to_archive(&args.namespace, entities, edges)?;
            runtime
                .import_kg(&archive, &token)
                .await
                .with_context(|| format!("import adapter records {}", args.source.display()))?
        }
    };

    let json = serde_json::to_string(&summary).expect("serialize ImportSummary");
    println!("{json}");
    Ok(())
}

fn ndjson_to_json_array(source: &str) -> Result<String> {
    let mut values = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("parse NDJSON line {}", idx + 1))?;
        values.push(value);
    }
    serde_json::to_string(&values).context("serialize NDJSON records as JSON array")
}

fn adapter_records_to_archive(
    namespace: &str,
    entities: Vec<EntityRecord>,
    edges: Vec<EdgeRecord>,
) -> Result<KgArchive> {
    let now = Utc::now();
    let entity_ids: HashSet<Uuid> = entities.iter().map(|e| e.id).collect();

    let exported_entities: Vec<ExportedEntity> = entities
        .into_iter()
        .map(|e| {
            // Entity kind is validated against the merged pack/runtime kind
            // registry inside `runtime.import_kg`, not here — a bare base-enum
            // parse would reject valid pack-registered kinds like `resource`.
            Ok(ExportedEntity {
                id: e.id,
                kind: e.kind,
                entity_type: e.entity_type,
                name: e.name,
                description: e.description,
                properties: if e.properties.is_null() {
                    None
                } else {
                    Some(e.properties)
                },
                tags: e.tags,
                created_at: parse_dt(e.created_at.as_deref(), now),
                updated_at: parse_dt(e.updated_at.as_deref(), now),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let exported_edges = edges
        .into_iter()
        .map(|edge| adapter_edge_to_exported(edge, &entity_ids))
        .collect::<Result<Vec<_>>>()?;

    Ok(KgArchive {
        format: "khive-kg".to_string(),
        version: "0.1".to_string(),
        namespace: namespace.to_string(),
        exported_at: now,
        entities: exported_entities,
        edges: exported_edges,
    })
}

/// Validate that a deserialized edge weight is finite and within [0.0, 1.0].
pub(super) fn validate_edge_weight(weight: f64, edge_id: impl std::fmt::Display) -> Result<()> {
    if !weight.is_finite() {
        bail!(
            "edge {} weight {weight} is not finite (NaN or infinity not allowed)",
            edge_id
        );
    }
    if !(0.0..=1.0).contains(&weight) {
        bail!(
            "edge {} weight {weight} is outside the valid range [0.0, 1.0]",
            edge_id
        );
    }
    Ok(())
}

/// Pre-import validation that does not require entity-kind knowledge.
/// Entity-kind validation happens inside `runtime.import_kg` against the
/// merged pack/runtime kind registry installed by `install_import_kind_registry`.
pub(super) fn validate_archive_edge_weights(archive: &KgArchive) -> Result<()> {
    for edge in &archive.edges {
        validate_edge_weight(edge.weight, edge.edge_id)?;
    }
    Ok(())
}

/// Install the merged pack/runtime entity- and note-kind registry on `runtime`
/// so that `runtime.import_kg` (and any other runtime-layer kind validation)
/// accepts pack-registered kinds such as `resource`, not just the eight base
/// `khive_types::EntityKind` variants.
///
/// Returns the merged entity-kind list so callers can also thread it into
/// adapter-layer validation (see `JsonFormatAdapter::new_with_valid_kinds`,
/// issue #530).
fn install_import_kind_registry(runtime: &KhiveRuntime) -> Result<Vec<String>> {
    let mut builder = VerbRegistryBuilder::new();
    let names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    PackRegistry::register_packs(&names, runtime.clone(), &mut builder)
        .map_err(|n| anyhow::anyhow!("pack {n:?} declared in inventory but factory missing"))?;
    let registry = builder.build().context("building import VerbRegistry")?;
    let entity_kinds: Vec<String> = registry
        .all_entity_kinds()
        .into_iter()
        .map(str::to_string)
        .collect();
    let note_kinds: Vec<String> = registry
        .all_note_kinds()
        .into_iter()
        .map(str::to_string)
        .collect();
    runtime.install_kind_registry(entity_kinds.clone(), note_kinds);
    Ok(entity_kinds)
}

fn adapter_edge_to_exported(edge: EdgeRecord, entity_ids: &HashSet<Uuid>) -> Result<ExportedEdge> {
    let source = edge
        .source
        .parse::<Uuid>()
        .with_context(|| format!("edge {} source must be a UUID", edge.edge_id))?;
    let target = edge
        .target
        .parse::<Uuid>()
        .with_context(|| format!("edge {} target must be a UUID", edge.edge_id))?;
    if !entity_ids.contains(&source) {
        bail!(
            "edge {} source {} is not present in adapter entities",
            edge.edge_id,
            source
        );
    }
    if !entity_ids.contains(&target) {
        bail!(
            "edge {} target {} is not present in adapter entities",
            edge.edge_id,
            target
        );
    }
    let relation: EdgeRelation = edge
        .relation
        .parse()
        .with_context(|| format!("edge {} invalid relation {:?}", edge.edge_id, edge.relation))?;

    validate_edge_weight(edge.weight, edge.edge_id)?;
    Ok(ExportedEdge {
        edge_id: edge.edge_id,
        source,
        target,
        relation,
        weight: edge.weight,
    })
}

/// Build a [`KgArchive`] from on-disk NDJSON files for hashing or import.
pub(super) fn archive_from_ndjson_repo(repo: &Path, namespace: &str) -> Result<KgArchive> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct NdjsonEntity {
        id: Uuid,
        kind: String,
        #[serde(default)]
        entity_type: Option<String>,
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        properties: Option<serde_json::Value>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        created_at: Option<String>,
        #[serde(default)]
        updated_at: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct NdjsonEdge {
        edge_id: Uuid,
        source: Uuid,
        target: Uuid,
        relation: String,
        #[serde(default = "default_weight")]
        weight: f64,
    }

    fn default_weight() -> f64 {
        1.0
    }

    let kg_dir = repo.join(".khive/kg");
    let entities = read_ndjson_records::<NdjsonEntity>(&kg_dir.join("entities.ndjson"), "entity")?;
    let edges = read_ndjson_records::<NdjsonEdge>(&kg_dir.join("edges.ndjson"), "edge")?;
    let now = Utc::now();

    let exported_entities = entities
        .into_iter()
        .map(|e| ExportedEntity {
            id: e.id,
            kind: e.kind,
            entity_type: e.entity_type,
            name: e.name,
            description: e.description,
            properties: e.properties,
            tags: e.tags,
            created_at: parse_dt(e.created_at.as_deref(), now),
            updated_at: parse_dt(e.updated_at.as_deref(), now),
        })
        .collect();

    let exported_edges = edges
        .into_iter()
        .map(|edge| {
            let relation: EdgeRelation = edge
                .relation
                .parse()
                .with_context(|| format!("invalid relation {:?}", edge.relation))?;
            validate_edge_weight(edge.weight, edge.edge_id)?;
            Ok(ExportedEdge {
                edge_id: edge.edge_id,
                source: edge.source,
                target: edge.target,
                relation,
                weight: edge.weight,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(KgArchive {
        format: "khive-kg".to_string(),
        version: "0.1".to_string(),
        namespace: namespace.to_string(),
        exported_at: now,
        entities: exported_entities,
        edges: exported_edges,
    })
}

pub(super) fn read_ndjson_records<T>(path: &Path, label: &str) -> Result<Vec<T>>
where
    T: for<'de> serde::Deserialize<'de>,
{
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut records = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record = serde_json::from_str(trimmed)
            .with_context(|| format!("parse {label} at {}:{}", path.display(), idx + 1))?;
        records.push(record);
    }
    Ok(records)
}

fn parse_dt(value: Option<&str>, fallback: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    value
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use uuid::Uuid;

    use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

    use super::*;

    #[tokio::test]
    async fn export_creates_archive_json() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let output_path = tmp.path().join("archive.json");

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..Default::default()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        let token = runtime.authorize(ns).unwrap();
        runtime
            .create_entity(&token, "concept", None, "TestEntity", None, None, vec![])
            .await
            .unwrap();

        let args = ExportArgs {
            output: output_path.clone(),
            db: db_path,
            namespace: "test-ns".to_string(),
        };
        cmd_export(args).await.unwrap();

        assert!(output_path.exists(), "output archive must exist");
        let content = std::fs::read_to_string(&output_path).unwrap();
        let archive: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(archive["format"].as_str().unwrap(), "khive-kg");
        let entities = archive["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 1, "one entity exported");
        assert_eq!(entities[0]["name"].as_str().unwrap(), "TestEntity");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn export_refuses_symlinked_output_to_db() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("working.db");

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..Default::default()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        let token = runtime.authorize(ns).unwrap();
        runtime
            .create_entity(&token, "concept", None, "Keep", None, None, vec![])
            .await
            .unwrap();
        drop(runtime);
        let before = std::fs::read(&db_path).unwrap();

        let link = tmp.path().join("archive.json");
        std::os::unix::fs::symlink(&db_path, &link).unwrap();

        let args = ExportArgs {
            output: link,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
        };
        assert!(
            cmd_export(args).await.is_err(),
            "export through a symlink to the DB must be refused"
        );

        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(before, after, "source DB must be byte-for-byte unchanged");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn export_refuses_symlinked_temp_to_db() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("working.db");

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..Default::default()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        let token = runtime.authorize(ns).unwrap();
        runtime
            .create_entity(&token, "concept", None, "Keep", None, None, vec![])
            .await
            .unwrap();
        drop(runtime);
        let before = std::fs::read(&db_path).unwrap();

        let out = tmp.path().join("archive.json");
        let mut tmp_name = out.file_name().unwrap().to_os_string();
        tmp_name.push(format!(".{}.inprogress", std::process::id()));
        let temp_path = out.with_file_name(tmp_name);
        std::os::unix::fs::symlink(&db_path, &temp_path).unwrap();

        let args = ExportArgs {
            output: out,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
        };
        assert!(
            cmd_export(args).await.is_err(),
            "export must refuse when the temp path is a symlink to the DB"
        );
        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(before, after, "source DB must be byte-for-byte unchanged");
    }

    #[tokio::test]
    async fn import_archive_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("import-test.db");
        let entity_id = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        let archive_json = format!(
            r#"{{"format":"khive-kg","version":"0.1","namespace":"test-ns","exported_at":"2026-01-01T00:00:00Z","entities":[{{"id":"{entity_id}","kind":"concept","name":"Imported","tags":[],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}],"edges":[]}}"#
        );
        let source_path = tmp.path().join("archive.json");
        std::fs::write(&source_path, &archive_json).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Archive,
            verbose: false,
        };
        cmd_import(args).await.unwrap();

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let entity_uuid: Uuid = entity_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
        assert_eq!(entity.name, "Imported");
    }

    #[tokio::test]
    async fn import_archive_accepts_resource_kind() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("import-resource.db");
        let entity_id = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";

        let archive_json = format!(
            r#"{{"format":"khive-kg","version":"0.1","namespace":"test-ns","exported_at":"2026-01-01T00:00:00Z","entities":[{{"id":"{entity_id}","kind":"resource","name":"ImportedResource","tags":[],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}],"edges":[]}}"#
        );
        let source_path = tmp.path().join("archive.json");
        std::fs::write(&source_path, &archive_json).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Archive,
            verbose: false,
        };
        cmd_import(args)
            .await
            .expect("archive import must accept pack-registered `resource` kind");

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let entity_uuid: Uuid = entity_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
        assert_eq!(entity.kind, "resource");
        assert_eq!(entity.name, "ImportedResource");
    }

    // #530 (follow-up to #438): the JSON/NDJSON adapter import path
    // (`ImportFormat::Json` / `ImportFormat::Ndjson`) goes through
    // `khive_vcs_adapters::JsonFormatAdapter`, which used to independently gate
    // entity kind through the base `khive_types::EntityKind::from_str` — before
    // an `ExportedEntity`/`KgArchive` was even constructed, and before this
    // module's `install_import_kind_registry` had any effect. `cmd_import` now
    // threads the merged entity-kind registry into
    // `JsonFormatAdapter::new_with_valid_kinds`, so pack-registered granular
    // kinds like `resource` are accepted at this layer too.

    #[tokio::test]
    async fn import_json_adapter_accepts_resource_kind() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("import-json-resource.db");
        let entity_id = "ffffffff-ffff-ffff-ffff-ffffffffffff";

        let json_input =
            format!(r#"[{{"id":"{entity_id}","kind":"resource","name":"JsonResource"}}]"#);
        let source_path = tmp.path().join("records.json");
        std::fs::write(&source_path, &json_input).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Json,
            verbose: false,
        };
        cmd_import(args)
            .await
            .expect("JSON adapter import must accept pack-registered `resource` kind");

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let entity_uuid: Uuid = entity_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
        assert_eq!(entity.kind, "resource");
        assert_eq!(entity.name, "JsonResource");
    }

    #[tokio::test]
    async fn import_ndjson_adapter_accepts_resource_kind() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("import-ndjson-resource.db");
        let entity_id = "abababab-abab-abab-abab-abababababab";

        let ndjson_input =
            format!(r#"{{"id":"{entity_id}","kind":"resource","name":"NdjsonResource"}}"#);
        let source_path = tmp.path().join("records.ndjson");
        std::fs::write(&source_path, &ndjson_input).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Ndjson,
            verbose: false,
        };
        cmd_import(args)
            .await
            .expect("NDJSON adapter import must accept pack-registered `resource` kind");

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let entity_uuid: Uuid = entity_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
        assert_eq!(entity.kind, "resource");
        assert_eq!(entity.name, "NdjsonResource");
    }

    #[tokio::test]
    async fn import_rejects_unregistered_entity_kind() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("import-unknown-kind.db");
        let entity_id = "12345678-1234-1234-1234-123456789012";

        let json_input =
            format!(r#"[{{"id":"{entity_id}","kind":"not_a_registered_kind","name":"Bad"}}]"#);
        let source_path = tmp.path().join("records.json");
        std::fs::write(&source_path, &json_input).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path,
            namespace: "test-ns".to_string(),
            format: ImportFormat::Json,
            verbose: false,
        };
        assert!(
            cmd_import(args).await.is_err(),
            "import must still reject entity kinds unknown to every registered pack"
        );
    }

    #[tokio::test]
    async fn import_json_adapter_imports_entities() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("adapter-json.db");
        let e1_id = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let e2_id = "dddddddd-dddd-dddd-dddd-dddddddddddd";

        let json_input = format!(
            r#"[{{"id":"{e1_id}","kind":"concept","name":"Entity1"}},{{"id":"{e2_id}","kind":"concept","name":"Entity2"}}]"#
        );
        let source_path = tmp.path().join("records.json");
        std::fs::write(&source_path, &json_input).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Json,
            verbose: false,
        };
        cmd_import(args).await.unwrap();

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let e1_uuid: Uuid = e1_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, e1_uuid).await.unwrap();
        assert_eq!(entity.name, "Entity1");
    }

    #[tokio::test]
    async fn import_ndjson_adapter_imports_entity() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("adapter-ndjson.db");
        let entity_id = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";

        let ndjson_input =
            format!(r#"{{"id":"{entity_id}","kind":"concept","name":"NdjsonEntity"}}"#);
        let source_path = tmp.path().join("records.ndjson");
        std::fs::write(&source_path, &ndjson_input).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Ndjson,
            verbose: false,
        };
        cmd_import(args).await.unwrap();

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let entity_uuid: Uuid = entity_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
        assert_eq!(entity.name, "NdjsonEntity");
    }

    #[test]
    fn validate_edge_weight_valid_boundaries() {
        assert!(validate_edge_weight(0.0, "edge-a").is_ok());
        assert!(validate_edge_weight(1.0, "edge-a").is_ok());
        assert!(validate_edge_weight(0.5, "edge-a").is_ok());
    }

    #[test]
    fn validate_edge_weight_nan_is_rejected() {
        let err = validate_edge_weight(f64::NAN, "edge-x").unwrap_err();
        assert!(
            err.to_string().contains("not finite"),
            "expected 'not finite' in error: {err}"
        );
    }

    #[test]
    fn validate_edge_weight_infinity_is_rejected() {
        let err = validate_edge_weight(f64::INFINITY, "edge-y").unwrap_err();
        assert!(
            err.to_string().contains("not finite"),
            "expected 'not finite' in error: {err}"
        );
        let err = validate_edge_weight(f64::NEG_INFINITY, "edge-y").unwrap_err();
        assert!(
            err.to_string().contains("not finite"),
            "expected 'not finite' in error: {err}"
        );
    }

    /// #472: `adapter_records_to_archive` must preserve ADR-020 `entity_type`
    /// and timestamp fields from adapter-parsed `EntityRecord`s instead of
    /// forcing `entity_type: None` and `Utc::now()`.
    #[test]
    fn adapter_records_to_archive_preserves_entity_adr020_fields() {
        let id = Uuid::new_v4();
        let record = EntityRecord {
            id,
            kind: "document".to_string(),
            entity_type: Some("paper".to_string()),
            name: "Attention Is All You Need".to_string(),
            description: None,
            properties: serde_json::Value::Null,
            tags: vec![],
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            updated_at: Some("2026-02-02T00:00:00Z".to_string()),
        };

        let archive = adapter_records_to_archive("test-ns", vec![record], vec![]).unwrap();
        assert_eq!(archive.entities.len(), 1);
        let e = &archive.entities[0];
        assert_eq!(e.entity_type.as_deref(), Some("paper"));
        assert_eq!(e.created_at.to_rfc3339(), "2026-01-01T00:00:00+00:00");
        assert_eq!(e.updated_at.to_rfc3339(), "2026-02-02T00:00:00+00:00");
    }

    /// #472: importing adapter JSON with `entity_type`/timestamps end-to-end
    /// through `cmd_import` must land those fields in the runtime, not `None`
    /// + import-time `Utc::now()`.
    #[tokio::test]
    async fn import_json_adapter_preserves_entity_type_and_timestamps() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("adapter-adr020.db");
        let entity_id = "77777777-7777-7777-7777-777777777777";

        let json_input = format!(
            r#"[{{"id":"{entity_id}","kind":"paper","name":"Some Paper","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-02-02T00:00:00Z"}}]"#
        );
        let source_path = tmp.path().join("records.json");
        std::fs::write(&source_path, &json_input).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Json,
            verbose: false,
        };
        cmd_import(args).await.unwrap();

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let entity_uuid: Uuid = entity_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
        assert_eq!(entity.kind, "document");
        assert_eq!(entity.entity_type.as_deref(), Some("paper"));
        let expected_created = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .timestamp_micros();
        let expected_updated = chrono::DateTime::parse_from_rfc3339("2026-02-02T00:00:00Z")
            .unwrap()
            .timestamp_micros();
        assert_eq!(entity.created_at, expected_created);
        assert_eq!(entity.updated_at, expected_updated);
    }

    /// #488a: a non-object element anywhere in the top-level JSON array must
    /// abort the whole import — including entities before AND after it in the
    /// array — before anything is written to the target DB. Previously this
    /// was a warning that skipped just the bad element and kept going.
    #[tokio::test]
    async fn import_json_adapter_rejects_non_object_array_element_without_db_write() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("adapter-488.db");
        let baseline_id = "88888888-8888-8888-8888-888888888888";

        // Establish a baseline DB state via a first, valid import.
        let baseline_json =
            format!(r#"[{{"id":"{baseline_id}","kind":"concept","name":"Baseline"}}]"#);
        let baseline_source = tmp.path().join("baseline.json");
        std::fs::write(&baseline_source, &baseline_json).unwrap();
        cmd_import(ImportArgs {
            source: baseline_source,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Json,
            verbose: false,
        })
        .await
        .unwrap();

        // A second import mixes a well-formed entity with a bare string
        // element. The whole import must be rejected — the well-formed
        // entity must NOT be written even though it appears before the
        // malformed element in the array.
        let never_imported_id = "99999999-9999-9999-9999-999999999999";
        let bad_json = format!(
            r#"[{{"id":"{never_imported_id}","kind":"concept","name":"NeverImported"}},"not-a-record"]"#
        );
        let bad_source = tmp.path().join("bad.json");
        std::fs::write(&bad_source, &bad_json).unwrap();

        let err = cmd_import(ImportArgs {
            source: bad_source,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Json,
            verbose: false,
        })
        .await
        .expect_err("non-object array element must fail the whole import");
        assert!(
            err.chain().any(|e| e.to_string().contains("$record")),
            "error must identify the malformed record, got: {err:#}"
        );

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();

        let baseline_uuid: Uuid = baseline_id.parse().unwrap();
        let baseline = rt2
            .get_entity(&tok2, baseline_uuid)
            .await
            .expect("baseline entity must still be present, untouched by the failed import");
        assert_eq!(baseline.name, "Baseline");

        let never_imported_uuid: Uuid = never_imported_id.parse().unwrap();
        assert!(
            rt2.get_entity(&tok2, never_imported_uuid).await.is_err(),
            "entity preceding the malformed element in the array must not have been imported"
        );
    }

    #[test]
    fn validate_edge_weight_out_of_range_is_rejected() {
        let err = validate_edge_weight(1.5, "edge-z").unwrap_err();
        assert!(
            err.to_string().contains("outside the valid range"),
            "expected range error: {err}"
        );
        let err = validate_edge_weight(-0.1, "edge-z").unwrap_err();
        assert!(
            err.to_string().contains("outside the valid range"),
            "expected range error: {err}"
        );
    }
}
