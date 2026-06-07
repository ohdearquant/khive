//! Bounded query-embedding LRU cache local to khive-pack-memory.
//!
//! Keyed by `(model_name, query_text) -> Arc<[f32]>`.  Cache hits bypass
//! `spawn_blocking` and return in microseconds.  No invalidation is needed:
//! query embedding is pure for a fixed model and text.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lru::LruCache;
use parking_lot::Mutex;

pub(crate) const DEFAULT_QUERY_CACHE_CAPACITY: usize = 512;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct QueryEmbeddingCacheKey {
    model_name: Arc<str>,
    query_text: Arc<str>,
}

impl QueryEmbeddingCacheKey {
    fn new(model_name: &str, query_text: &str) -> Self {
        Self {
            model_name: Arc::from(model_name),
            query_text: Arc::from(query_text),
        }
    }
}

/// Thread-safe LRU cache for query embeddings.
///
/// [`Clone`] gives a cheap handle to the same underlying cache (Arc-wrapped
/// interior).  All clones share the same capacity and eviction state.
#[derive(Clone)]
pub(crate) struct QueryEmbeddingCache {
    inner: Arc<Mutex<LruCache<QueryEmbeddingCacheKey, Arc<[f32]>>>>,
}

impl QueryEmbeddingCache {
    pub(crate) fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(capacity))),
        }
    }

    pub(crate) fn with_default_capacity() -> Self {
        Self::new(
            NonZeroUsize::new(DEFAULT_QUERY_CACHE_CAPACITY)
                .expect("DEFAULT_QUERY_CACHE_CAPACITY > 0"),
        )
    }

    /// Return a cached embedding vector or `None` on miss.
    pub(crate) fn get(&self, model_name: &str, query_text: &str) -> Option<Vec<f32>> {
        let key = QueryEmbeddingCacheKey::new(model_name, query_text);
        self.inner.lock().get(&key).map(|arc| arc.to_vec())
    }

    /// Store a successful embedding.  Overwrites an existing entry (LRU
    /// promotes it to most-recently-used position).
    pub(crate) fn put(&self, model_name: &str, query_text: &str, embedding: Vec<f32>) {
        let key = QueryEmbeddingCacheKey::new(model_name, query_text);
        self.inner.lock().put(key, embedding.into());
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_cache_exact_match_hits() {
        let cache = QueryEmbeddingCache::with_default_capacity();
        assert!(cache.get("m1", "hello world").is_none());

        let v = vec![1.0_f32, 2.0, 3.0];
        cache.put("m1", "hello world", v.clone());

        let hit = cache.get("m1", "hello world").expect("cache hit expected");
        assert_eq!(hit, v);
    }

    #[test]
    fn query_cache_model_names_do_not_collide() {
        let cache = QueryEmbeddingCache::with_default_capacity();
        let va = vec![1.0_f32];
        let vb = vec![2.0_f32];
        cache.put("model-a", "same query", va.clone());
        cache.put("model-b", "same query", vb.clone());

        assert_eq!(cache.get("model-a", "same query").unwrap(), va);
        assert_eq!(cache.get("model-b", "same query").unwrap(), vb);
    }

    #[test]
    fn query_cache_evicts_at_capacity() {
        let cap = NonZeroUsize::new(2).unwrap();
        let cache = QueryEmbeddingCache::new(cap);

        cache.put("m", "q1", vec![1.0]);
        cache.put("m", "q2", vec![2.0]);
        assert_eq!(cache.len(), 2);

        // Inserting a third entry evicts the least-recently-used ("q1").
        cache.put("m", "q3", vec![3.0]);
        assert_eq!(cache.len(), 2);
        assert!(
            cache.get("m", "q1").is_none(),
            "q1 should have been evicted"
        );
        assert!(cache.get("m", "q2").is_some());
        assert!(cache.get("m", "q3").is_some());
    }
}
