//! Index alias manager: atomic blue-green swap for zero-downtime HNSW index migration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use super::drain::{drain_readers, ReaderCounter, ReaderGuard};
use super::error::AliasError;
use super::validation::IndexValidator;
use crate::config::HnswConfig;
use crate::HnswIndex;
use crate::NodeId;

/// Process-wide discriminator for migration collection names created within
/// the same wall-clock millisecond. Managers do not share registries, but a
/// single manager can build several candidates concurrently (#1437).
static MIGRATION_NAME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
        self.register_collection_and_pin(name, index).map(|_| ())
    }

    /// Register a named collection and retain the exact index identity that
    /// was inserted. Holding the returned `Arc` closes the remove/re-register
    /// gap between exposing a migration candidate and pinning it (#1437).
    fn register_collection_and_pin(
        &self,
        name: &str,
        index: HnswIndex,
    ) -> Result<Arc<HnswIndex>, AliasError> {
        let index = Arc::new(index);
        let mut collections = self.collections.write();
        if collections.contains_key(name) {
            return Err(AliasError::CollectionAlreadyExists(name.to_string()));
        }
        collections.insert(
            name.to_string(),
            Collection {
                index: Arc::clone(&index),
                readers: Arc::new(ReaderCounter::new()),
            },
        );
        Ok(index)
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

    /// Publish a migration candidate only while `alias` still targets the
    /// exact collection captured before the candidate build began, and retire
    /// that source atomically when no other alias still references it.
    ///
    /// Both the alias comparison and candidate identity check run under the
    /// canonical aliases -> collections lock order. The `Arc` comparison
    /// prevents a same-name collection replacement from being published after
    /// a migration validated a different index (#1438).
    fn publish_migration_if_current(
        &self,
        alias: &str,
        expected_collection: &str,
        expected_old_index: &Arc<HnswIndex>,
        new_collection: &str,
        expected_new_index: &Arc<HnswIndex>,
    ) -> Result<Option<Arc<ReaderCounter>>, AliasError> {
        let mut aliases = self.aliases.write();
        let mut collections = self.collections.write();
        let candidate = collections
            .get(new_collection)
            .ok_or_else(|| AliasError::CollectionNotFound(new_collection.to_string()))?;
        if !Arc::ptr_eq(expected_new_index, &candidate.index) {
            return Err(AliasError::CollectionNotFound(new_collection.to_string()));
        }

        let actual_collection = aliases
            .get(alias)
            .ok_or_else(|| AliasError::AliasNotFound(alias.to_string()))?;
        if actual_collection != expected_collection {
            return Err(AliasError::AliasTargetChanged {
                alias: alias.to_string(),
                expected: expected_collection.to_string(),
                actual: actual_collection.clone(),
            });
        }
        let old_collection = collections
            .get(expected_collection)
            .ok_or_else(|| AliasError::CollectionNotFound(expected_collection.to_string()))?;
        if !Arc::ptr_eq(expected_old_index, &old_collection.index) {
            // The alias name made an ABA transition back to a different
            // same-name collection while this candidate was building.
            return Err(AliasError::CollectionNotFound(
                expected_collection.to_string(),
            ));
        }

        aliases.insert(alias.to_string(), new_collection.to_string());
        if aliases.values().any(|target| target == expected_collection) {
            // Another alias still owns the source collection (#1438).
            return Ok(None);
        }

        let retired = collections
            .remove(expected_collection)
            .expect("source identity checked while holding the collections write lock");
        Ok(Some(Arc::clone(&retired.readers)))
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

    /// Build and validate a replacement, then publish it only if `alias` still
    /// targets the collection captured at the start of the migration.
    ///
    /// A concurrent migration that publishes first causes this call to clean
    /// up its candidate and return [`AliasError::AliasTargetChanged`].
    pub async fn migrate(
        &self,
        alias: &str,
        vectors: Vec<(NodeId, Vec<f32>)>,
        new_config: HnswConfig,
        validator: Option<Box<dyn IndexValidator>>,
    ) -> Result<MigrationReport, AliasError> {
        self.migrate_inner(alias, vectors, new_config, validator, |_| {})
            .await
    }

    async fn migrate_inner<F>(
        &self,
        alias: &str,
        vectors: Vec<(NodeId, Vec<f32>)>,
        new_config: HnswConfig,
        validator: Option<Box<dyn IndexValidator>>,
        after_candidate_registered: F,
    ) -> Result<MigrationReport, AliasError>
    where
        F: FnOnce(&str),
    {
        let total_start = Instant::now();

        // Resolve current alias to get old collection info
        let old_collection_name = {
            let aliases = self.aliases.read();
            aliases
                .get(alias)
                .ok_or_else(|| AliasError::AliasNotFound(alias.to_string()))?
                .clone()
        };

        let old_index = {
            let collections = self.collections.read();
            Arc::clone(
                &collections
                    .get(&old_collection_name)
                    .ok_or_else(|| AliasError::CollectionNotFound(old_collection_name.clone()))?
                    .index,
            )
        };
        let old_size = old_index.len_live();

        // Generate a unique name for the new collection
        let new_collection_name = format!(
            "{}_migrated_{}_{}",
            old_collection_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            MIGRATION_NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed),
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

        // Create the candidate identity before registry exposure and retain it
        // across validation, conditional publication, cleanup, and reporting.
        // A remove/re-register race can replace the name, but never this Arc.
        let candidate_index = self.register_collection_and_pin(&new_collection_name, new_index)?;
        after_candidate_registered(&new_collection_name);
        let new_size = candidate_index.len_live();

        // Validate and swap
        let swap_drain_start = Instant::now();

        // Recall score for the report
        let recall_score = None;

        // If we have a validator, run it and capture recall
        if let Some(ref v) = validator {
            // Validation may block on recalls; the exact candidate snapshot
            // above avoids monopolizing collection registry updates.
            match v.validate(&candidate_index) {
                Ok(()) => {}
                Err(AliasError::ValidationFailed {
                    recall, min_recall, ..
                }) => {
                    // Preserve the validation error even if another alias now
                    // owns the candidate or its name refers to a replacement.
                    let _ = self.retire_collection(&new_collection_name, Some(&candidate_index));
                    return Err(AliasError::ValidationFailed {
                        reason: "recall below threshold".to_string(),
                        recall,
                        min_recall,
                    });
                }
                Err(e) => {
                    let _ = self.retire_collection(&new_collection_name, Some(&candidate_index));
                    return Err(e);
                }
            }
        }

        // Compare-and-switch against the exact target captured before the
        // build, retiring that source in the same lock transaction when no
        // other alias needs it. A loser must neither publish nor leak its
        // replacement.
        let retired_readers = match self.publish_migration_if_current(
            alias,
            &old_collection_name,
            &old_index,
            &new_collection_name,
            &candidate_index,
        ) {
            Ok(readers) => readers,
            Err(error) => {
                let _ = self.retire_collection(&new_collection_name, Some(&candidate_index));
                return Err(error);
            }
        };
        // The source is no longer manager-owned when `retired_readers` is
        // `Some`; release this migration's identity pin before waiting.
        drop(old_index);

        // The atomic publish step already retired the old collection when
        // eligible. Drain its readers without holding either registry lock.
        let drain_result = match retired_readers {
            Some(readers) => {
                drain_readers(&readers, self.drain_timeout, self.drain_poll_interval).await
            }
            None => Ok(()),
        };

        let swap_drain_duration = swap_drain_start.elapsed();
        let total_duration = total_start.elapsed();

        match drain_result {
            Ok(()) => {}
            Err(AliasError::DrainTimeout { .. }) => {
                // The old collection is already retired from manager ownership;
                // report the completed migration without waiting longer.
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
#[path = "manager/tests.rs"]
mod tests;
