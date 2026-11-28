//! ADR-220 arms 3, 4 and 8 again, with the store's own `exec` pack in place of
//! the stand-in.
//!
//! The store here can launch a real sandboxed process, so a run that reaches
//! it leaves the file its command writes under the store's exec root, and the
//! root is kept after the run so the file can be looked for. The computer is
//! the same test computer as in the other arms. Only macOS has a sandbox
//! backend, so these arms build there alone, and the two that launch a process
//! on the store (4 and 8) pass only where the sandbox can start: the hosted
//! macOS checks.

use std::path::Path;
use std::sync::Arc;

use khive_db::stores::blob::FsBlobStore;
use khive_pack_blob::BlobPack;
use khive_pack_exec::ExecPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::ExecSectionConfig;
use khive_runtime::{KhiveConfig, KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::Value;

use super::super::super::KhiveMcpServer;
use super::{frames, reset_frames, Caller, Runs, TestComputer, ACTOR, SENTINEL};

const EXEC_ROOT: &str = "store-exec-root";

/// A bridge over a file-backed store whose `exec` pack is the real one, with
/// `/bin/sh` registered and allowed, and the `exec.run` call that writes the
/// sentinel file from an empty tree. The setup calls go through the bridge
/// before any computer is attached, and the frames they leave are cleared.
async fn real_exec_store() -> (Caller, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("main.db")),
        actor_id: Some(ACTOR.to_string()),
        exec: ExecSectionConfig {
            root: Some(dir.path().join(EXEC_ROOT).to_string_lossy().into_owned()),
            read_roots: vec!["/bin".into(), "/usr/bin".into()],
            keep: true,
            ..ExecSectionConfig::default()
        },
        // No embedding model: tool.register would otherwise build the
        // default embedder, which needs a model file the host may not have.
        ..RuntimeConfig::no_embeddings()
    })
    .expect("the store's runtime");
    let blobs = FsBlobStore::new(dir.path().join("blobs"), 0).expect("the store's blob store");
    runtime
        .install_blob_store(Arc::new(blobs))
        .expect("install the blob store");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(ACTOR.into()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(BlobPack::new(runtime.clone()));
    builder.register(ToolPack::new(runtime.clone()));
    builder.register(ExecPack::new(runtime.clone()));
    builder
        .with_runtime_event_store(&runtime)
        .expect("the store's event store");
    let registry = builder.build().expect("store registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    let server = KhiveMcpServer::from_registry(registry).with_runtime(runtime.clone());
    let store = Caller {
        dir,
        server,
        runtime,
        runs: Runs::default(),
    };

    for setup in [
        r#"tool.register(name="sh", kind="tool", description="shell", source="exec:/bin/sh", side_effect="write", trust="first_party")"#,
        r#"tool.policy(actor="*", tool="sh", decision="allow")"#,
    ] {
        let answer = store.request(setup).await;
        assert_eq!(answer["results"][0]["ok"], true, "{answer}");
    }
    let tree = store.request("exec.tree(entries=[])").await;
    let tree = tree["results"][0]["result"]["tree"]
        .as_str()
        .unwrap_or_else(|| panic!("an empty tree: {tree}"))
        .to_owned();
    reset_frames();
    let run = format!(
        r#"exec.run(tree="{tree}", tool="sh", actor="{ACTOR}", args=["-c", "printf ran > {SENTINEL}"], declared_write_paths=["{SENTINEL}"])"#
    );
    (store, run)
}

/// Every file named [`SENTINEL`] under the store's exec root.
fn store_sentinels(store: &Caller) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if entry.file_name() == SENTINEL {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&store.dir.path().join(EXEC_ROOT), &mut found);
    found
}

/// The entries the store's exec root holds; none when it was never made.
fn exec_root_entries(store: &Caller) -> usize {
    std::fs::read_dir(store.dir.path().join(EXEC_ROOT)).map_or(0, |entries| entries.count())
}

fn assert_ran_here(run: &Value) {
    assert_eq!(run["results"][0]["ok"], true, "{run}");
    let receipt = &run["results"][0]["result"]["receipt"];
    assert_eq!(receipt["success"], true, "{run}");
    assert_eq!(receipt["actor"], ACTOR, "{run}");
    assert_eq!(receipt["tool"], "sh", "{run}");
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_3_with_the_real_exec_pack_nothing_runs_on_the_store_machine() {
    let (store, run) = real_exec_store().await;
    let computer = TestComputer::start(ACTOR);
    let caller = store.routing_to(&computer);

    let answer = caller.request(&run).await;
    assert_eq!(answer["results"][0]["ok"], true, "{answer}");

    assert!(
        frames().is_empty(),
        "no frame for the daemon: {:?}",
        frames()
    );
    assert!(
        store_sentinels(&caller).is_empty(),
        "no sentinel file on the store's machine"
    );
    assert_eq!(exec_root_entries(&caller), 0, "no run directory");
    assert_eq!(caller.rows("exec.run").await, 0, "no dispatch row");
    assert_eq!(computer.runs.all().len(), 1);
    assert!(computer.sentinel());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_4_with_the_real_exec_pack_and_routing_off_the_store_runs_the_command() {
    let (store, run) = real_exec_store().await;
    let computer = TestComputer::start(ACTOR);
    let mut caller = store.routing_to(&computer);
    // The daemon's own server: the computer is dropped before it serves.
    caller.server = caller.server.without_computer();
    assert!(caller.server.computer.is_none());

    let answer = caller.request(&run).await;
    assert_ran_here(&answer);

    assert_eq!(frames().len(), 1, "{:?}", frames());
    assert!(frames()[0].contains("exec.run"), "{:?}", frames());
    let sentinels = store_sentinels(&caller);
    assert_eq!(sentinels.len(), 1, "{sentinels:?}");
    assert_eq!(std::fs::read(&sentinels[0]).expect("the sentinel"), b"ran");
    assert!(
        caller.rows("exec.run").await >= 1,
        "the dispatch row is read"
    );
    assert!(computer.calls().is_empty());
    assert!(!computer.sentinel());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_8_with_the_real_exec_pack_and_no_computer_table_exec_runs_on_the_store() {
    let (mut store, run) = real_exec_store().await;
    let config: KhiveConfig =
        toml::from_str(&format!("[actor]\nid = \"{ACTOR}\"\n")).expect("a configuration");
    store.server = store
        .server
        .with_computer(&config)
        .expect("no table is a valid configuration");
    assert!(store.server.computer.is_none());

    let answer = store.request(&run).await;
    assert_ran_here(&answer);

    let sentinels = store_sentinels(&store);
    assert_eq!(sentinels.len(), 1, "{sentinels:?}");
    assert_eq!(std::fs::read(&sentinels[0]).expect("the sentinel"), b"ran");
    assert_eq!(frames().len(), 1, "{:?}", frames());
    assert!(frames()[0].contains("exec.run"), "{:?}", frames());
}
