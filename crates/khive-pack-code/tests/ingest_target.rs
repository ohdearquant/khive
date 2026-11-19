//! Public dispatch controls for explicit existing-only map targets and the
//! deliberately creatable workspace default. These do not model path races.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

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
        // The guarded VFS refuses symlink components of explicit targets.
        // macOS's ordinary /var TMPDIR spelling aliases /private/var, so
        // positive targets must be constructed under the physical temp root.
        let physical_temp = std::env::temp_dir()
            .canonicalize()
            .expect("physical temporary directory");
        let root = tempfile::tempdir_in(physical_temp).expect("isolated ingest fixture");
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
        self.assert_refused_without_creating_with_reason(target, "existing regular file")
            .await;
    }

    async fn assert_refused_without_creating_with_reason(&self, target: &Path, reason: &str) {
        let before = directory_listing(self.root.path());
        let error = self
            .ingest(Some(target))
            .await
            .expect_err("explicit target preflight must refuse");
        assert!(
            matches!(&error, RuntimeError::InvalidInput(message)
                if message.contains(reason) && message.contains(&target.display().to_string())),
            "typed target refusal must name the path: {error:?}"
        );
        assert_eq!(
            directory_listing(self.root.path()),
            before,
            "refusal created filesystem artifacts"
        );
    }
}

/// Positive VFS tests must not sample the runner's real HOME production DB.
/// Run each one in a child with a private HOME, without mutating process-wide
/// environment while the integration tests execute concurrently.
fn run_with_private_home_in_child() -> bool {
    const CHILD_TEST: &str = "KHIVE_INGEST_TARGET_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    if std::env::var(CHILD_TEST).ok().as_deref() == Some(name) {
        return false;
    }
    let physical_temp = std::env::temp_dir()
        .canonicalize()
        .expect("physical temporary directory");
    let home = tempfile::tempdir_in(physical_temp).expect("private child HOME");
    let output = Command::new(std::env::current_exe().expect("integration test executable"))
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD_TEST, name)
        .env("HOME", home.path())
        .env("KHIVE_DB", "")
        .output()
        .expect("spawn isolated ingest test");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
        "isolated ingest test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn registry(runtime: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(CodePack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn file_backed_registry(main: &Path, declared_backend_db_paths: Vec<PathBuf>) -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(main.to_path_buf()),
        packs: vec!["kg".into(), "code".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
    .with_declared_backend_db_paths(declared_backend_db_paths.into());
    registry(runtime)
}

fn create_runtime_database(path: &Path) {
    drop(
        KhiveRuntime::new(RuntimeConfig {
            db_path: Some(path.to_path_buf()),
            packs: vec!["kg".into(), "code".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap(),
    );
}

#[derive(Debug, PartialEq, Eq)]
struct FileSnapshot {
    bytes: Vec<u8>,
    len: u64,
    modified: SystemTime,
}

fn file_snapshot(path: &Path) -> FileSnapshot {
    let metadata = std::fs::metadata(path).unwrap();
    FileSnapshot {
        bytes: std::fs::read(path).unwrap(),
        len: metadata.len(),
        modified: metadata.modified().unwrap(),
    }
}

async fn assert_protected_target_refused(
    registry: &VerbRegistry,
    source: &Path,
    target: &Path,
    matched_member: &Path,
) {
    let before = file_snapshot(target);
    let error = registry
        .dispatch("code.ingest", json!({"path":source,"db":target,"tiers":[]}))
        .await
        .expect_err("production store must be refused before opening the target");
    assert!(
        matches!(&error, RuntimeError::InvalidInput(message)
            if message.contains("shared production database")
                && message.contains(&matched_member.display().to_string())),
        "refusal must name the matched production store: {error:?}"
    );
    assert_eq!(file_snapshot(target), before, "refusal mutated the target");
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
    fixture
        .assert_refused_without_creating_with_reason(&link, "final-component symlink")
        .await;
    assert!(!missing.exists());
    assert_eq!(std::fs::read_link(link).unwrap(), missing);
}

#[tokio::test]
async fn explicit_precreated_empty_file_initializes_and_reopens() {
    if run_with_private_home_in_child() {
        return;
    }
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

#[cfg(target_os = "macos")]
#[tokio::test]
async fn explicit_var_parent_alias_refuses_without_rewriting_target_path() {
    if run_with_private_home_in_child() {
        return;
    }
    let fixture = Fixture::new();
    let physical_temp = std::env::temp_dir()
        .canonicalize()
        .expect("physical temporary directory");
    let physical_parent =
        tempfile::tempdir_in(physical_temp).expect("physical macOS /var alias control directory");
    let target = physical_parent.path().join("parent-alias-map.db");
    std::fs::File::create(&target).unwrap();
    let suffix = target
        .strip_prefix("/private/var")
        .expect("macOS temporary root is under /private/var");
    let alias = Path::new("/var").join(suffix);
    let error = fixture
        .ingest(Some(&alias))
        .await
        .expect_err("explicit parent symlink must not be canonicalized away");
    assert!(
        error.to_string().contains("code-map VFS cannot prove"),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("symlinked code-map parent component /var"),
        "{error:?}"
    );
    assert_eq!(std::fs::metadata(target).unwrap().len(), 0);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn omitted_var_parent_alias_creates_physical_default() {
    if run_with_private_home_in_child() {
        return;
    }
    let physical_temp = std::env::temp_dir()
        .canonicalize()
        .expect("physical temporary directory");
    let root =
        tempfile::tempdir_in(physical_temp).expect("physical macOS /var alias control directory");
    let physical_source = root.path().join("source");
    std::fs::create_dir(&physical_source).unwrap();
    let suffix = physical_source
        .strip_prefix("/private/var")
        .expect("source is below the physical /var directory");
    let alias_source = Path::new("/var").join(suffix);
    let physical_target = physical_source.join(".khive").join("code-map.db");
    let registry = registry(KhiveRuntime::memory().unwrap());
    let response = registry
        .dispatch("code.ingest", json!({"path": alias_source, "tiers": []}))
        .await
        .expect("omitted db resolves its configured parent alias once");
    assert_eq!(response["db_path"], json!(physical_target));
    assert!(physical_target.is_file());
}

// MUST-FAIL: applying the existing-file restriction to omitted db prevents
// the documented first-ingest creation of the dedicated workspace map.
#[tokio::test]
async fn omitted_target_creates_workspace_default() {
    if run_with_private_home_in_child() {
        return;
    }
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
async fn explicit_symlink_to_existing_dedicated_file_refuses_without_mutation() {
    if run_with_private_home_in_child() {
        return;
    }
    let fixture = Fixture::new();
    let target = fixture.root.path().join("dedicated.db");
    let link = fixture.root.path().join("dedicated-link.db");
    std::fs::File::create(&target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let before = file_snapshot(&target);
    fixture
        .assert_refused_without_creating_with_reason(&link, "final-component symlink")
        .await;
    assert_eq!(file_snapshot(&target), before, "refusal mutated the map");
    assert_eq!(std::fs::read_link(&link).unwrap(), target);
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
async fn explicit_events_database_beside_runtime_main_is_refused_before_mutation() {
    let fixture = Fixture::new();
    let main = fixture.root.path().join("production.db");
    create_runtime_database(&main);
    let events = khive_runtime::events_split::events_db_path_beside(&main);
    create_runtime_database(&events);
    let registry = file_backed_registry(&main, vec![]);

    assert_protected_target_refused(&registry, &fixture.source, &events, &events).await;
}

#[tokio::test]
async fn explicit_declared_secondary_backend_is_refused_before_mutation() {
    let fixture = Fixture::new();
    let main = fixture.root.path().join("production.db");
    let secondary = fixture.root.path().join("secondary.db");
    create_runtime_database(&secondary);
    let registry = file_backed_registry(&main, vec![secondary.clone()]);

    assert_protected_target_refused(&registry, &fixture.source, &secondary, &secondary).await;
}

#[tokio::test]
async fn default_map_target_colliding_with_declared_backend_is_refused_before_mutation() {
    let fixture = Fixture::new();
    let main = fixture.root.path().join("production.db");
    let default_map = fixture.source.join(".khive").join("code-map.db");
    std::fs::create_dir_all(default_map.parent().unwrap()).unwrap();
    create_runtime_database(&default_map);
    let registry = file_backed_registry(&main, vec![default_map.clone()]);
    let before = file_snapshot(&default_map);

    let error = registry
        .dispatch("code.ingest", json!({"path":fixture.source,"tiers":[]}))
        .await
        .expect_err("the default must not bypass the production deny set");
    assert!(
        matches!(&error, RuntimeError::InvalidInput(message)
            if message.contains("shared production database")
                && message.contains(&default_map.display().to_string())),
        "refusal must name the declared backend: {error:?}"
    );
    assert_eq!(file_snapshot(&default_map), before);
}

#[cfg(unix)]
#[tokio::test]
async fn hardlink_to_runtime_main_is_refused_but_fresh_inode_copy_is_accepted() {
    if run_with_private_home_in_child() {
        return;
    }
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    let main = fixture.root.path().join("production.db");
    let alias = fixture.root.path().join("unrelated-name.db");
    let copy = fixture.root.path().join("independent-map.db");
    create_runtime_database(&main);
    std::fs::hard_link(&main, &alias).unwrap();
    std::fs::copy(&main, &copy).unwrap();
    assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(&main).unwrap());
    drop(
        KhiveRuntime::new_readonly(RuntimeConfig {
            db_path: Some(copy.clone()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("closed source runtime leaves a standalone, readable database copy"),
    );
    let main_identity = std::fs::metadata(&main).unwrap();
    let alias_identity = std::fs::metadata(&alias).unwrap();
    let copy_identity = std::fs::metadata(&copy).unwrap();
    assert_eq!(
        (alias_identity.dev(), alias_identity.ino()),
        (main_identity.dev(), main_identity.ino())
    );
    assert_ne!(
        (copy_identity.dev(), copy_identity.ino()),
        (main_identity.dev(), main_identity.ino())
    );

    let registry = file_backed_registry(&main, vec![]);
    assert_protected_target_refused(&registry, &fixture.source, &alias, &main).await;
    let response = registry
        .dispatch(
            "code.ingest",
            json!({"path":fixture.source,"db":copy,"tiers":[]}),
        )
        .await
        .expect("a byte copy with a new inode is a distinct target");
    assert_eq!(response["db_path"], json!(copy));
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
