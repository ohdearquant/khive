use super::*;
use khive_pack_blob::BlobPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::ExecSectionConfig;
use khive_runtime::{RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::BlobStore;

struct Fixture {
    registry: VerbRegistry,
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("exec-root");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(dir.path().join("khive.db")),
            exec: ExecSectionConfig {
                root: Some(root.to_string_lossy().into_owned()),
                read_roots: vec!["/bin".into(), "/usr/bin".into()],
                ..Default::default()
            },
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let blob_store =
            khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
        runtime
            .install_blob_store(Arc::new(blob_store) as Arc<dyn BlobStore>)
            .unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(BlobPack::new(runtime.clone()));
        builder.register(ToolPack::new(runtime.clone()));
        builder.register(crate::ExecPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self {
            registry,
            _dir: dir,
            root,
        }
    }

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry
            .dispatch(verb, params)
            .await
            .unwrap_or_else(|error| panic!("{verb}: {error}"))
    }

    async fn ready_tree(&self) -> String {
        self.call(
            "tool.register",
            json!({
                "name": "sh",
                "kind": "tool",
                "description": "shell",
                "source": "exec:/bin/sh",
                "side_effect": "write",
                "trust": "first_party"
            }),
        )
        .await;
        self.call(
            "tool.policy",
            json!({"actor": "*", "tool": "sh", "decision": "allow"}),
        )
        .await;
        self.call("exec.tree", json!({"entries": []})).await["tree"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

struct HookReset(PathBuf);

impl Drop for HookReset {
    fn drop(&mut self) {
        CAPTURE_BEFORE_READ_HOOK
            .lock()
            .unwrap()
            .retain(|(root, _)| root != &self.0);
    }
}

#[tokio::test]
async fn run_capture_refuses_symlink_in_run_directory() {
    let fixture = Fixture::new();
    let tree = fixture.ready_tree().await;
    let outside = fixture._dir.path().join("outside");
    std::fs::write(&outside, b"outside bytes").unwrap();
    let target = outside.clone();
    CAPTURE_BEFORE_READ_HOOK.lock().unwrap().push((
        fixture.root.clone(),
        Arc::new(move |path: &Path| {
            if path
                .file_name()
                .is_some_and(|name| name == std::ffi::OsStr::new("output"))
            {
                std::fs::remove_file(path).unwrap();
                std::os::unix::fs::symlink(&target, path).unwrap();
            }
        }),
    ));
    let _hook_reset = HookReset(fixture.root.clone());

    let result = fixture
        .call(
            "exec.run",
            json!({
                "tree": tree,
                "tool": "sh",
                "args": ["-c", "printf inside > output"],
                "actor": "local"
            }),
        )
        .await;
    let receipt = &result["receipt"];
    assert_eq!(receipt["exit_code"], 0, "{receipt}");
    assert_eq!(receipt["success"], false, "{receipt}");
    assert!(receipt["tree_out"].is_null(), "{receipt}");
    assert_eq!(receipt["changed"], json!([]), "{receipt}");
    let reason = receipt["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("capture entry \"output\"") && reason.contains("changed after inspection"),
        "{receipt}"
    );
    assert!(!result.to_string().contains("outside bytes"));
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside bytes");
}

#[test]
fn run_capture_refuses_fifo_in_run_directory_without_blocking() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let fixture = Fixture::new();
            let tree = fixture.ready_tree().await;
            CAPTURE_BEFORE_READ_HOOK.lock().unwrap().push((
                fixture.root.clone(),
                Arc::new(|path: &Path| {
                    if path
                        .file_name()
                        .is_some_and(|name| name == std::ffi::OsStr::new("output"))
                    {
                        use std::os::unix::ffi::OsStrExt;
                        std::fs::remove_file(path).unwrap();
                        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                    }
                }),
            ));
            let _hook_reset = HookReset(fixture.root.clone());
            fixture
                .call(
                    "exec.run",
                    json!({
                        "tree": tree,
                        "tool": "sh",
                        "args": ["-c", "printf inside > output"],
                        "actor": "local"
                    }),
                )
                .await
        });
        sender.send(result).unwrap();
    });
    let result = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("run capture blocked on a FIFO entry");
    worker.join().unwrap();
    let receipt = &result["receipt"];
    assert_eq!(receipt["exit_code"], 0, "{receipt}");
    assert_eq!(receipt["success"], false, "{receipt}");
    assert!(receipt["tree_out"].is_null(), "{receipt}");
    assert!(
        receipt["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("capture entry changed after inspection"),
        "{receipt}"
    );
}

#[tokio::test]
async fn run_that_removes_its_run_directory_reports_degraded_capture() {
    let fixture = Fixture::new();
    let tree = fixture.ready_tree().await;

    let complete = fixture
        .call(
            "exec.run",
            json!({
                "tree": tree,
                "tool": "sh",
                "args": ["-c", "printf inside > output"],
                "actor": "local"
            }),
        )
        .await;
    let receipt = &complete["receipt"];
    assert_eq!(receipt["success"], true, "{receipt}");
    assert_eq!(receipt["tree_capture"], "complete", "{receipt}");
    assert!(receipt["tree_capture_detail"].is_null(), "{receipt}");

    let result = fixture
        .call(
            "exec.run",
            json!({
                "tree": tree,
                "tool": "sh",
                "args": ["-c", "d=$PWD; cd / && rm -rf \"$d\" && test ! -e \"$d\""],
                "actor": "local"
            }),
        )
        .await;
    let receipt = &result["receipt"];
    // The tool's own exit is recorded unchanged; capture's verdict is separate.
    assert_eq!(receipt["exit_code"], 0, "{receipt}");
    assert_eq!(receipt["timed_out"], false, "{receipt}");
    assert_eq!(receipt["tree_capture"], "degraded", "{receipt}");
    let detail = receipt["tree_capture_detail"].as_str().unwrap_or_default();
    assert!(detail.starts_with("root_missing: F_GETPATH"), "{receipt}");
    assert_eq!(receipt["success"], false, "{receipt}");
    assert!(receipt["tree_out"].is_null(), "{receipt}");
    assert_eq!(receipt["changed"], json!([]), "{receipt}");
    let reason = receipt["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("capture degraded: root_missing"),
        "{receipt}"
    );
}
