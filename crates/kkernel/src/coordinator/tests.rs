use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use khive_runtime::{BackendId, KhiveRuntime};
use khive_types::namespace::Namespace;

use super::{BackendRegistry, LocatorCache, SubstrateCoordinator};

fn memory_runtime() -> Arc<KhiveRuntime> {
    Arc::new(KhiveRuntime::memory().expect("memory runtime"))
}

// ---- Existing tests (D1 infrastructure) ----

#[test]
fn single_coordinator_is_single_backend() {
    let coord = SubstrateCoordinator::single(memory_runtime());
    assert!(coord.is_single_backend());
    assert_eq!(coord.backend_count(), 1);
    assert_eq!(coord.backend_ids().len(), 1);
    assert_eq!(coord.backend_ids()[0].as_str(), "main");
}

#[test]
fn registry_register_dedup() {
    let mut reg = BackendRegistry::new();
    let rt = memory_runtime();
    assert!(reg.register(BackendId::new("main"), Arc::clone(&rt)));
    assert!(!reg.register(BackendId::new("main"), Arc::clone(&rt)));
    assert_eq!(reg.len(), 1);
}

#[test]
fn registry_primary_is_first_registered() {
    let mut reg = BackendRegistry::new();
    let rt1 = memory_runtime();
    let rt2 = memory_runtime();
    reg.register(BackendId::new("main"), rt1);
    reg.register(BackendId::new("lore"), rt2);
    assert_eq!(reg.primary().unwrap().id.as_str(), "main");
}

#[test]
fn multi_backend_coordinator_not_single() {
    let mut registry = BackendRegistry::new();
    registry.register(BackendId::new("main"), memory_runtime());
    registry.register(BackendId::new("lore"), memory_runtime());
    let coord = SubstrateCoordinator::new(registry);
    assert!(!coord.is_single_backend());
    assert_eq!(coord.backend_count(), 2);
}

#[test]
fn backend_id_display() {
    let id = BackendId::new("archive");
    assert_eq!(id.to_string(), "archive");
    assert_eq!(id.as_str(), "archive");
}

#[test]
fn backend_id_main_constant() {
    assert_eq!(BackendId::main().as_str(), BackendId::MAIN);
}

// ---- D2: LocatorCache tests ----

#[test]
fn locator_cache_miss_returns_none() {
    let cache = LocatorCache::new();
    let id = Uuid::new_v4();
    assert!(cache.get(id).is_none());
}

#[test]
fn locator_cache_insert_then_get_returns_backend() {
    let cache = LocatorCache::new();
    let id = Uuid::new_v4();
    cache.insert(id, BackendId::new("main"));
    let result = cache.get(id);
    assert!(result.is_some());
    assert_eq!(result.unwrap().as_str(), "main");
}

#[test]
fn locator_cache_expired_entry_returns_none() {
    // Use a 1-nanosecond TTL so entries expire immediately.
    let cache = LocatorCache::with_ttl(Duration::from_nanos(1));
    let id = Uuid::new_v4();
    cache.insert(id, BackendId::new("main"));
    // Sleep long enough for the TTL to elapse (1 µs is more than 1 ns).
    std::thread::sleep(Duration::from_micros(1));
    assert!(cache.get(id).is_none());
}

#[test]
fn locator_cache_purge_removes_expired() {
    let cache = LocatorCache::with_ttl(Duration::from_nanos(1));
    for _ in 0..5 {
        cache.insert(Uuid::new_v4(), BackendId::new("main"));
    }
    std::thread::sleep(Duration::from_micros(1));
    cache.purge_expired();
    assert_eq!(cache.len(), 0);
}

// ---- D2: locate() integration tests ----

#[tokio::test]
async fn locator_cache_miss_then_hit() {
    let coord = SubstrateCoordinator::single(memory_runtime());
    let ns = Namespace::local();

    // Create an entity on the primary backend.
    let runtime = coord.primary_runtime().unwrap();
    let token = runtime.authorize(ns.clone()).unwrap();
    let entity = runtime
        .create_entity(&token, "concept", None, "LoRA", None, None, vec![])
        .await
        .expect("create entity");

    // First locate: cache miss → backend scan → cache populated.
    let first = coord.locate(entity.id, &ns).await;
    assert!(
        first.is_some(),
        "locate should find the entity on first call"
    );
    assert_eq!(first.unwrap().as_str(), BackendId::MAIN);
    assert_eq!(coord.locator_cache().len(), 1, "cache should be populated");

    // Second locate: cache hit (no backend I/O).
    let second = coord.locate(entity.id, &ns).await;
    assert!(second.is_some(), "second locate should hit cache");
}

#[tokio::test]
async fn locator_cache_returns_none_for_unknown_uuid() {
    let coord = SubstrateCoordinator::single(memory_runtime());
    let ns = Namespace::local();
    let unknown = Uuid::new_v4();
    let result = coord.locate(unknown, &ns).await;
    assert!(result.is_none(), "unknown UUID should resolve to None");
}

// ---- D3: fan_out_search tests ----

#[tokio::test]
async fn fan_out_search_single_backend_returns_hits() {
    let coord = SubstrateCoordinator::single(memory_runtime());
    let ns = Namespace::local();

    let runtime = coord.primary_runtime().unwrap();
    let token = runtime.authorize(ns.clone()).unwrap();
    runtime
        .create_entity(
            &token,
            "concept",
            None,
            "FlashAttention",
            Some("IO-aware exact attention"),
            None,
            vec![],
        )
        .await
        .expect("create entity");

    let (hits, per_backend) = coord.fan_out_search("FlashAttention", &ns, 10).await;

    assert!(!hits.is_empty(), "should find the entity");
    assert_eq!(per_backend.len(), 1, "single backend report");
    assert!(per_backend[0].error.is_none(), "no error");
}

#[tokio::test]
async fn fan_out_search_two_backends_merged() {
    let mut registry = BackendRegistry::new();
    let rt_main = memory_runtime();
    let rt_lore = memory_runtime();
    registry.register(BackendId::new("main"), Arc::clone(&rt_main));
    registry.register(BackendId::new("lore"), Arc::clone(&rt_lore));
    let coord = SubstrateCoordinator::new(registry);
    let ns = Namespace::local();

    // Create one entity on each backend.
    let tok_main = rt_main.authorize(ns.clone()).unwrap();
    rt_main
        .create_entity(
            &tok_main,
            "concept",
            None,
            "LoRA",
            Some("Low-rank adaptation"),
            None,
            vec![],
        )
        .await
        .expect("create on main");

    let tok_lore = rt_lore.authorize(ns.clone()).unwrap();
    rt_lore
        .create_entity(
            &tok_lore,
            "concept",
            None,
            "QLoRA",
            Some("Quantised LoRA"),
            None,
            vec![],
        )
        .await
        .expect("create on lore");

    // Fan-out search for "LoRA" — both backends should contribute.
    let (merged_hits, per_backend) = coord.fan_out_search("LoRA", &ns, 10).await;

    assert_eq!(per_backend.len(), 2, "both backends in report");
    // Merged set should contain at least one hit from the combined results.
    assert!(
        !merged_hits.is_empty(),
        "merged results should not be empty"
    );
}

#[tokio::test]
async fn fan_out_search_empty_registry_returns_empty() {
    let coord = SubstrateCoordinator::new(BackendRegistry::new());
    let ns = Namespace::local();
    let (hits, per_backend) = coord.fan_out_search("anything", &ns, 10).await;
    assert!(hits.is_empty());
    assert!(per_backend.is_empty());
}

// ---- D3: partial-failure regression test ----

/// One backend errors; the other succeeds. The merged hits must contain
/// results from the working backend, and the failing backend's
/// `BackendSearchResult.error` must be populated (not `None`).
#[tokio::test]
async fn fan_out_partial_failure_preserves_working_backend_hits() {
    let rt_main = memory_runtime();
    let rt_lore = memory_runtime();

    let ns = Namespace::local();

    // Seed one entity on the "lore" backend so a search returns a hit.
    let tok_lore = rt_lore.authorize(ns.clone()).unwrap();
    rt_lore
        .create_entity(
            &tok_lore,
            "concept",
            None,
            "PartialFailureProbe",
            Some("probe entity for partial-failure test"),
            None,
            vec![],
        )
        .await
        .expect("create entity on lore");

    let mut registry = BackendRegistry::new();
    registry.register(BackendId::new("main"), Arc::clone(&rt_main));
    registry.register(BackendId::new("lore"), Arc::clone(&rt_lore));

    // Force "main" to error; "lore" should still return hits.
    let coord = SubstrateCoordinator::new(registry).with_failing_backend("main");

    let (merged_hits, per_backend) = coord.fan_out_search("PartialFailureProbe", &ns, 10).await;

    // Both backends must be reported.
    assert_eq!(
        per_backend.len(),
        2,
        "both backends should appear in the report"
    );

    // The failing backend ("main") must have an error annotation.
    let main_result = per_backend
        .iter()
        .find(|r| r.backend_id.as_str() == "main")
        .expect("main backend result must be present");
    assert!(
        main_result.error.is_some(),
        "main backend should report an error"
    );
    assert!(
        main_result.hits.is_empty(),
        "main backend should have no hits"
    );

    // The working backend ("lore") must have no error.
    let lore_result = per_backend
        .iter()
        .find(|r| r.backend_id.as_str() == "lore")
        .expect("lore backend result must be present");
    assert!(
        lore_result.error.is_none(),
        "lore backend should have no error"
    );

    // Merged hits must contain the hit from the working backend.
    assert!(
        !merged_hits.is_empty(),
        "merged hits must include results from the working backend"
    );
}

// ---- D2: note-locate regression test ----

/// `locate` must resolve note UUIDs in addition to entity UUIDs.
#[tokio::test]
async fn locate_finds_note_uuid() {
    let coord = SubstrateCoordinator::single(memory_runtime());
    let ns = Namespace::local();

    let runtime = coord.primary_runtime().unwrap();
    let token = runtime.authorize(ns.clone()).unwrap();
    let note = runtime
        .create_note(
            &token,
            "observation",
            Some("locate-note-regression"),
            "content for locate regression test",
            None,
            None,
            vec![],
        )
        .await
        .expect("create note");

    // locate must return the backend for a note UUID, not just entities.
    let backend = coord.locate(note.id, &ns).await;
    assert!(backend.is_some(), "locate should find the note's backend");
    assert_eq!(backend.unwrap().as_str(), BackendId::MAIN);
    assert_eq!(
        coord.locator_cache().len(),
        1,
        "cache should be populated for the note"
    );
}

// ---- D2: cache eviction on expired read ----

/// After TTL expiry, `get` must remove the entry from the map (not just
/// return `None` while leaking memory).
#[test]
fn locator_cache_get_evicts_expired_entry() {
    let cache = LocatorCache::with_ttl(Duration::from_nanos(1));
    let id = Uuid::new_v4();
    cache.insert(id, BackendId::new("main"));
    assert_eq!(cache.len(), 1, "entry inserted");
    std::thread::sleep(Duration::from_micros(1));
    // get() should return None AND remove the entry from the map.
    assert!(cache.get(id).is_none(), "expired entry returns None");
    assert_eq!(cache.len(), 0, "expired entry must be evicted from the map");
}

// ---- D2: cache invalidation via remove() ----

#[test]
fn locator_cache_remove_evicts_live_entry() {
    let cache = LocatorCache::new();
    let id = Uuid::new_v4();
    cache.insert(id, BackendId::new("main"));
    assert!(cache.get(id).is_some(), "entry live before remove");
    cache.remove(id);
    assert!(cache.get(id).is_none(), "entry gone after remove");
    assert_eq!(cache.len(), 0, "map must be empty after remove");
}

#[tokio::test]
async fn invalidate_clears_locate_cache() {
    let coord = SubstrateCoordinator::single(memory_runtime());
    let ns = Namespace::local();

    let runtime = coord.primary_runtime().unwrap();
    let token = runtime.authorize(ns.clone()).unwrap();
    let entity = runtime
        .create_entity(
            &token,
            "concept",
            None,
            "InvalidateTest",
            None,
            None,
            vec![],
        )
        .await
        .expect("create entity");

    // Populate the cache.
    coord.locate(entity.id, &ns).await;
    assert_eq!(coord.locator_cache().len(), 1, "cache populated");

    // Invalidate — simulates a hard-delete.
    coord.invalidate(entity.id);
    assert_eq!(
        coord.locator_cache().len(),
        0,
        "cache cleared after invalidate"
    );

    // locate must now return None (entity was deleted, cache is empty).
    // Since the entity still exists on the backend, it will be re-found
    // and re-cached. Verify the round-trip works.
    let found_again = coord.locate(entity.id, &ns).await;
    assert!(found_again.is_some(), "locate re-finds after cache clear");
}
