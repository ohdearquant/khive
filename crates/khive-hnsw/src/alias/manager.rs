//! Index alias manager: atomic blue-green swap for zero-downtime HNSW index migration.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use super::drain::{drain_readers, ReaderCounter, ReaderGuard};
use super::error::AliasError;
use super::validation::IndexValidator;
use crate::config::HnswConfig;
use crate::HnswIndex;
use crate::NodeId;

/// Metadata for a registered collection.
struct Collection {
    /// The index, wrapped in Arc for snapshot sharing with readers.
    index: Arc<HnswIndex>,
    /// Active reader counter for drain detection.
    readers: Arc<ReaderCounter>,
}

/// Report from a completed migration.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    /// Name of the old collection that was replaced.
    pub old_collection: String,
    /// Name of the new collection.
    pub new_collection: String,
    /// Number of vectors in the old index.
    pub old_size: usize,
    /// Number of vectors in the new index.
    pub new_size: usize,
    /// Recall score from validation (if validation was run).
    pub recall_score: Option<f32>,
    /// Wall-clock time for the entire migration (build + validate + swap + drain).
    pub total_duration: Duration,
    /// Wall-clock time for the index build phase.
    pub build_duration: Duration,
    /// Wall-clock time for the swap + drain phase.
    pub swap_drain_duration: Duration,
}

/// Manages named collections and aliases for zero-downtime HNSW index switching.
///
/// Methods that need both maps always acquire `aliases` before `collections`.
pub struct IndexAliasManager {
    /// Collection name -> Collection data.
    /// Protected by RwLock: reads (search) take shared lock, writes (register/remove)
    /// take exclusive lock.
    collections: RwLock<HashMap<String, Collection>>,

    /// Alias name -> collection name mapping.
    /// Protected by RwLock: reads (resolve alias) take shared lock, writes
    /// (create/switch alias) take exclusive lock.
    aliases: RwLock<HashMap<String, String>>,

    /// Maximum time to wait for readers to drain before force-dropping.
    drain_timeout: Duration,

    /// Poll interval for drain detection.
    drain_poll_interval: Duration,
}

impl IndexAliasManager {
    /// Create a new alias manager with the given drain timeout.
    pub fn new(drain_timeout: Duration) -> Self {
        Self {
            collections: RwLock::new(HashMap::new()),
            aliases: RwLock::new(HashMap::new()),
            drain_timeout,
            drain_poll_interval: Duration::from_millis(10),
        }
    }

    /// Set the drain poll interval (default: 10ms).
    pub fn with_drain_poll_interval(mut self, interval: Duration) -> Self {
        self.drain_poll_interval = interval;
        self
    }

    /// Register a named collection. Fails if the name already exists.
    pub fn register_collection(&self, name: &str, index: HnswIndex) -> Result<(), AliasError> {
        let mut collections = self.collections.write();
        if collections.contains_key(name) {
            return Err(AliasError::CollectionAlreadyExists(name.to_string()));
        }
        collections.insert(
            name.to_string(),
            Collection {
                index: Arc::new(index),
                readers: Arc::new(ReaderCounter::new()),
            },
        );
        Ok(())
    }

    /// Create an alias pointing to an existing collection.
    /// Fails if the alias already exists or the collection does not exist.
    pub fn create_alias(&self, alias: &str, collection: &str) -> Result<(), AliasError> {
        // Keep the canonical aliases -> collections order and hold both
        // through insertion so retirement cannot remove the target between
        // its existence check and publishing the alias.
        let mut aliases = self.aliases.write();
        let collections = self.collections.read();
        if !collections.contains_key(collection) {
            return Err(AliasError::CollectionNotFound(collection.to_string()));
        }

        if aliases.contains_key(alias) {
            return Err(AliasError::AliasAlreadyExists(alias.to_string()));
        }
        aliases.insert(alias.to_string(), collection.to_string());
        Ok(())
    }

    /// Acquire a reader guard; holds an `Arc<HnswIndex>` snapshot that stays alive until dropped.
    ///
    /// Holds the `aliases` read lock for the entire resolve -> lookup ->
    /// count sequence (not just alias resolution). `switch_alias` needs the
    /// `aliases` write lock, so it cannot swap the alias -- and therefore
    /// cannot make a concurrent `drain_and_remove` observe a zero reader
    /// count -- until this reader has been counted or has failed outright
    /// (#417).
    pub fn acquire_reader(&self, alias: &str) -> Result<ReaderGuard, AliasError> {
        let aliases = self.aliases.read();
        let collection_name = aliases
            .get(alias)
            .ok_or_else(|| AliasError::AliasNotFound(alias.to_string()))?;

        let collections = self.collections.read();
        let collection = collections
            .get(collection_name)
            .ok_or_else(|| AliasError::CollectionNotFound(collection_name.clone()))?;

        let guard = ReaderGuard::new(
            Arc::clone(&collection.index),
            Arc::clone(&collection.readers),
        );
        drop(collections);
        drop(aliases);
        Ok(guard)
    }

    /// Test-only variant of `acquire_reader` that runs `hook` right after
    /// alias resolution while still holding the `aliases` read lock, used
    /// to deterministically exercise the switch/drain race window (#417).
    #[cfg(test)]
    pub(crate) fn acquire_reader_with_test_hook(
        &self,
        alias: &str,
        hook: impl FnOnce(),
    ) -> Result<ReaderGuard, AliasError> {
        let aliases = self.aliases.read();
        let collection_name = aliases
            .get(alias)
            .ok_or_else(|| AliasError::AliasNotFound(alias.to_string()))?;

        hook();

        let collections = self.collections.read();
        let collection = collections
            .get(collection_name)
            .ok_or_else(|| AliasError::CollectionNotFound(collection_name.clone()))?;

        let guard = ReaderGuard::new(
            Arc::clone(&collection.index),
            Arc::clone(&collection.readers),
        );
        drop(collections);
        drop(aliases);
        Ok(guard)
    }

    /// Switch an alias to a different collection, optionally validating first.
    /// Returns the previous collection name for drain purposes.
    pub fn switch_alias(
        &self,
        alias: &str,
        new_collection: &str,
        validator: Option<&dyn IndexValidator>,
    ) -> Result<String, AliasError> {
        self.switch_alias_inner(alias, new_collection, validator, || {})
    }

    fn switch_alias_inner(
        &self,
        alias: &str,
        new_collection: &str,
        validator: Option<&dyn IndexValidator>,
        before_publish: impl FnOnce(),
    ) -> Result<String, AliasError> {
        // Validate before taking the aliases write lock so readers and
        // unrelated alias operations can continue during validation.
        let validated_index = if let Some(v) = validator {
            let collections = self.collections.read();
            let collection = collections
                .get(new_collection)
                .ok_or_else(|| AliasError::CollectionNotFound(new_collection.to_string()))?;
            let index = Arc::clone(&collection.index);
            v.validate(&index)?;
            drop(collections);
            Some(index)
        } else {
            None
        };

        before_publish();

        // Keep the canonical aliases -> collections order and hold both
        // through the swap so retirement cannot remove the target between
        // its existence check and publishing the alias.
        let mut aliases = self.aliases.write();
        let collections = self.collections.read();
        let collection = collections
            .get(new_collection)
            .ok_or_else(|| AliasError::CollectionNotFound(new_collection.to_string()))?;
        if validated_index
            .as_ref()
            .is_some_and(|validated| !Arc::ptr_eq(validated, &collection.index))
        {
            // The validated target disappeared even though its name was reused.
            return Err(AliasError::CollectionNotFound(new_collection.to_string()));
        }

        let old_collection = aliases
            .get(alias)
            .ok_or_else(|| AliasError::AliasNotFound(alias.to_string()))?
            .clone();

        aliases.insert(alias.to_string(), new_collection.to_string());
        Ok(old_collection)
    }

    /// Test-only variant that pauses after validation and before publishing
    /// the alias, used to exercise replacement of the validated collection.
    #[cfg(test)]
    fn switch_alias_with_test_hook(
        &self,
        alias: &str,
        new_collection: &str,
        validator: Option<&dyn IndexValidator>,
        hook: impl FnOnce(),
    ) -> Result<String, AliasError> {
        self.switch_alias_inner(alias, new_collection, validator, hook)
    }

    fn retire_collection(
        &self,
        collection: &str,
        expected_index: Option<&Arc<HnswIndex>>,
    ) -> Result<Arc<ReaderCounter>, AliasError> {
        // Keep the canonical aliases -> collections order and hold both
        // through removal so no alias can begin referencing the target
        // after the check succeeds.
        let aliases = self.aliases.read();
        let mut collections = self.collections.write();
        let registered = collections
            .get(collection)
            .ok_or_else(|| AliasError::CollectionNotFound(collection.to_string()))?;
        if expected_index.is_some_and(|expected| !Arc::ptr_eq(expected, &registered.index)) {
            // Do not retire a replacement that merely reused the expected name.
            return Err(AliasError::CollectionNotFound(collection.to_string()));
        }
        if aliases.values().any(|target| target == collection) {
            return Err(AliasError::CollectionInUse(collection.to_string()));
        }

        let coll = collections
            .remove(collection)
            .expect("collection existence checked while holding the write lock");
        let counter = Arc::clone(&coll.readers);
        drop(collections);
        drop(aliases);
        Ok(counter)
    }

    /// Retire a collection from manager ownership and wait for its readers to drain.
    ///
    /// Returns [`AliasError::CollectionInUse`] without changing either map
    /// when any alias still references the collection.
    ///
    /// The collection is removed from `self.collections` *before* waiting,
    /// not after: outstanding `ReaderGuard`s hold their own `Arc<HnswIndex>`
    /// clone, so removing the manager's entry does not affect them. This
    /// means that even if drain times out below, the manager no longer
    /// owns the retired collection forever -- its memory is reclaimed
    /// normally once the last guard drops (#418).
    pub async fn drain_and_remove(&self, collection: &str) -> Result<(), AliasError> {
        let counter = self.retire_collection(collection, None)?;

        // Wait for readers to drain (async, no locks held). A timeout here
        // no longer leaves the collection manager-owned -- it was already
        // retired above.
        drain_readers(&counter, self.drain_timeout, self.drain_poll_interval).await
    }

    /// Build new index, validate, swap alias atomically, drain old; returns `MigrationReport`.
    pub async fn migrate(
        &self,
        alias: &str,
        vectors: Vec<(NodeId, Vec<f32>)>,
        new_config: HnswConfig,
        validator: Option<Box<dyn IndexValidator>>,
    ) -> Result<MigrationReport, AliasError> {
        let total_start = Instant::now();

        // Resolve current alias to get old collection info
        let old_collection_name = {
            let aliases = self.aliases.read();
            aliases
                .get(alias)
                .ok_or_else(|| AliasError::AliasNotFound(alias.to_string()))?
                .clone()
        };

        let old_size = {
            let collections = self.collections.read();
            collections
                .get(&old_collection_name)
                .map(|c| c.index.len_live())
                .unwrap_or(0)
        };

        // Generate a unique name for the new collection
        let new_collection_name = format!(
            "{}_migrated_{}",
            old_collection_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );

        // Build new index on a blocking thread
        let build_start = Instant::now();
        let new_index = tokio::task::spawn_blocking(move || {
            let mut index = HnswIndex::with_config(new_config);
            for (id, vec) in vectors {
                if let Err(e) = index.insert(id, vec) {
                    return Err(AliasError::IndexError(e.to_string()));
                }
            }
            Ok(index)
        })
        .await
        .map_err(|e| AliasError::IndexError(format!("build task panicked: {e}")))?
        .map_err(|e| AliasError::IndexError(format!("build failed: {e}")))?;

        let build_duration = build_start.elapsed();
        let new_size = new_index.len_live();

        // Register the new collection
        self.register_collection(&new_collection_name, new_index)?;

        // Validate and swap
        let swap_drain_start = Instant::now();

        // Recall score for the report
        let recall_score = None;

        // If we have a validator, run it and capture recall
        if let Some(ref v) = validator {
            let collections = self.collections.read();
            let index = Arc::clone(&collections.get(&new_collection_name).unwrap().index);
            let validation_result = v.validate(&index);
            drop(collections);
            match validation_result {
                Ok(()) => {}
                Err(AliasError::ValidationFailed {
                    recall, min_recall, ..
                }) => {
                    // Preserve the validation error even if another alias now
                    // owns the candidate or its name refers to a replacement.
                    let _ = self.retire_collection(&new_collection_name, Some(&index));
                    return Err(AliasError::ValidationFailed {
                        reason: "recall below threshold".to_string(),
                        recall,
                        min_recall,
                    });
                }
                Err(e) => {
                    let _ = self.retire_collection(&new_collection_name, Some(&index));
                    return Err(e);
                }
            }
        }

        // Switch the alias
        self.switch_alias(alias, &new_collection_name, None)?;

        // Retire and drain the old collection. `drain_and_remove` removes it
        // from `self.collections` up front, so even if drain times out below
        // the manager no longer owns it -- outstanding reader guards keep
        // their own Arc<HnswIndex> clone alive and the memory is reclaimed
        // normally once the last guard drops (#418).
        let drain_result = self.drain_and_remove(&old_collection_name).await;

        let swap_drain_duration = swap_drain_start.elapsed();
        let total_duration = total_start.elapsed();

        match drain_result {
            Ok(()) => {}
            Err(AliasError::DrainTimeout { .. }) => {
                // The old collection is already retired from manager ownership;
                // report the completed migration without waiting longer.
            }
            Err(AliasError::CollectionInUse(_)) => {
                // Another alias still needs the old collection, so leaving it
                // registered is the successful migration outcome for this alias.
            }
            Err(error) => return Err(error),
        }

        Ok(MigrationReport {
            old_collection: old_collection_name,
            new_collection: new_collection_name,
            old_size,
            new_size,
            recall_score,
            total_duration,
            build_duration,
            swap_drain_duration,
        })
    }

    /// Get the number of registered collections.
    pub fn collection_count(&self) -> usize {
        self.collections.read().len()
    }

    /// Get the number of registered aliases.
    pub fn alias_count(&self) -> usize {
        self.aliases.read().len()
    }

    /// Get the collection name that an alias points to.
    pub fn resolve_alias(&self, alias: &str) -> Option<String> {
        self.aliases.read().get(alias).cloned()
    }

    /// Get the active reader count for a collection.
    pub fn reader_count(&self, collection: &str) -> Option<u64> {
        self.collections
            .read()
            .get(collection)
            .map(|c| c.readers.load())
    }
}

impl std::fmt::Debug for IndexAliasManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Canonical lock order is aliases -> collections, matching
        // `acquire_reader`; do not flip this or a concurrent
        // migrate/switch_alias can form a lock cycle with this fmt (#417).
        let aliases = self.aliases.read();
        let collections = self.collections.read();
        f.debug_struct("IndexAliasManager")
            .field("collections", &collections.keys().collect::<Vec<_>>())
            .field("aliases", &*aliases)
            .field("drain_timeout", &self.drain_timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HnswIndex;

    fn make_index(dims: usize, count: usize) -> HnswIndex {
        let mut index = HnswIndex::new(dims);
        for i in 0..count {
            let id = NodeId::new([(i & 0xFF) as u8; 16]);
            let vec = vec![i as f32; dims];
            index.insert(id, vec).unwrap();
        }
        index
    }

    #[test]
    fn test_register_and_create_alias() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));

        let index = make_index(4, 5);
        mgr.register_collection("v1", index).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        assert_eq!(mgr.collection_count(), 1);
        assert_eq!(mgr.alias_count(), 1);
        assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
    }

    #[test]
    fn test_register_duplicate_collection() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        mgr.register_collection("v1", make_index(4, 1)).unwrap();

        let result = mgr.register_collection("v1", make_index(4, 1));
        assert!(matches!(
            result,
            Err(AliasError::CollectionAlreadyExists(_))
        ));
    }

    #[test]
    fn test_create_alias_missing_collection() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        let result = mgr.create_alias("active", "nonexistent");
        assert!(matches!(result, Err(AliasError::CollectionNotFound(_))));
    }

    #[test]
    fn test_create_duplicate_alias() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        mgr.register_collection("v1", make_index(4, 1)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        let result = mgr.create_alias("active", "v1");
        assert!(matches!(result, Err(AliasError::AliasAlreadyExists(_))));
    }

    #[test]
    fn test_acquire_reader() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        let guard = mgr.acquire_reader("active").unwrap();
        assert_eq!(guard.len(), 5);
        assert_eq!(mgr.reader_count("v1"), Some(1));

        drop(guard);
        assert_eq!(mgr.reader_count("v1"), Some(0));
    }

    #[test]
    fn test_acquire_reader_missing_alias() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        let result = mgr.acquire_reader("nonexistent");
        assert!(matches!(result, Err(AliasError::AliasNotFound(_))));
    }

    #[test]
    fn test_switch_alias() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.register_collection("v2", make_index(4, 10)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        let old = mgr.switch_alias("active", "v2", None).unwrap();
        assert_eq!(old, "v1");
        assert_eq!(mgr.resolve_alias("active"), Some("v2".to_string()));

        // Reader should now get v2
        let guard = mgr.acquire_reader("active").unwrap();
        assert_eq!(guard.len(), 10);
    }

    #[test]
    fn test_switch_alias_with_failing_validator() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.register_collection("v2", make_index(4, 10)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        // Validator that always fails
        struct FailValidator;
        impl IndexValidator for FailValidator {
            fn validate(&self, _: &HnswIndex) -> Result<(), AliasError> {
                Err(AliasError::ValidationFailed {
                    reason: "test failure".to_string(),
                    recall: Some(0.5),
                    min_recall: Some(0.95),
                })
            }
        }

        let result = mgr.switch_alias("active", "v2", Some(&FailValidator));
        assert!(matches!(result, Err(AliasError::ValidationFailed { .. })));

        // Alias should still point to v1
        assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
    }

    /// Regression for #1438: a validated index cannot be retired and replaced
    /// under the same name before the alias is published.
    #[tokio::test]
    async fn test_switch_alias_rejects_replacement_after_validation() {
        struct PassValidator;

        impl IndexValidator for PassValidator {
            fn validate(&self, _index: &HnswIndex) -> Result<(), AliasError> {
                Ok(())
            }
        }

        let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(1)));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.register_collection("v2", make_index(4, 10)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        let (validated_tx, validated_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let switch_manager = Arc::clone(&mgr);
        let switch = std::thread::spawn(move || {
            switch_manager.switch_alias_with_test_hook("active", "v2", Some(&PassValidator), || {
                validated_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });

        validated_rx.recv().unwrap();
        mgr.drain_and_remove("v2").await.unwrap();
        mgr.register_collection("v2", make_index(4, 12)).unwrap();
        release_tx.send(()).unwrap();

        assert!(matches!(
            switch.join().unwrap(),
            Err(AliasError::CollectionNotFound(name)) if name == "v2"
        ));
        assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
        assert_eq!(mgr.acquire_reader("active").unwrap().len(), 5);

        mgr.create_alias("replacement", "v2").unwrap();
        assert_eq!(mgr.acquire_reader("replacement").unwrap().len(), 12);
    }

    #[tokio::test]
    async fn test_drain_and_remove() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();

        // No readers -- drain should succeed immediately
        mgr.drain_and_remove("v1").await.unwrap();
        assert_eq!(mgr.collection_count(), 0);
    }

    /// Regression for #1438: cleanup of a known candidate must not remove a
    /// different index that later reuses the same collection name.
    #[tokio::test]
    async fn test_retire_collection_preserves_same_name_replacement() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        let original = Arc::clone(&mgr.collections.read().get("v1").unwrap().index);

        mgr.drain_and_remove("v1").await.unwrap();
        mgr.register_collection("v1", make_index(4, 10)).unwrap();

        assert!(matches!(
            mgr.retire_collection("v1", Some(&original)),
            Err(AliasError::CollectionNotFound(name)) if name == "v1"
        ));
        mgr.create_alias("active", "v1").unwrap();
        assert_eq!(mgr.acquire_reader("active").unwrap().len(), 10);
    }

    /// Regression for #1438: rejecting retirement must leave every alias and
    /// collection usable, including aliases unrelated to the rejected target.
    #[tokio::test]
    async fn test_drain_and_remove_rejects_aliased_collection_without_state_change() {
        let mgr = IndexAliasManager::new(Duration::from_secs(1));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.register_collection("v2", make_index(4, 10)).unwrap();
        mgr.create_alias("active", "v1").unwrap();
        mgr.create_alias("mirror", "v1").unwrap();
        mgr.create_alias("fallback", "v2").unwrap();

        let result = mgr.drain_and_remove("v1").await;
        assert!(matches!(
            result,
            Err(AliasError::CollectionInUse(name)) if name == "v1"
        ));

        assert_eq!(mgr.collection_count(), 2);
        assert_eq!(mgr.alias_count(), 3);
        assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
        assert_eq!(mgr.resolve_alias("mirror"), Some("v1".to_string()));
        assert_eq!(mgr.resolve_alias("fallback"), Some("v2".to_string()));
        assert_eq!(mgr.acquire_reader("active").unwrap().len(), 5);
        assert_eq!(mgr.acquire_reader("mirror").unwrap().len(), 5);
        assert_eq!(mgr.acquire_reader("fallback").unwrap().len(), 10);
    }

    #[tokio::test]
    async fn test_concurrent_read_during_swap() {
        let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.register_collection("v2", make_index(4, 10)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        // Acquire a reader on v1
        let guard = mgr.acquire_reader("active").unwrap();
        assert_eq!(guard.len(), 5);

        // Swap alias to v2 while v1 reader is active
        let old = mgr.switch_alias("active", "v2", None).unwrap();
        assert_eq!(old, "v1");

        // Old reader should still see v1 (5 vectors)
        assert_eq!(guard.len(), 5);

        // New reader should see v2 (10 vectors)
        let new_guard = mgr.acquire_reader("active").unwrap();
        assert_eq!(new_guard.len(), 10);

        // Drop the old reader
        drop(guard);

        // Now drain should succeed for v1
        let mgr_clone = Arc::clone(&mgr);
        mgr_clone.drain_and_remove("v1").await.unwrap();

        // v1 should be gone, v2 should remain
        assert_eq!(mgr.collection_count(), 1);
    }

    #[tokio::test]
    async fn test_migrate() {
        let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        // Prepare vectors for the new index
        let vectors: Vec<(NodeId, Vec<f32>)> = (0..8u8)
            .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
            .collect();

        let config = HnswConfig::with_dimensions(4);
        let report = mgr.migrate("active", vectors, config, None).await.unwrap();

        assert_eq!(report.old_size, 5);
        assert_eq!(report.new_size, 8);

        // The alias should now point to the new collection
        let guard = mgr.acquire_reader("active").unwrap();
        assert_eq!(guard.len(), 8);
        assert_eq!(mgr.reader_count("v1"), None);
        assert_eq!(mgr.collection_count(), 1);
    }

    /// Regression for #1438: migrating one alias must keep an old collection
    /// registered when another alias still targets it.
    #[tokio::test]
    async fn test_migrate_preserves_old_collection_used_by_another_alias() {
        let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.create_alias("active", "v1").unwrap();
        mgr.create_alias("fallback", "v1").unwrap();

        let vectors: Vec<(NodeId, Vec<f32>)> = (0..8u8)
            .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
            .collect();
        let report = mgr
            .migrate("active", vectors, HnswConfig::with_dimensions(4), None)
            .await
            .unwrap();

        assert_eq!(
            mgr.resolve_alias("active"),
            Some(report.new_collection.clone())
        );
        assert_eq!(mgr.resolve_alias("fallback"), Some("v1".to_string()));
        assert_eq!(mgr.acquire_reader("active").unwrap().len(), 8);
        assert_eq!(mgr.acquire_reader("fallback").unwrap().len(), 5);
        assert_eq!(mgr.collection_count(), 2);
    }

    /// Regression for #1438: validation cleanup must retain a failed candidate
    /// once an alias begins referencing it.
    #[test]
    fn test_migrate_validation_failure_preserves_aliased_candidate() {
        use std::sync::{mpsc, Mutex};

        struct BlockingFailValidator {
            entered: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }

        impl IndexValidator for BlockingFailValidator {
            fn validate(&self, _index: &HnswIndex) -> Result<(), AliasError> {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Err(AliasError::ValidationFailed {
                    reason: "candidate rejected".to_string(),
                    recall: Some(0.4),
                    min_recall: Some(0.9),
                })
            }
        }

        let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::channel();
        let migration_manager = Arc::clone(&mgr);
        let migration = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let vectors = (0..8u8)
                .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
                .collect();
            runtime.block_on(migration_manager.migrate(
                "active",
                vectors,
                HnswConfig::with_dimensions(4),
                Some(Box::new(BlockingFailValidator {
                    entered: entered_tx,
                    release: Mutex::new(release_rx),
                })),
            ))
        });

        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("validator did not start");
        let candidate = mgr
            .collections
            .read()
            .keys()
            .find(|name| name.starts_with("v1_migrated_"))
            .cloned()
            .expect("registered migration candidate");
        let alias_result = mgr.create_alias("candidate", &candidate);
        release_tx.send(()).unwrap();
        let migration_result = migration.join().unwrap();

        alias_result.unwrap();
        assert!(matches!(
            migration_result,
            Err(AliasError::ValidationFailed {
                reason,
                recall: Some(0.4),
                min_recall: Some(0.9),
            }) if reason == "recall below threshold"
        ));
        assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
        assert_eq!(mgr.resolve_alias("candidate"), Some(candidate));
        assert_eq!(mgr.acquire_reader("active").unwrap().len(), 5);
        assert_eq!(mgr.acquire_reader("candidate").unwrap().len(), 8);
        assert_eq!(mgr.collection_count(), 2);
    }

    /// Regression for #417: acquiring a reader must not race a concurrent
    /// alias switch + drain/remove of the collection the reader resolved
    /// to. Pauses right after alias resolution (deterministic via channel,
    /// no sleep), then performs a switch+drain+remove that would, on the
    /// buggy code, remove the collection before the reader is counted.
    #[tokio::test]
    async fn test_acquire_reader_switch_drain_race_does_not_return_collection_not_found() {
        let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.register_collection("v2", make_index(4, 10)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        let (paused_tx, paused_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (b_done_tx, b_done_rx) = std::sync::mpsc::channel::<()>();

        let mgr_a = Arc::clone(&mgr);
        let handle_a = std::thread::spawn(move || {
            mgr_a.acquire_reader_with_test_hook("active", || {
                paused_tx.send(()).expect("send paused signal");
                release_rx.recv().expect("recv release signal");
            })
        });

        // Wait until thread A has resolved alias -> v1 and is paused.
        paused_rx.recv().expect("recv paused signal");

        // Concurrently switch the alias to v2 and drain+remove v1 on a
        // second thread -- exactly what a migration does. On the buggy
        // code this has no lock contention and completes immediately,
        // removing v1 before A is released below. On the fixed code it
        // blocks behind A's held `aliases` read lock until A is released.
        let mgr_b = Arc::clone(&mgr);
        let handle_b = std::thread::spawn(move || {
            mgr_b.switch_alias("active", "v2", None).unwrap();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build nested runtime for thread B");
            rt.block_on(mgr_b.drain_and_remove("v1")).unwrap();
            b_done_tx.send(()).ok();
        });

        // Give B a bounded window to run to completion if nothing blocks it
        // (the pre-fix case: B finishes almost instantly). Whether or not
        // it finished, release A next -- on the fixed code B is still
        // blocked at this point and only proceeds once A releases the
        // `aliases` lock below.
        let _ = b_done_rx.recv_timeout(Duration::from_millis(200));
        release_tx.send(()).expect("send release signal");

        let result = handle_a.join().expect("thread A join");
        let a_len = match result {
            Ok(guard) => {
                let len = guard.len();
                // Drop the guard so B's drain_and_remove (waiting on this
                // reader on the fixed code) does not block on join below.
                drop(guard);
                len
            }
            Err(e) => panic!(
                "acquire_reader must not return an error due to a switch/drain race, got {e:?}"
            ),
        };
        handle_b.join().expect("thread B join");

        assert_eq!(a_len, 5, "must return the old v1 snapshot");
    }

    /// Regression for #418: if drain times out during migration, the old
    /// collection must be retired from `self.collections` immediately
    /// (not kept forever), even though the held reader guard keeps the
    /// underlying index alive until it is dropped.
    #[tokio::test]
    async fn test_migrate_timeout_retires_old_collection_from_manager() {
        let mgr = Arc::new(IndexAliasManager::new(Duration::ZERO));
        mgr.register_collection("v1", make_index(4, 5)).unwrap();
        mgr.create_alias("active", "v1").unwrap();

        // Hold a reader guard on v1 so drain can never complete within the
        // zero-duration timeout.
        let guard = mgr.acquire_reader("active").unwrap();
        assert_eq!(guard.len(), 5);

        let vectors: Vec<(NodeId, Vec<f32>)> = (0..8u8)
            .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
            .collect();
        let config = HnswConfig::with_dimensions(4);

        let report = mgr
            .migrate("active", vectors, config, None)
            .await
            .expect("migration must succeed even if drain times out");
        assert_eq!(report.old_collection, "v1");

        assert_eq!(
            mgr.resolve_alias("active"),
            Some(report.new_collection.clone())
        );
        assert_eq!(
            mgr.reader_count("v1"),
            None,
            "old collection must no longer be manager-owned even though drain timed out"
        );
        assert_eq!(
            mgr.collection_count(),
            1,
            "manager must not keep the retired collection around forever"
        );

        // The held guard must still be usable until dropped.
        assert_eq!(guard.len(), 5);
        drop(guard);
        assert_eq!(mgr.collection_count(), 1);
    }
}
