//! Public extraction controls for bounded text buffers and lossy UTF-8.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_web::WebPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{BlobStore, ContentRef, Entity};
use serde_json::json;
use uuid::Uuid;

thread_local! {
    static LARGEST_ALLOCATION: Cell<Option<usize>> = const { Cell::new(None) };
}

struct ObservedAllocator;

fn record_allocation(size: usize) {
    // Const-initialized TLS + Cell access allocate nothing. Other test threads
    // and the filesystem's blocking workers have observation disabled.
    let _ = LARGEST_ALLOCATION.try_with(|largest| {
        if let Some(previous) = largest.get() {
            largest.set(Some(previous.max(size)));
        }
    });
}

// SAFETY: every request is forwarded unchanged to System; only its size is
// observed, without dereferencing pointers or allocating in the observer.
unsafe impl GlobalAlloc for ObservedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: ObservedAllocator = ObservedAllocator;

struct AllocationObservation;

impl AllocationObservation {
    fn start() -> Self {
        LARGEST_ALLOCATION.with(|largest| largest.set(Some(0)));
        Self
    }

    fn finish(self) -> usize {
        LARGEST_ALLOCATION.with(|largest| largest.replace(None).unwrap())
    }
}

impl Drop for AllocationObservation {
    fn drop(&mut self) {
        LARGEST_ALLOCATION.with(|largest| largest.set(None));
    }
}

fn fixture() -> (
    KhiveRuntime,
    VerbRegistry,
    Arc<dyn BlobStore>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn BlobStore> =
        Arc::new(khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0).unwrap());
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_blob_store(store.clone()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry, store, dir)
}

async fn seed_page(runtime: &KhiveRuntime, store: &dyn BlobStore, body: Vec<u8>) -> Uuid {
    let content_ref = store.put(body).await.unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut entity =
        Entity::new("local", "document", "extraction fixture").with_entity_type(Some("page"));
    entity.properties = Some(json!({
        "url": "https://extract.example.test/page",
        "blob_ref": content_ref.to_string(),
        "content_type": "text/html",
    }));
    let id = entity.id;
    assert!(runtime
        .entities(&token)
        .unwrap()
        .insert_entity_if_absent(entity)
        .await
        .unwrap());
    id
}

#[tokio::test]
async fn extract_text_preserves_lossy_decoding_and_fixed_byte_boundary() {
    let mut invalid = b"<p>".to_vec();
    invalid.extend(std::iter::repeat_n(0xff, 100_000));
    invalid.extend_from_slice(b"</p>");
    let multibyte = format!("<p>x{}</p>", "é".repeat(150_000)).into_bytes();
    let script = format!(
        "<p>before</p><script>{}</script><style>hidden</style><p>after</p>",
        "not prose".repeat(100_000),
    )
    .into_bytes();
    for (body, expected) in [
        (invalid, "\u{fffd}".repeat(66_666)),
        (multibyte, format!("x{}", "é".repeat(99_999))),
        (script, "before after".to_string()),
    ] {
        let (runtime, registry, store, _dir) = fixture();
        let id = seed_page(&runtime, store.as_ref(), body).await;
        let response = registry
            .dispatch("web.extract", json!({"id": id, "kinds": ["text"]}))
            .await
            .unwrap();
        let text_id = response["result"]["text"]["id"].as_str().unwrap();
        let entity = registry
            .dispatch("get", json!({"id": text_id}))
            .await
            .unwrap();
        let content_ref =
            ContentRef::from_hex(entity["properties"]["blob_ref"].as_str().unwrap()).unwrap();
        let bytes = store
            .get_bounded_verified(&content_ref, 200_000)
            .await
            .unwrap();
        assert_eq!(bytes, expected.as_bytes());
        assert_eq!(entity["properties"]["size"], expected.len());
        assert!(bytes.len() <= 200_000);
        assert!(std::str::from_utf8(&bytes).is_ok());
    }
}

#[tokio::test]
async fn extract_text_keeps_unclosed_tag_text_across_removed_script_spans() {
    let (runtime, registry, store, _dir) = fixture();
    // Must fail if the bounded implementation drops unclosed tags, stops at a
    // raw-byte prefix before folding whitespace, or loses state at script spans.
    let body = format!(
        "<p title='<script>secret</script>'>visible</p><{}tail",
        " ".repeat(400_000),
    )
    .into_bytes();
    let id = seed_page(&runtime, store.as_ref(), body).await;
    let response = registry
        .dispatch("web.extract", json!({"id": id, "kinds": ["text"]}))
        .await
        .unwrap();
    let entity = registry
        .dispatch(
            "get",
            json!({
                "id": response["result"]["text"]["id"],
            }),
        )
        .await
        .unwrap();
    let content_ref =
        ContentRef::from_hex(entity["properties"]["blob_ref"].as_str().unwrap()).unwrap();
    let bytes = store
        .get_bounded_verified(&content_ref, 200_000)
        .await
        .unwrap();
    assert_eq!(bytes, b"visible < tail");
}

#[tokio::test]
async fn extract_refuses_foreign_source_before_parsing_its_blob_reference() {
    let (runtime, registry, _store, _dir) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut entity =
        Entity::new("private-owner", "document", "private-name").with_entity_type(Some("page"));
    entity.properties = Some(json!({
        "url": "https://private.example.test/secret",
        "blob_ref": "deliberately-invalid-reference",
        "content_type": "text/html",
    }));
    let id = entity.id;
    assert!(runtime
        .entities(&token)
        .unwrap()
        .insert_entity_if_absent(entity)
        .await
        .unwrap());
    // Must fail when the source-ownership guard is removed: parsing the foreign
    // blob_ref would then produce an Internal error instead of this NotFound.
    let error = registry
        .dispatch("web.extract", json!({"id": id, "kinds": ["text"]}))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, RuntimeError::Khive(error)
        if error.kind() == khive_types::ErrorKind::NotFound),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains(&id.to_string()));
    for private in [
        "private-owner",
        "private-name",
        "private.example.test",
        "deliberately-invalid",
    ] {
        assert!(!message.contains(private), "{message}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn extract_text_never_allocates_an_intermediate_full_body_copy() {
    let (runtime, registry, store, _dir) = fixture();
    let warm = seed_page(
        &runtime,
        store.as_ref(),
        b"<p>warm regex and runtime paths</p>".to_vec(),
    )
    .await;
    registry
        .dispatch("web.extract", json!({"id": warm, "kinds": ["text"]}))
        .await
        .unwrap();
    let body = format!("<p>{}</p>", "x".repeat(4 * 1024 * 1024)).into_bytes();
    let id = seed_page(&runtime, store.as_ref(), body).await;

    let observation = AllocationObservation::start();
    let response = registry
        .dispatch("web.extract", json!({"id": id, "kinds": ["text"]}))
        .await;
    let largest = observation.finish();
    response.unwrap();
    // Must fail with the old tag replace_all/full collapsed String restored:
    // each requests >=4 MiB on this thread before truncating to 200,000 bytes.
    // The raw source allocation is separately admitted and performed by the FS
    // blocking worker; this control measures derived work on the handler thread.
    assert!(
        largest <= 1024 * 1024,
        "derived handler allocation was {largest} bytes"
    );
}
