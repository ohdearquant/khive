//! `Bm25Index` struct definition, constructors, public methods, and serde impl.

use std::collections::HashMap;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::{Arc, RwLock};

use serde::Serialize;

use super::document_id::DocumentId;
use super::posting::{BlockMaxState, PostingList};
use super::scoring::{build_term_block_max_meta, idf_from_doc_freq, Bm25Stats, IdfCache};
use crate::config::Bm25Config;
use crate::error::{Result, RetrievalError};
use crate::metrics::MetricsSink;
use crate::tokenizer::{BoxedTokenizer, SimpleTokenizer};

pub const DEFAULT_BLOCK_SIZE: usize = 128;
const INITIAL_POSTINGS_EPOCH: u64 = 0;

pub(super) fn default_block_size() -> usize {
    DEFAULT_BLOCK_SIZE
}

pub(super) fn default_postings_epoch() -> u64 {
    INITIAL_POSTINGS_EPOCH
}

/// Default tokenizer for deserialization.
pub(super) fn default_tokenizer() -> BoxedTokenizer {
    Arc::new(SimpleTokenizer::default())
}

/// Serde helpers for `Vec<Arc<str>>` ↔ `Vec<String>` (transparent wire format).
pub(super) mod arc_str_vec_serde {
    use std::sync::Arc;

    pub fn serialize<S>(v: &[Arc<str>], ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;
        let mut seq = ser.serialize_seq(Some(v.len()))?;
        for s in v {
            seq.serialize_element(s.as_ref())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(de: D) -> Result<Vec<Arc<str>>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::Deserialize;
        let strings: Vec<String> = Vec::deserialize(de)?;
        Ok(strings.into_iter().map(|s| Arc::from(s.as_str())).collect())
    }
}

/// In-memory BM25 index whose custom deserializer validates state and rebuilds derived caches.
///
/// See `crates/khive-bm25/docs/api/index-lifecycle.md`.
#[derive(Serialize)]
pub struct Bm25Index {
    /// Term -> posting list (SoA layout: separate doc_id and term_freq arrays).
    /// Posting lists are sorted by doc_id for binary-search seeks in WAND.
    pub(crate) inverted_index: HashMap<String, PostingList>,

    /// Document lengths (in tokens) keyed by internal u32 ID.
    /// Kept for serialization compatibility and `doc_count()`.
    pub(crate) doc_lengths: HashMap<u32, usize>,

    /// Forward map: external DocumentId -> internal u32 ID.
    pub(crate) id_to_internal: HashMap<DocumentId, u32>,

    /// Reverse map: internal u32 ID -> `Arc<str>` for O(1) refcount clone on search.
    #[serde(with = "arc_str_vec_serde")]
    pub(crate) internal_to_id: Vec<Arc<str>>,

    /// Next internal ID to assign.
    pub(crate) next_internal_id: u32,

    /// Total token count across all documents.
    pub(crate) total_tokens: usize,

    /// Monotonic counter incremented whenever postings or corpus statistics change.
    /// Used to lazily invalidate block-max metadata.
    #[serde(default = "default_postings_epoch")]
    pub(crate) postings_epoch: u64,

    /// Fixed posting-list block size used for block-max metadata.
    #[serde(default = "default_block_size")]
    pub(crate) block_size: usize,

    /// Lazily rebuilt block-max metadata.
    #[serde(skip, default)]
    pub(crate) block_max_state: RwLock<BlockMaxState>,

    /// IDF cache keyed by df, auto-invalidated on doc_count change.
    #[serde(skip, default)]
    pub(crate) idf_cache: IdfCache,
    /// Vec-indexed doc lengths for O(1) hot-path access (rebuilt on deserialization).
    #[serde(skip, default)]
    pub(crate) doc_lengths_vec: Vec<usize>,
    /// Pre-converted f32 doc lengths for SIMD batch scoring.
    #[serde(skip, default)]
    pub(crate) doc_lengths_f32: Vec<f32>,
    pub(crate) config: Bm25Config,
    #[serde(skip, default = "default_tokenizer")]
    pub(crate) tokenizer: BoxedTokenizer,

    /// Forward index: internal doc_id -> list of terms in that document.
    /// Enables O(terms_in_doc) removal instead of O(|vocabulary|).
    #[serde(skip, default)]
    pub(crate) forward_index: HashMap<u32, Vec<String>>,

    /// Optional metrics sink for observability.
    #[serde(skip)]
    pub(crate) metrics: Option<Arc<dyn MetricsSink>>,
}

impl std::fmt::Debug for Bm25Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bm25Index")
            .field("doc_count", &self.doc_lengths.len())
            .field("unique_terms", &self.inverted_index.len())
            .field("total_tokens", &self.total_tokens)
            .field("block_size", &self.block_size)
            .field("config", &self.config)
            .finish()
    }
}

impl Clone for Bm25Index {
    fn clone(&self) -> Self {
        let block_max_clone = self
            .block_max_state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        Self {
            inverted_index: self.inverted_index.clone(),
            doc_lengths: self.doc_lengths.clone(),
            id_to_internal: self.id_to_internal.clone(),
            // Arc<str> clone = atomic refcount bump, not a String heap copy.
            internal_to_id: self.internal_to_id.clone(),
            next_internal_id: self.next_internal_id,
            total_tokens: self.total_tokens,
            postings_epoch: self.postings_epoch,
            block_size: self.block_size,
            block_max_state: RwLock::new(block_max_clone),
            idf_cache: self.idf_cache.clone(),
            doc_lengths_vec: self.doc_lengths_vec.clone(),
            doc_lengths_f32: self.doc_lengths_f32.clone(),
            forward_index: self.forward_index.clone(),
            config: self.config.clone(),
            tokenizer: self.tokenizer.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

impl Default for Bm25Index {
    fn default() -> Self {
        Self::try_new(Bm25Config::default()).expect("default Bm25Config is always valid")
    }
}

/// Wire representation used only during deserialization of `Bm25Index`.
/// Mirrors the serialized fields (those without `#[serde(skip)]`).
#[derive(serde::Deserialize)]
struct Bm25IndexWire {
    inverted_index: HashMap<String, PostingList>,
    doc_lengths: HashMap<u32, usize>,
    id_to_internal: HashMap<DocumentId, u32>,
    #[serde(with = "arc_str_vec_serde")]
    internal_to_id: Vec<Arc<str>>,
    next_internal_id: u32,
    total_tokens: usize,
    #[serde(default = "default_postings_epoch")]
    postings_epoch: u64,
    #[serde(default = "default_block_size")]
    block_size: usize,
    config: Bm25Config,
}

impl<'de> serde::Deserialize<'de> for Bm25Index {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as DeError;

        let wire = Bm25IndexWire::deserialize(deserializer)?;

        // Validate: block_size must be > 0 to avoid div-by-zero in WAND metadata.
        if wire.block_size == 0 {
            return Err(D::Error::custom("block_size must be > 0"));
        }

        // Validate: config must be valid (catches NaN/Inf k1 and b from serialized input).
        wire.config
            .validate()
            .map_err(|e| D::Error::custom(format!("invalid config: {e}")))?;

        // Validate: every live doc_id (a doc_lengths key) must be in range and have a
        // consistent internal_to_id / id_to_internal mapping; accumulate a checked sum
        // of doc_lengths so total_tokens can be cross-checked below.
        let mut checked_total: usize = 0;
        for (&internal_id, &len) in &wire.doc_lengths {
            if internal_id == u32::MAX || internal_id >= wire.next_internal_id {
                return Err(D::Error::custom(format!(
                    "doc_lengths contains invalid live doc_id {internal_id} (next_internal_id={})",
                    wire.next_internal_id
                )));
            }
            let external = wire
                .internal_to_id
                .get(internal_id as usize)
                .ok_or_else(|| {
                    D::Error::custom(format!(
                        "missing internal_to_id entry for live doc_id {internal_id}"
                    ))
                })?;
            match wire.id_to_internal.get(external.as_ref() as &str) {
                Some(&mapped) if mapped == internal_id => {}
                _ => {
                    return Err(D::Error::custom(format!(
                        "id_to_internal does not map '{external}' back to live doc_id {internal_id}"
                    )));
                }
            }
            checked_total = checked_total.checked_add(len).ok_or_else(|| {
                D::Error::custom("total_tokens overflow while summing doc_lengths")
            })?;
        }

        // Validate: every id_to_internal entry must point at a live, in-range doc that
        // reverse-maps back through internal_to_id to the same external ID.
        for (external_id, &internal_id) in &wire.id_to_internal {
            if internal_id == u32::MAX
                || internal_id >= wire.next_internal_id
                || !wire.doc_lengths.contains_key(&internal_id)
            {
                return Err(D::Error::custom(format!(
                    "id_to_internal maps '{external_id}' to non-live doc_id {internal_id}"
                )));
            }
            let reverse = wire
                .internal_to_id
                .get(internal_id as usize)
                .ok_or_else(|| {
                    D::Error::custom(format!("missing internal_to_id entry for '{external_id}'"))
                })?;
            if reverse.as_ref() != external_id.as_str() {
                return Err(D::Error::custom(format!(
                    "internal_to_id[{internal_id}] does not reverse-map to '{external_id}'"
                )));
            }
        }

        // Validate: PostingList doc_ids must reference only live, in-range doc IDs.
        for (term, postings) in &wire.inverted_index {
            for &doc_id in &postings.doc_ids {
                if doc_id == u32::MAX
                    || doc_id >= wire.next_internal_id
                    || !wire.doc_lengths.contains_key(&doc_id)
                {
                    return Err(D::Error::custom(format!(
                        "PostingList for term '{}' references non-live doc_id {}",
                        term, doc_id
                    )));
                }
            }
        }

        // Validate: total_tokens must equal the checked sum of doc_lengths.
        if wire.total_tokens != checked_total {
            return Err(D::Error::custom(format!(
                "total_tokens {} does not match doc_lengths sum {}",
                wire.total_tokens, checked_total
            )));
        }

        // Build the derived caches from the persisted data.
        let mut index = Bm25Index {
            inverted_index: wire.inverted_index,
            doc_lengths: wire.doc_lengths,
            id_to_internal: wire.id_to_internal,
            internal_to_id: wire.internal_to_id,
            next_internal_id: wire.next_internal_id,
            total_tokens: wire.total_tokens,
            postings_epoch: wire.postings_epoch,
            block_size: wire.block_size,
            block_max_state: RwLock::new(BlockMaxState::default()),
            idf_cache: IdfCache::default(),
            doc_lengths_vec: Vec::new(),
            doc_lengths_f32: Vec::new(),
            config: wire.config,
            tokenizer: default_tokenizer(),
            forward_index: HashMap::new(),
            metrics: None,
        };

        // Rebuild the fast-path vectors so that SIMD search does not panic.
        index.ensure_doc_lengths_vec();

        // Rebuild the forward index so that remove_document works correctly
        // even before any new documents are inserted.
        index.ensure_forward_index_complete();

        Ok(index)
    }
}

impl Bm25Index {
    /// Create a new empty BM25 index. Panics on invalid config; use `try_new` for an error.
    #[deprecated(
        note = "use `try_new` (or `try_with_tokenizer`), which returns a Result instead of panicking on an invalid config"
    )]
    pub fn new(config: Bm25Config) -> Self {
        Self::try_new(config).expect("invalid BM25 config")
    }

    /// Non-panicking constructor. Returns `Err(RetrievalError::Configuration(...))`
    /// if the config is invalid instead of panicking.
    pub fn try_new(config: Bm25Config) -> Result<Self> {
        config
            .validate()
            .map_err(|e| RetrievalError::Configuration(format!("invalid BM25 config: {e}")))?;
        Ok(Self {
            inverted_index: HashMap::new(),
            doc_lengths: HashMap::new(),
            id_to_internal: HashMap::new(),
            internal_to_id: Vec::new(),
            next_internal_id: 0,
            total_tokens: 0,
            postings_epoch: INITIAL_POSTINGS_EPOCH,
            block_size: DEFAULT_BLOCK_SIZE,
            block_max_state: RwLock::new(BlockMaxState::default()),
            idf_cache: IdfCache::default(),
            doc_lengths_vec: Vec::new(),
            doc_lengths_f32: Vec::new(),
            forward_index: HashMap::new(),
            config,
            tokenizer: Arc::new(SimpleTokenizer::default()),
            metrics: None,
        })
    }

    /// Non-panicking constructor with a custom tokenizer. Returns `Err(RetrievalError::Configuration(...))`
    /// if the config is invalid instead of panicking.
    pub fn try_with_tokenizer(config: Bm25Config, tokenizer: BoxedTokenizer) -> Result<Self> {
        config
            .validate()
            .map_err(|e| RetrievalError::Configuration(format!("invalid BM25 config: {e}")))?;
        Ok(Self {
            inverted_index: HashMap::new(),
            doc_lengths: HashMap::new(),
            id_to_internal: HashMap::new(),
            internal_to_id: Vec::new(),
            next_internal_id: 0,
            total_tokens: 0,
            postings_epoch: INITIAL_POSTINGS_EPOCH,
            block_size: DEFAULT_BLOCK_SIZE,
            block_max_state: RwLock::new(BlockMaxState::default()),
            idf_cache: IdfCache::default(),
            doc_lengths_vec: Vec::new(),
            doc_lengths_f32: Vec::new(),
            forward_index: HashMap::new(),
            config,
            tokenizer,
            metrics: None,
        })
    }

    /// Create a new BM25 index with a custom tokenizer. Panics on invalid config.
    #[deprecated(
        note = "use `try_new` (or `try_with_tokenizer`), which returns a Result instead of panicking on an invalid config"
    )]
    pub fn with_tokenizer(config: Bm25Config, tokenizer: BoxedTokenizer) -> Self {
        Self::try_with_tokenizer(config, tokenizer).expect("invalid BM25 config")
    }

    /// Set the tokenizer. Does not re-tokenize existing documents.
    pub fn set_tokenizer(&mut self, tokenizer: BoxedTokenizer) {
        self.tokenizer = tokenizer;
    }

    /// Get a reference to the current tokenizer.
    pub fn tokenizer(&self) -> &BoxedTokenizer {
        &self.tokenizer
    }

    /// Attach a metrics sink (builder pattern).
    #[must_use]
    pub fn with_metrics(mut self, sink: Arc<dyn MetricsSink>) -> Self {
        self.metrics = Some(sink);
        self
    }

    /// Set or replace the metrics sink at runtime.
    pub fn set_metrics(&mut self, sink: Option<Arc<dyn MetricsSink>>) {
        self.metrics = sink;
    }

    /// Get the number of indexed documents.
    pub fn doc_count(&self) -> usize {
        self.doc_lengths.len()
    }

    /// Get the average document length (in tokens). Returns 0.0 for empty index.
    pub fn avg_doc_length(&self) -> f64 {
        let count = self.doc_count();
        if count == 0 {
            0.0
        } else {
            self.total_tokens as f64 / count as f64
        }
    }

    /// Check if a document is indexed.
    pub fn contains_document(&self, doc_id: &str) -> bool {
        self.id_to_internal.contains_key(doc_id)
    }

    /// Get or assign an internal u32 ID for a `DocumentId`.
    pub(crate) fn get_or_assign_internal_id(&mut self, doc_id: &DocumentId) -> Result<u32> {
        if let Some(&id) = self.id_to_internal.get(doc_id) {
            return Ok(id);
        }
        let id = self.next_internal_id;
        self.next_internal_id = self
            .next_internal_id
            .checked_add(1)
            .ok_or(RetrievalError::IdSpaceExhausted)?;
        self.id_to_internal.insert(doc_id.clone(), id);
        if id as usize >= self.internal_to_id.len() {
            // Placeholder: Arc<str> from empty &str.
            self.internal_to_id.resize(id as usize + 1, Arc::from(""));
        }
        // Store as Arc<str> — avoids cloning the full String on every
        // search hit; lookup just does an atomic refcount bump.
        self.internal_to_id[id as usize] = Arc::from(doc_id.as_str());
        Ok(id)
    }

    /// Resolve an internal u32 ID to an `Arc<str>` (refcount clone, no allocation).
    #[inline]
    pub(crate) fn resolve_internal_id(&self, internal_id: u32) -> Option<Arc<str>> {
        self.internal_to_id
            .get(internal_id as usize)
            .map(Arc::clone)
    }

    /// Get the configuration.
    pub fn config(&self) -> &Bm25Config {
        &self.config
    }

    /// Clear the index, removing all documents.
    pub fn clear(&mut self) {
        self.inverted_index.clear();
        self.doc_lengths.clear();
        self.doc_lengths_vec.clear();
        self.doc_lengths_f32.clear();
        self.forward_index.clear();
        self.id_to_internal.clear();
        self.internal_to_id.clear();
        self.next_internal_id = 0;
        self.total_tokens = 0;
        self.postings_epoch = INITIAL_POSTINGS_EPOCH;
        self.idf_cache
            .cached_doc_count
            .store(0, AtomicOrdering::Relaxed);
        if let Ok(mut cache) = self.idf_cache.by_df.write() {
            cache.clear();
        }
        if let Ok(mut block_state) = self.block_max_state.write() {
            block_state.built_epoch = None;
            block_state.per_term.clear();
        }
    }

    /// Update the O(1) doc_lengths_vec for a given internal id.
    /// Called on every document insert.
    #[inline]
    pub(crate) fn set_doc_length_fast(&mut self, internal_id: u32, length: usize) {
        let idx = internal_id as usize;
        if idx >= self.doc_lengths_vec.len() {
            self.doc_lengths_vec.resize(idx + 1, 0);
        }
        self.doc_lengths_vec[idx] = length;
        // Keep f32 mirror in sync for SIMD batch scoring.
        if idx >= self.doc_lengths_f32.len() {
            self.doc_lengths_f32.resize(idx + 1, 0.0);
        }
        self.doc_lengths_f32[idx] = length as f32;
    }

    /// Look up document length by internal id using the fast Vec path.
    /// Falls back to HashMap if Vec is not yet populated (deserialization).
    #[inline]
    pub(crate) fn doc_length_fast(&self, internal_id: u32) -> usize {
        let idx = internal_id as usize;
        if idx < self.doc_lengths_vec.len() {
            self.doc_lengths_vec[idx]
        } else {
            debug_assert!(
                false,
                "doc_lengths_vec not populated for internal_id {internal_id}"
            );
            self.doc_lengths.get(&internal_id).copied().unwrap_or(0)
        }
    }

    /// Rebuild `doc_lengths_vec` and `doc_lengths_f32` from `doc_lengths` HashMap.
    /// Called after deserialization to populate the fast-path Vecs (see `persist::bm25`).
    pub fn ensure_doc_lengths_vec(&mut self) {
        if !self.doc_lengths_vec.is_empty() || self.doc_lengths.is_empty() {
            return;
        }
        let max_id = self.doc_lengths.keys().copied().max().unwrap_or(0) as usize;
        self.doc_lengths_vec.resize(max_id + 1, 0);
        self.doc_lengths_f32.resize(max_id + 1, 0.0);
        for (&id, &len) in &self.doc_lengths {
            self.doc_lengths_vec[id as usize] = len;
            self.doc_lengths_f32[id as usize] = len as f32;
        }
    }

    /// Rebuild `forward_index` from inverted index if empty; no-op when populated.
    pub fn ensure_forward_index(&mut self) {
        if !self.forward_index.is_empty() || self.inverted_index.is_empty() {
            return;
        }
        for (term, postings) in &self.inverted_index {
            for &doc_id in &postings.doc_ids {
                if self.doc_lengths.contains_key(&doc_id) {
                    self.forward_index
                        .entry(doc_id)
                        .or_default()
                        .push(term.clone());
                }
            }
        }
    }

    /// Rebuild `forward_index` for all docs; stronger than `ensure_forward_index`.
    pub fn ensure_forward_index_complete(&mut self) {
        if self.inverted_index.is_empty() {
            return;
        }

        // Check completeness: every doc in doc_lengths must have an entry.
        let already_complete = self.forward_index.len() == self.doc_lengths.len()
            && self
                .doc_lengths
                .keys()
                .all(|id| self.forward_index.contains_key(id));

        if already_complete {
            return;
        }

        // Rebuild from scratch.
        self.forward_index.clear();
        for (term, postings) in &self.inverted_index {
            for &doc_id in &postings.doc_ids {
                if self.doc_lengths.contains_key(&doc_id) {
                    self.forward_index
                        .entry(doc_id)
                        .or_default()
                        .push(term.clone());
                }
            }
        }
    }

    /// Get statistics about the index.
    pub fn stats(&self) -> Bm25Stats {
        Bm25Stats {
            doc_count: self.doc_count(),
            total_tokens: self.total_tokens,
            avg_doc_length: self.avg_doc_length(),
            unique_terms: self.inverted_index.len(),
        }
    }

    /// Check if the IDF cache is empty.
    pub fn is_idf_cache_empty(&self) -> bool {
        self.idf_cache
            .by_df
            .read()
            .map(|cache| cache.is_empty())
            .unwrap_or(true)
    }

    /// Return the posting list for a term, or `None` if absent (for tests).
    #[doc(hidden)]
    pub fn inverted_index_for_test(&self, term: &str) -> Option<PostingList> {
        self.inverted_index.get(term).cloned()
    }

    /// Bump postings epoch to lazily invalidate block-max metadata.
    #[inline]
    pub(crate) fn invalidate_block_max_after_mutation(&mut self) {
        self.postings_epoch = self.postings_epoch.wrapping_add(1);
        if let Ok(mut block_state) = self.block_max_state.write() {
            block_state.built_epoch = None;
            block_state.per_term.clear();
        }
    }

    /// Lazily rebuild block-max metadata if the current epoch is stale.
    pub(crate) fn ensure_block_max_metadata(&self) {
        let target_epoch = self.postings_epoch;

        if let Ok(block_state) = self.block_max_state.read() {
            if block_state.built_epoch == Some(target_epoch) {
                return;
            }
        }

        let doc_count = self.doc_count();
        if doc_count == 0 {
            if let Ok(mut block_state) = self.block_max_state.write() {
                block_state.built_epoch = Some(target_epoch);
                block_state.per_term.clear();
            }
            return;
        }

        let avgdl = self.avg_doc_length();
        let k1 = self.config.k1;
        let b = self.config.b;

        if let Ok(mut block_state) = self.block_max_state.write() {
            // Double-check under write lock (another thread may have rebuilt).
            if block_state.built_epoch == Some(target_epoch) {
                return;
            }

            let mut per_term = HashMap::with_capacity(self.inverted_index.len());
            for (term, postings) in &self.inverted_index {
                let term_meta = build_term_block_max_meta(
                    postings,
                    &self.doc_lengths,
                    self.block_size,
                    idf_from_doc_freq(postings.len(), doc_count),
                    avgdl,
                    k1,
                    b,
                );
                per_term.insert(term.clone(), term_meta);
            }

            block_state.per_term = per_term;
            block_state.built_epoch = Some(target_epoch);
        }
    }
}

#[cfg(test)]
mod regression_tests {
    use crate::{Bm25Config, Bm25Index};

    #[test]
    fn serde_roundtrip_search_works_with_4_postings() {
        let mut index = Bm25Index::default();
        for i in 0..4 {
            index.index_document(format!("doc{i}"), "alpha").unwrap();
        }
        let json = serde_json::to_string(&index).unwrap();
        let restored: Bm25Index = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.doc_lengths_f32.len(),
            restored.next_internal_id as usize,
            "doc_lengths_f32 must be rebuilt on deserialization"
        );
        let results = restored.search("alpha", 10);
        assert_eq!(results.len(), 4, "all 4 docs must be found");
    }

    #[test]
    fn serde_roundtrip_search_works_with_8_postings() {
        let mut index = Bm25Index::default();
        for i in 0..8 {
            index.index_document(format!("doc{i}"), "alpha").unwrap();
        }
        let json = serde_json::to_string(&index).unwrap();
        let restored: Bm25Index = serde_json::from_str(&json).unwrap();
        let results = restored.search("alpha", 10);
        assert_eq!(results.len(), 8, "all 8 docs must be found");
    }

    #[test]
    fn remove_old_doc_after_deserialize_and_new_insert_leaves_no_stale_posting() {
        let mut index = Bm25Index::default();
        index.index_document("old_doc", "alpha").unwrap();
        let json = serde_json::to_string(&index).unwrap();
        let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();
        restored.index_document("new_doc", "beta").unwrap();
        for internal_id in restored.doc_lengths.keys() {
            assert!(
                restored.forward_index.contains_key(internal_id),
                "forward_index must cover every live doc after new insert post-serde"
            );
        }
        assert!(restored.remove_document("old_doc"));
        let hits = restored.search("alpha", 10);
        assert!(
            hits.is_empty(),
            "old_doc must not remain searchable after removal"
        );
    }

    #[test]
    fn reindex_with_empty_text_preserves_old_document() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "original content").unwrap();
        index.index_document("doc1", "").unwrap();
        assert!(
            index.contains_document("doc1"),
            "doc must survive a no-op empty reindex"
        );
        let results = index.search("original", 10);
        assert_eq!(
            results.len(),
            1,
            "doc must still be searchable after empty reindex"
        );
    }

    #[test]
    fn budget_check_does_not_overflow() {
        let config = Bm25Config::default().with_memory_budget(1);
        let mut index = Bm25Index::try_new(config).expect("valid config");
        let result = index.index_document("doc1", "hello world");
        assert!(result.is_err(), "budget should be exceeded");
    }

    #[test]
    fn config_nan_k1_rejected_by_try_new() {
        let config = Bm25Config::new(f64::NAN, 0.75);
        assert!(
            Bm25Index::try_new(config).is_err(),
            "NaN k1 must be rejected by try_new"
        );
    }

    #[test]
    fn config_inf_b_rejected_by_try_new() {
        let config = Bm25Config::new(1.2, f64::INFINITY);
        assert!(
            Bm25Index::try_new(config).is_err(),
            "Inf b must be rejected by try_new"
        );
    }

    #[test]
    fn config_nan_k1_rejected_by_try_with_tokenizer() {
        use std::sync::Arc;
        let config = Bm25Config::new(f64::NAN, 0.75);
        let tokenizer = Arc::new(crate::tokenizer::SimpleTokenizer::default());
        assert!(
            Bm25Index::try_with_tokenizer(config, tokenizer).is_err(),
            "NaN k1 must be rejected by try_with_tokenizer"
        );
    }

    #[test]
    fn config_inf_b_rejected_by_try_with_tokenizer() {
        use std::sync::Arc;
        let config = Bm25Config::new(1.2, f64::INFINITY);
        let tokenizer = Arc::new(crate::tokenizer::SimpleTokenizer::default());
        assert!(
            Bm25Index::try_with_tokenizer(config, tokenizer).is_err(),
            "Inf b must be rejected by try_with_tokenizer"
        );
    }

    #[test]
    fn block_size_zero_rejected_on_deserialization() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "hello world").unwrap();
        let json = serde_json::to_string(&index).unwrap();
        let tampered = json.replace(
            &format!("\"block_size\":{}", super::DEFAULT_BLOCK_SIZE),
            "\"block_size\":0",
        );
        let result: Result<Bm25Index, _> = serde_json::from_str(&tampered);
        assert!(
            result.is_err(),
            "block_size=0 must be rejected during deserialization"
        );
    }

    #[test]
    fn postings_epoch_max_does_not_collide_with_stale_sentinel() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "hello world").unwrap();
        let json = serde_json::to_string(&index).unwrap();
        let tampered = json.replace(
            &format!("\"postings_epoch\":{}", index.postings_epoch),
            &format!("\"postings_epoch\":{}", u64::MAX),
        );
        let restored: Bm25Index = serde_json::from_str(&tampered).unwrap();
        let results = restored.search("hello", 10);
        assert_eq!(
            results.len(),
            1,
            "search must work with postings_epoch=u64::MAX"
        );
    }

    #[test]
    fn posting_list_sentinel_doc_id_rejected_via_index_serde() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "hello").unwrap();
        let json = serde_json::to_string(&index).unwrap();
        let tampered = json.replace("[0],\"term_freqs\":[1]", "[4294967295],\"term_freqs\":[1]");
        if tampered == json {
            return;
        }
        let result: Result<Bm25Index, _> = serde_json::from_str(&tampered);
        assert!(
            result.is_err(),
            "posting list with u32::MAX doc_id must be rejected"
        );
    }

    #[test]
    fn unsorted_posting_list_rejected_via_index_serde() {
        let mut index = Bm25Index::default();
        index.index_document("doc0", "common term").unwrap();
        index.index_document("doc1", "common word").unwrap();
        let json = serde_json::to_string(&index).unwrap();
        let tampered = json.replace("[0,1],\"term_freqs\"", "[1,0],\"term_freqs\"");
        if tampered == json {
            return;
        }
        let result: Result<Bm25Index, _> = serde_json::from_str(&tampered);
        assert!(
            result.is_err(),
            "posting list with unsorted doc_ids must be rejected"
        );
    }

    #[test]
    fn bm25_index_clone_recovers_idf_cache_from_poisoned_lock() {
        let mut index = Bm25Index::default();
        // Index enough documents so the IDF cache gets populated.
        for i in 0..5 {
            index
                .index_document(format!("doc{i}"), "rust systems programming")
                .unwrap();
        }
        // Warm the IDF cache by running a search.
        let _ = index.search("rust", 10);
        assert!(
            !index.is_idf_cache_empty(),
            "IDF cache must be populated before poisoning"
        );

        // Poison the IDF cache lock by panicking inside its write-lock scope.
        let _ = std::panic::catch_unwind(|| {
            let _guard = index.idf_cache.by_df.write().unwrap();
            panic!("intentional idf cache poison");
        });
        assert!(
            index.idf_cache.by_df.read().is_err(),
            "IDF cache lock must be poisoned"
        );

        // Clone must preserve populated IDF cache, not collapse to empty.
        let cloned = index.clone();
        assert!(
            !cloned.is_idf_cache_empty(),
            "cloned index must retain non-empty IDF cache after lock was poisoned"
        );

        // Search results must be non-empty (scores non-zero) — not silently zeroed.
        let results = cloned.search("rust", 10);
        assert_eq!(
            results.len(),
            5,
            "all documents must be found in clone of poisoned index"
        );
        assert!(
            results.iter().all(|(_id, score)| score.to_f64() > 0.0),
            "BM25 scores must be non-zero; a collapsed IDF cache would produce score=0"
        );
    }

    #[test]
    fn bm25_index_clone_recovers_block_max_state_from_poisoned_lock() {
        let mut index = Bm25Index::default();
        for i in 0..4 {
            index
                .index_document(format!("doc{i}"), "search engine indexing")
                .unwrap();
        }
        // Force block-max metadata to be built.
        index.ensure_block_max_metadata();

        // Poison the block_max_state lock.
        let _ = std::panic::catch_unwind(|| {
            let _guard = index.block_max_state.write().unwrap();
            panic!("intentional block_max poison");
        });
        assert!(
            index.block_max_state.read().is_err(),
            "block_max_state lock must be poisoned"
        );

        // Clone must succeed and the clone must be searchable.
        let cloned = index.clone();
        let results = cloned.search("search", 10);
        assert_eq!(
            results.len(),
            4,
            "all documents must be found in clone after block_max_state lock was poisoned"
        );
    }

    /// A schema-valid single-live-doc wire snapshot: doc "doc0" -> internal id 0,
    /// one posting for term "alpha". Tests corrupt one field at a time from this base.
    fn valid_single_doc_json() -> serde_json::Value {
        serde_json::json!({
            "inverted_index": {
                "alpha": { "doc_ids": [0], "term_freqs": [1] }
            },
            "doc_lengths": { "0": 1 },
            "id_to_internal": { "doc0": 0 },
            "internal_to_id": ["doc0"],
            "next_internal_id": 1,
            "total_tokens": 1,
            "postings_epoch": 0,
            "block_size": 128,
            "config": { "k1": 1.2, "b": 0.75, "memory_budget": null }
        })
    }

    #[test]
    fn deserialize_rejects_doc_length_id_at_u32_max() {
        let json = serde_json::json!({
            "inverted_index": {},
            "doc_lengths": { "4294967295": 1 },
            "id_to_internal": {},
            "internal_to_id": [],
            "next_internal_id": 0,
            "total_tokens": 1,
            "postings_epoch": 0,
            "block_size": 128,
            "config": { "k1": 1.2, "b": 0.75, "memory_budget": null }
        });
        let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
        assert!(
            err.to_string().contains("invalid live doc_id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_live_doc_id_at_or_above_next_internal_id() {
        let mut json = valid_single_doc_json();
        // doc_lengths key 0 is fine, but shift next_internal_id below it.
        json["next_internal_id"] = serde_json::json!(0);
        let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
        assert!(
            err.to_string().contains("invalid live doc_id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_posting_id_at_or_above_next_internal_id() {
        let mut json = valid_single_doc_json();
        json["doc_lengths"] = serde_json::json!({ "0": 1, "5": 1 });
        json["inverted_index"] = serde_json::json!({
            "alpha": { "doc_ids": [0, 5], "term_freqs": [1, 1] }
        });
        json["total_tokens"] = serde_json::json!(2);
        // next_internal_id stays 1, so doc_id 5 is out of range for both doc_lengths and postings.
        let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
        assert!(
            err.to_string().contains("invalid live doc_id")
                || err.to_string().contains("non-live doc_id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_missing_internal_to_id_for_live_doc() {
        let mut json = valid_single_doc_json();
        json["internal_to_id"] = serde_json::json!([]);
        let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
        assert!(
            err.to_string().contains("missing internal_to_id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_mismatched_internal_to_id_reverse_mapping() {
        let mut json = valid_single_doc_json();
        json["internal_to_id"] = serde_json::json!(["wrong"]);
        let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
        assert!(
            err.to_string().contains("does not map")
                || err.to_string().contains("does not reverse-map"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_stale_id_to_internal_value() {
        let mut json = valid_single_doc_json();
        // "stale" points at internal id 7, which is neither live nor in range.
        json["id_to_internal"] = serde_json::json!({ "doc0": 0, "stale": 7 });
        let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
        assert!(
            err.to_string().contains("non-live doc_id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_bad_total_tokens() {
        let mut json = valid_single_doc_json();
        json["total_tokens"] = serde_json::json!(4);
        let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
        assert!(
            err.to_string().contains("does not match doc_lengths sum"),
            "unexpected error: {err}"
        );
    }
}
