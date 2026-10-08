//! Real ingest identity checks with private source trees and an in-memory map.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::Utc;
use khive_pack_code::source_ingest::{run_code_ingest, CodeSourceIngestOptions};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::SqlStatement;
use serde_json::Value;
use tempfile::TempDir;

fn runtime() -> KhiveRuntime {
    let config = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        credentials: Vec::new(),
        visibility_receipts: None,
        packs: Vec::new(),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: Vec::new(),
        events_split: None,
        default_namespace: Namespace::local(),
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        ..RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new(config).expect("private memory map");
    assert!(!rt.backend().is_file_backed());
    assert!(rt.backend_data_dir().is_none());
    assert!(rt.backend_ann_root().is_none());
    assert!(rt.backend().pool().canonical_path().is_none());
    assert!(rt.default_embedder_name().is_empty());
    assert!(rt.blob_store().is_none());
    rt
}

fn source_tree(project: &Path, manifest: bool) {
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::create_dir(project.join("child")).unwrap();
    std::fs::write(
        project.join("src/lib.rs"),
        "mod util;\nuse crate::util::helper;\npub fn call() { helper(); }\n",
    )
    .unwrap();
    std::fs::write(project.join("src/util.rs"), "pub fn helper() {}\n").unwrap();
    if manifest {
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"manifest_owner\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
    }
}

#[derive(Clone, Copy)]
struct Tiers {
    manifests: bool,
    imports: bool,
    symbols: bool,
}

const IMPORTS: Tiers = Tiers {
    manifests: false,
    imports: true,
    symbols: false,
};
const SYMBOLS: Tiers = Tiers {
    manifests: false,
    imports: false,
    symbols: true,
};
const BOTH: Tiers = Tiers {
    manifests: false,
    imports: true,
    symbols: true,
};

async fn warm(rt: &KhiveRuntime, path: &Path, tiers: Tiers) {
    let token = rt.authorize(Namespace::local()).expect("local token");
    // The existing manifestless fixture repeats ingestion to settle imports
    // regardless of file-walk order; compare edges only after that same warm-up.
    for _ in 0..2 {
        let report = run_code_ingest(
            rt,
            &token,
            CodeSourceIngestOptions {
                path,
                languages: ["rust"].into_iter().collect(),
                sweep_time: Utc::now(),
                enable_l1: tiers.manifests,
                enable_l1_5: tiers.imports,
                enable_l2: tiers.symbols,
            },
        )
        .await
        .expect("real source ingest");
        assert_eq!(report.blocked_count, 0);
        assert_eq!(report.source_files_refused, 0);
        assert_eq!(report.manifest_files_refused, 0);
        assert_eq!(report.files_dropped_without_source_path, 0);
        if tiers.symbols {
            assert_eq!(
                report.l2.as_ref().expect("L2 report").symbol_parse_failures,
                0
            );
        } else {
            assert!(report.l2.is_none());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    kind: String,
    name: String,
    entity_type: Option<String>,
    owner: String,
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    entities: BTreeMap<String, Identity>,
    source_paths: BTreeMap<String, String>,
    edges: BTreeMap<String, (String, String, String)>,
}

async fn snapshot(rt: &KhiveRuntime, owner: &str, tiers: Tiers) -> Snapshot {
    let access = rt.sql();
    let mut reader = access.reader().await.expect("memory reader");
    let rows = reader.query_all(SqlStatement {
        sql: "SELECT id, kind, name, entity_type, properties FROM entities WHERE deleted_at IS NULL ORDER BY id".into(),
        params: vec![], label: Some("test.fallback_project.entities".into()),
    }).await.unwrap();
    let mut entities = BTreeMap::new();
    let mut source_paths = BTreeMap::new();
    for row in rows {
        let id = row.text("id").unwrap().to_string();
        let props: Value = serde_json::from_str(row.text("properties").unwrap()).unwrap();
        assert_eq!(props["source_project"], owner);
        let identity = Identity {
            kind: row.text("kind").unwrap().to_string(),
            name: row.text("name").unwrap().to_string(),
            entity_type: row.opt_text("entity_type").unwrap().map(str::to_string),
            owner: props["source_project"].as_str().unwrap().to_string(),
        };
        if identity.kind == "project" {
            assert_eq!(identity.name, owner);
        } else {
            let source_path = props["source_path"].as_str().expect("source provenance");
            assert!(!Path::new(source_path).is_absolute());
            assert!(source_path.ends_with("src/lib.rs") || source_path.ends_with("src/util.rs"));
            source_paths.insert(id.clone(), source_path.to_string());
        }
        assert!(entities.insert(id, identity).is_none());
    }
    assert_eq!(
        entities
            .values()
            .filter(|row| row.kind == "project")
            .count(),
        1
    );
    assert!(
        entities
            .values()
            .filter(|row| row.entity_type.as_deref() == Some("module"))
            .count()
            >= 2
    );
    let functions = entities
        .values()
        .filter(|row| row.entity_type.as_deref() == Some("function"))
        .count();
    if tiers.symbols {
        assert!(functions >= 2);
    } else {
        assert_eq!(functions, 0);
    }
    let rows = reader.query_all(SqlStatement {
        sql: "SELECT id, source_id, target_id, relation FROM graph_edges WHERE deleted_at IS NULL ORDER BY id".into(),
        params: vec![], label: Some("test.fallback_project.edges".into()),
    }).await.unwrap();
    let mut edges = BTreeMap::new();
    for row in rows {
        let value = (
            row.text("source_id").unwrap().to_string(),
            row.text("target_id").unwrap().to_string(),
            row.text("relation").unwrap().to_string(),
        );
        assert!(entities.contains_key(&value.0));
        assert!(entities.contains_key(&value.1));
        assert!(edges
            .insert(row.text("id").unwrap().to_string(), value)
            .is_none());
    }
    assert!(edges
        .values()
        .any(|(_, _, relation)| relation == "contains"));
    if tiers.imports {
        assert!(edges
            .values()
            .any(|(source, target, relation)| relation == "depends_on"
                && entities[source].name == "crate"
                && entities[target].name == "util"));
    }
    Snapshot {
        entities,
        source_paths,
        edges,
    }
}

async fn equivalent_spellings(tiers: Tiers, manifest: bool) {
    for parent_first in [false, true] {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        source_tree(&project, manifest);
        let parent_spelling = project.join("child").join("..");
        let dotted_spelling = project.join(".");
        let owner = if manifest {
            "manifest_owner"
        } else {
            "project"
        };
        let (first, second) = if parent_first {
            (&parent_spelling, &project)
        } else {
            (&project, &parent_spelling)
        };
        let rt = runtime();
        warm(&rt, first, tiers).await;
        let baseline = snapshot(&rt, owner, tiers).await;
        warm(&rt, second, tiers).await;
        let after = snapshot(&rt, owner, tiers).await;
        assert_eq!(
            after.entities.len(),
            baseline.entities.len(),
            "no second entity family"
        );
        assert_eq!(
            after.edges.len(),
            baseline.edges.len(),
            "no second natural-edge family"
        );
        assert_eq!(
            after, baseline,
            "all persisted IDs and source paths remain stable"
        );
        warm(&rt, &dotted_spelling, tiers).await;
        assert_eq!(snapshot(&rt, owner, tiers).await, baseline);
    }
}

#[tokio::test]
async fn parent_directory_spelling_preserves_l1_5_identity() {
    equivalent_spellings(IMPORTS, false).await;
}

#[tokio::test]
async fn parent_directory_spelling_preserves_l2_identity() {
    equivalent_spellings(SYMBOLS, false).await;
}

#[tokio::test]
async fn parent_directory_spelling_preserves_combined_identity() {
    equivalent_spellings(BOTH, false).await;
}

#[tokio::test]
async fn manifest_name_still_wins_for_both_spellings_and_tiers() {
    equivalent_spellings(
        Tiers {
            manifests: true,
            ..IMPORTS
        },
        true,
    )
    .await;
    equivalent_spellings(SYMBOLS, true).await;
    equivalent_spellings(
        Tiers {
            manifests: true,
            ..BOTH
        },
        true,
    )
    .await;
}

#[tokio::test]
async fn distinct_roots_with_the_same_basename_keep_name_based_ids() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("first/project");
    let second = root.path().join("second/project");
    source_tree(&first, false);
    source_tree(&second, false);
    for tiers in [IMPORTS, SYMBOLS, BOTH] {
        let rt = runtime();
        warm(&rt, &first, tiers).await;
        let baseline = snapshot(&rt, "project", tiers).await;
        warm(&rt, &second, tiers).await;
        let after = snapshot(&rt, "project", tiers).await;
        assert_eq!(after.entities, baseline.entities);
        assert_eq!(after.edges, baseline.edges);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_present_symlink_basename_remains_the_project_name() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("physical_project");
    let alias = root.path().join("caller_alias");
    source_tree(&project, false);
    std::os::unix::fs::symlink(&project, &alias).unwrap();
    for tiers in [IMPORTS, SYMBOLS, BOTH] {
        let rt = runtime();
        warm(&rt, &alias, tiers).await;
        let baseline = snapshot(&rt, "caller_alias", tiers).await;
        warm(&rt, &alias, tiers).await;
        assert_eq!(snapshot(&rt, "caller_alias", tiers).await, baseline);
    }
}
