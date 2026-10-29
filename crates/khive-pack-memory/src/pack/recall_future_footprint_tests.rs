use super::*;

use khive_runtime::{Namespace, VerbRegistryBuilder};

#[test]
#[serial_test::serial(config_ledger)]
fn deadline_wrapper_does_not_embed_the_recall_pipeline() {
    // Measure on an explicitly roomy stack so the regression reports the
    // historical inline footprint instead of aborting the test process.
    let (pipeline_bytes, profiled_bytes, deadline_bytes) = std::thread::Builder::new()
        .name("recall-future-footprint".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let token = runtime
                .authorize(Namespace::local())
                .expect("authorize local");
            let registry = VerbRegistryBuilder::new().build().expect("empty registry");
            let pack = MemoryPack::new(runtime);

            let pipeline_bytes = std::mem::size_of_val(&pack.handle_recall(
                &token,
                serde_json::json!({"query": "future footprint probe"}),
                &registry,
                None,
            ));
            let profiled_bytes = std::mem::size_of_val(&pack.handle_recall_profiled(
                &token,
                serde_json::json!({"query": "future footprint probe"}),
                &registry,
                None,
            ));
            let deadline_bytes = std::mem::size_of_val(&pack.handle_recall_with_deadline(
                &token,
                serde_json::json!({"query": "future footprint probe"}),
                &registry,
            ));
            (pipeline_bytes, profiled_bytes, deadline_bytes)
        })
        .expect("spawn footprint measurement")
        .join()
        .expect("footprint measurement panicked");

    assert!(
        profiled_bytes < pipeline_bytes,
        "the profiled wrapper must hold the recall pipeline behind a pointer in both branches: \
             wrapper={profiled_bytes}B pipeline={pipeline_bytes}B"
    );
    assert!(
        deadline_bytes < pipeline_bytes,
        "the deadline wrapper must hold the recall pipeline behind a pointer: \
             wrapper={deadline_bytes}B pipeline={pipeline_bytes}B"
    );
    // On the 64-bit CI target the unboxed candidate collector made this
    // future 8,968 bytes; boxing that 6,792-byte child reduces it to 4,888
    // bytes. Seven KiB leaves ample platform/layout headroom while still
    // catching either candidate await being inlined again.
    assert!(
        pipeline_bytes < 7 * 1024,
        "the recall pipeline re-embedded its candidate fan-out: \
             pipeline={pipeline_bytes}B"
    );
}

// This exercises the pack registry path, not the daemon/MCP request stack.
// Each profile mode is a fresh process because the profile flag is cached.
#[test]
fn recall_dispatch_fits_two_mib_worker_in_child() {
    use std::io::Read;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn read_bounded_log(path: &Path) -> String {
        let mut bytes = Vec::new();
        if let Ok(file) = std::fs::File::open(path) {
            let _ = file.take(128 * 1024).read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    let scratch = tempfile::tempdir().expect("parent-owned synthetic scratch");
    let executable = std::env::current_exe().expect("test executable");
    let module = module_path!()
        .split_once("::")
        .expect("test module path has a crate prefix")
        .1;
    let child_test = format!("{module}::recall_two_mib_worker_child");
    for profile in [false, true] {
        let mode = if profile { "profiled" } else { "plain" };
        // Validate the same fixture with a roomy stack before the 2 MiB arm.
        for stack_mib in [16, 2] {
            let arm = format!("{mode}-{stack_mib}mib");
            let child_root = scratch.path().join(&arm);
            std::fs::create_dir(&child_root).expect("child scratch directory");
            let log_path = scratch.path().join(format!("{arm}.log"));
            let log = std::fs::File::create(&log_path).expect("child log");
            let mut command = Command::new(&executable);
            command
                .arg("--exact")
                .arg(&child_test)
                .arg("--nocapture")
                .env("KHIVE_6182_CHILD_ROOT", &child_root)
                .env("KHIVE_6182_CHILD_MODE", mode)
                .env("KHIVE_6182_CHILD_STACK_MIB", stack_mib.to_string())
                .stdout(Stdio::from(log.try_clone().expect("clone child log")))
                .stderr(Stdio::from(log));
            if profile {
                command.env("KHIVE_RECALL_PROFILE", "1");
            } else {
                command.env_remove("KHIVE_RECALL_PROFILE");
            }
            let mut child = command.spawn().expect("spawn bounded recall child");
            let deadline = Instant::now() + Duration::from_secs(90);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        let output = read_bounded_log(&log_path);
                        panic!("{arm} recall child timed out and was killed:\n{output}");
                    }
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        let output = read_bounded_log(&log_path);
                        panic!("{arm} recall child wait failed: {error}\n{output}");
                    }
                }
            };
            let output = read_bounded_log(&log_path);
            assert!(
                status.success(),
                "{arm} recall child failed: {status}\n{output}"
            );
            for marker in [
                "KHIVE_6182_SETUP_OK",
                "KHIVE_6182_DISPATCH_STARTED",
                "KHIVE_6182_FIRST_RESULT_OK",
                "KHIVE_6182_RESULT_OK",
                "KHIVE_6182_CLEANUP_OK",
            ] {
                assert!(output.contains(marker), "{arm} missing {marker}:\n{output}");
            }
        }
    }
}

#[test]
fn recall_two_mib_worker_child() {
    use crate::test_support::HashVecProvider;
    use khive_pack_kg::KgPack;
    use khive_runtime::RuntimeConfig;
    use std::time::Duration;

    let Some(root) = std::env::var_os("KHIVE_6182_CHILD_ROOT") else {
        return;
    };
    let mode = std::env::var("KHIVE_6182_CHILD_MODE").expect("child mode");
    assert_eq!(
        std::env::var_os("KHIVE_RECALL_PROFILE").is_some(),
        mode == "profiled"
    );
    let stack_mib: usize = std::env::var("KHIVE_6182_CHILD_STACK_MIB")
        .expect("child stack size")
        .parse()
        .expect("numeric child stack size");
    assert!([2, 16].contains(&stack_mib));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(stack_mib * 1024 * 1024)
        .enable_all()
        .build()
        .expect("create controlled Tokio runtime");
    runtime.block_on(async move {
        const MODEL: &str = "stack-boundary-test-model";
        const NOTE: &str = "stack boundary synthetic recall note 7";
        let db_path = std::path::PathBuf::from(root).join("scratch.db");
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("create synthetic runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: 32,
        });
        let token = rt
            .authorize(Namespace::local())
            .expect("authorize synthetic namespace");
        for i in 0..32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("stack boundary synthetic recall note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("seed synthetic note");
        }
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let pack = MemoryPack::new(crate::test_support::with_receipt_credentials(rt.clone()));
        let ann = pack.ann_for_test();
        builder.register(pack);
        let registry = builder.build().expect("synthetic registry");
        eprintln!("KHIVE_6182_SETUP_OK mode={mode}");

        fn assert_expected_hit(result: &serde_json::Value) {
            let hits = result
                .as_array()
                .or_else(|| result.get("results").and_then(|rows| rows.as_array()))
                .expect("recall response has result rows");
            assert!(
                hits.iter().any(|hit| hit["content"].as_str() == Some(NOTE)),
                "expected synthetic note in recall result: {result:?}"
            );
            assert!(result.get("degraded").is_none());
            assert!(hits.iter().all(|hit| hit.get("degraded").is_none()));
        }

        let dispatch = tokio::spawn(async move {
            eprintln!("KHIVE_6182_DISPATCH_STARTED");
            let params = serde_json::json!({ "query": NOTE, "limit": 10 });
            let first = registry
                .dispatch("memory.recall", params.clone())
                .await
                .expect("first recall failed");
            assert_expected_hit(&first);
            eprintln!("KHIVE_6182_FIRST_RESULT_OK");
            ann.reset_warm_route_count();
            let second = registry
                .dispatch("memory.recall", params)
                .await
                .expect("second recall failed");
            assert_expected_hit(&second);
            assert!(
                ann.warm_route_count() > 0,
                "second recall must take the warm ANN route"
            );
        });
        dispatch.await.expect("dispatch task panicked");
        eprintln!("KHIVE_6182_RESULT_OK");

        let writer_join = rt.backend().pool().take_writer_task_join();
        drop(token);
        drop(rt);
        if let Some(join) = writer_join {
            tokio::time::timeout(Duration::from_secs(5), join)
                .await
                .expect("synthetic writer cleanup timed out")
                .expect("synthetic writer cleanup failed");
        }
        eprintln!("KHIVE_6182_CLEANUP_OK");
    });
}
