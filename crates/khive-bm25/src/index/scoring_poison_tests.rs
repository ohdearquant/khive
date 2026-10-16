use super::IdfCache;
use std::sync::atomic::Ordering;

#[test]
fn idf_cache_clone_recovers_data_from_poisoned_lock() {
    let cache = IdfCache::default();
    cache.cached_doc_count.store(10, Ordering::Relaxed);

    // Populate the cache before poisoning.
    {
        let mut guard = cache.by_df.write().unwrap();
        guard.insert(1, 2.5);
        guard.insert(3, 1.1);
    }

    // Poison the lock by panicking inside a write-lock scope.
    let _ = std::panic::catch_unwind(|| {
        let _guard = cache.by_df.write().unwrap();
        panic!("intentional poison");
    });

    // The lock is now poisoned — verify that.
    assert!(cache.by_df.read().is_err(), "lock must be poisoned");

    // Clone must recover the populated data, not produce an empty map.
    let cloned = cache.clone();

    let guard = cloned.by_df.read().unwrap_or_else(|p| p.into_inner());
    assert_eq!(
        guard.len(),
        2,
        "cloned IdfCache must preserve all entries from a poisoned lock"
    );
    assert!(
        (*guard.get(&1).unwrap() - 2.5).abs() < f64::EPSILON,
        "IDF value for df=1 must survive poison-clone"
    );
    assert_eq!(
        cloned.cached_doc_count.load(Ordering::Relaxed),
        10,
        "doc_count must be preserved across poison-clone"
    );
}
