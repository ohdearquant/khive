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
