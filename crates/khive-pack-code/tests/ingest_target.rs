//! Public dispatch controls for explicit existing-only map targets and the
//! deliberately creatable workspace default. These do not model path races.

use std::path::{Path, PathBuf};

use khive_pack_code::CodePack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    source: PathBuf,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("isolated ingest fixture");
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let runtime = KhiveRuntime::memory().unwrap();
        Self {
            root,
            source,
            registry: registry(runtime),
        }
    }

    async fn ingest(&self, db: Option<&Path>) -> Result<Value, RuntimeError> {
        let mut args = json!({"path":self.source, "tiers":[]});
        if let Some(db) = db {
            args["db"] = json!(db);
        }
        self.registry.dispatch("code.ingest", args).await
    }

    async fn assert_refused_without_creating(&self, target: &Path) {
        let before = directory_listing(self.root.path());
        let error = self
            .ingest(Some(target))
            .await
            .expect_err("explicit target preflight must refuse");
        assert!(
            matches!(&error, RuntimeError::InvalidInput(message)
                if message.contains("existing regular file") && message.contains(&target.display().to_string())),
            "typed target refusal must name the path: {error:?}"
        );
        assert_eq!(
            directory_listing(self.root.path()),
            before,
            "refusal created filesystem artifacts"
        );
    }
}

fn registry(runtime: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(CodePack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn directory_listing(root: &Path) -> Vec<PathBuf> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            entries.push(entry.path().strip_prefix(root).unwrap().to_path_buf());
            if entry.file_type().unwrap().is_dir() {
                visit(root, &entry.path(), entries);
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort();
    entries
}

// MUST-FAIL: moving syntax admission after path.is_dir() returns the source-path
// error; removing it admits SQLite URI/relative spellings to target resolution.
#[tokio::test]
async fn explicit_db_syntax_refuses_before_source_or_target_filesystem_probes() {
    let fixture = Fixture::new();
    let missing_source = fixture.root.path().join("missing-source");
    let missing_target = fixture.root.path().join("missing-map.db");
    let before = directory_listing(fixture.root.path());
    for db in [
        format!("file:{}", missing_target.display()),
        format!("file:{}?mode=rw", missing_target.display()),
        format!("{}?mode=rw", missing_target.display()),
        "relative-map.db".to_string(),
        "".to_string(),
    ] {
        for source in [&fixture.source, &missing_source] {
            let error = fixture
                .registry
                .dispatch("code.ingest", json!({"path":source, "db":db, "tiers":[]}))
                .await
                .expect_err("explicit db syntax must refuse at parameter admission");
            assert!(
                matches!(&error, RuntimeError::InvalidInput(message)
                    if message.contains("absolute, plain filesystem path")),
                "expected syntax refusal before any filesystem error: {error:?}"
            );
        }
    }
    assert_eq!(directory_listing(fixture.root.path()), before);
}

// MUST-FAIL: deleting the explicit-target preflight lets ordinary runtime
// construction create/migrate both the missing file and any missing parent.
#[tokio::test]
async fn explicit_missing_target_refuses_without_creating_files_or_parent() {
    let fixture = Fixture::new();
    for target in [
        fixture.root.path().join("mistyped-map.db"),
        fixture.root.path().join("missing-parent").join("map.db"),
    ] {
        fixture.assert_refused_without_creating(&target).await;
        assert!(!target.exists());
    }
    assert!(!fixture.root.path().join("missing-parent").exists());
}

#[tokio::test]
async fn explicit_directory_refuses_before_runtime_construction() {
    let fixture = Fixture::new();
    let target = fixture.root.path().join("not-a-file");
    std::fs::create_dir(&target).unwrap();
    fixture.assert_refused_without_creating(&target).await;
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_dangling_symlink_refuses_without_creating_its_target() {
    let fixture = Fixture::new();
    let missing = fixture.root.path().join("missing.db");
    let link = fixture.root.path().join("map-link.db");
    std::os::unix::fs::symlink(&missing, &link).unwrap();
    fixture.assert_refused_without_creating(&link).await;
    assert!(!missing.exists());
    assert_eq!(std::fs::read_link(link).unwrap(), missing);
}

#[tokio::test]
async fn explicit_precreated_empty_file_initializes_and_reopens() {
    let fixture = Fixture::new();
    let target = fixture.root.path().join("intentional-map.db");
    std::fs::File::create(&target).unwrap();
    assert_eq!(std::fs::metadata(&target).unwrap().len(), 0);
    let first = fixture
        .ingest(Some(&target))
        .await
        .expect("pre-created dedicated file may initialize");
    assert_eq!(first["db_path"], json!(target));
    assert!(std::fs::metadata(&target).unwrap().len() > 0);
    let second = fixture
        .ingest(Some(&target))
        .await
        .expect("existing initialized map remains usable");
    assert_eq!(second["db_path"], json!(target));
    assert_eq!(second["projects_created"], 0);
}

// MUST-FAIL: applying the existing-file restriction to omitted db prevents
// the documented first-ingest creation of the dedicated workspace map.
#[tokio::test]
async fn omitted_target_creates_workspace_default() {
    let fixture = Fixture::new();
    let target = fixture.source.join(".khive").join("code-map.db");
    assert!(!target.exists());
    let response = fixture
        .ingest(None)
        .await
        .expect("default map creation remains deliberate");
    assert_eq!(response["db_path"], json!(target));
    assert!(target.is_file());
    assert!(std::fs::metadata(target).unwrap().len() > 0);
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_symlink_to_existing_dedicated_file_remains_accepted() {
    let fixture = Fixture::new();
    let target = fixture.root.path().join("dedicated.db");
    let link = fixture.root.path().join("dedicated-link.db");
    std::fs::File::create(&target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let response = fixture
        .ingest(Some(&link))
        .await
        .expect("regular-file symlink preserves compatibility");
    assert_eq!(response["db_path"], json!(link));
    assert!(std::fs::metadata(target).unwrap().len() > 0);
}

#[tokio::test]
async fn explicit_current_runtime_database_is_still_refused() {
    let fixture = Fixture::new();
    let main = fixture.root.path().join("production.db");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(main.clone()),
        packs: vec!["kg".into(), "code".into()],
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let registry = registry(runtime);
    let before = directory_listing(fixture.root.path());
    let error = registry
        .dispatch(
            "code.ingest",
            json!({"path":fixture.source,"db":main,"tiers":[]}),
        )
        .await
        .expect_err("existing production DB is not a dedicated map");
    assert!(
        matches!(&error, RuntimeError::InvalidInput(message) if message.contains("shared production database")),
        "{error:?}"
    );
    assert_eq!(directory_listing(fixture.root.path()), before);
}

#[tokio::test]
async fn invalid_ingest_arguments_still_refuse_before_default_creation() {
    let fixture = Fixture::new();
    for args in [
        json!({"path":fixture.source,"tiers":["unknown"]}),
        json!({"path":fixture.source,"languages":["unknown"]}),
        json!({"path":fixture.root.path().join("missing-source")}),
        json!({"path":fixture.source,"unknown":true}),
    ] {
        let before = directory_listing(fixture.root.path());
        let error = fixture
            .registry
            .dispatch("code.ingest", args)
            .await
            .expect_err("invalid input must refuse before creating the default map");
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
        assert_eq!(directory_listing(fixture.root.path()), before);
    }
}
