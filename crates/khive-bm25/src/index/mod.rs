//! BM25 inverted index: in-memory inverted index with WAND acceleration and SIMD scoring.
//!
//! Scores are converted to `DeterministicScore` at the API boundary for cross-platform
//! consistency. See `docs/algorithm.md` for BM25 properties, floating-point design
//! rationale, WAND block-max details, IDF cache design, and thread-safety trade-offs.

mod indexing;
mod memory;
mod search;

pub use search::SearchContext;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, RwLock};

use super::config::Bm25Config;
use super::tokenizer::{BoxedTokenizer, SimpleTokenizer};
use crate::error::{Result, RetrievalError};
use crate::metrics::MetricsSink;

/// IDF cache keyed by document frequency (`df`) rather than term string.
///
/// IDF depends on two inputs: `df` (document frequency of a term) and `N`
/// (total document count). Multiple terms sharing the same `df` produce
/// identical IDF values, so keying by `df` (a `usize`) is both more compact
/// and more correct than keying by term string.
///
/// When `N` changes (any add/remove), the entire cache is invalidated by
/// comparing `cached_doc_count` against the current `doc_count()`. This
/// eliminates the stale-IDF bug where targeted per-term eviction left
/// entries computed with the old `N` in the cache.
#[derive(Debug, Default)]
pub(crate) struct IdfCache {
    /// The `N` (total document count) for which cached values are valid.
    cached_doc_count: AtomicUsize,
    /// Map from document frequency -> precomputed IDF value.
    by_df: RwLock<HashMap<usize, f64>>,
}

impl Clone for IdfCache {
    fn clone(&self) -> Self {
        let map_clone = self.by_df.read().map(|m| m.clone()).unwrap_or_default();
        Self {
            cached_doc_count: AtomicUsize::new(self.cached_doc_count.load(AtomicOrdering::Relaxed)),
            by_df: RwLock::new(map_clone),
        }
    }
}

/// Default tokenizer for deserialization.
fn default_tokenizer() -> BoxedTokenizer {
    Arc::new(SimpleTokenizer::default())
}

/// Serde helpers for `Vec<Arc<str>>` ↔ `Vec<String>` (transparent wire format).
mod arc_str_vec_serde {
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

pub const DEFAULT_BLOCK_SIZE: usize = 128;
const INITIAL_POSTINGS_EPOCH: u64 = 0;

fn default_block_size() -> usize {
    DEFAULT_BLOCK_SIZE
}

fn default_postings_epoch() -> u64 {
    INITIAL_POSTINGS_EPOCH
}

/// Typed document identifier for BM25 index operations.
///
/// Wire format: plain JSON string (serde transparent).
///
/// # ID Bridging (Hybrid Search)
///
/// When combining BM25 keyword results with HNSW vector results in hybrid
/// search, the ID types differ: BM25 uses `DocumentId` (string-based) while
/// HNSW uses `EmbeddingId` (128-bit UUID-based). Bridging strategies include:
///
/// 1. String-based fusion: convert both ID types to `String` before fusion.
/// 2. DocumentId fusion: convert `EmbeddingId` to `DocumentId` via its
///    display representation, then fuse using `DocumentId`.
/// 3. Application-level mapping: maintain a lookup table mapping between
///    `EmbeddingId` and `DocumentId` in the application layer.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct DocumentId(String);

impl DocumentId {
    /// Create a new `DocumentId` from any `Into<String>`.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume `self` and return the inner `String`.
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Return the length of the underlying string in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Return `true` if the underlying string is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for DocumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::ops::Deref for DocumentId {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for DocumentId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for DocumentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for DocumentId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for DocumentId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl PartialEq<str> for DocumentId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for DocumentId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for DocumentId {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

/// Structure-of-Arrays posting list for memory-efficient storage.
///
/// Stores doc_ids (`Vec<u32>`) and term_freqs (`Vec<u8>`) in separate
/// contiguous arrays, achieving exactly 5 bytes per posting with no
/// alignment padding waste (vs 8 bytes for AoS `struct { u32, u8 }`).
///
/// At 200K postings this saves ~600 KB; at 1M postings, ~3 MB.
///
/// Both arrays are always the same length and sorted by doc_id.
#[derive(Debug, Clone, Default, Serialize)]
#[doc(hidden)]
pub struct PostingList {
    /// Document IDs, sorted ascending for binary-search seeks in WAND.
    pub doc_ids: Vec<u32>,
    /// Term frequencies, parallel to `doc_ids`. Clamped to u8::MAX (255).
    pub(crate) term_freqs: Vec<u8>,
}

impl<'de> serde::Deserialize<'de> for PostingList {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as DeError;

        // Deserialize into raw struct first.
        #[derive(serde::Deserialize)]
        struct Raw {
            doc_ids: Vec<u32>,
            term_freqs: Vec<u8>,
        }

        let raw = Raw::deserialize(deserializer)?;

        // Invariant: lengths must match.
        if raw.doc_ids.len() != raw.term_freqs.len() {
            return Err(D::Error::custom(format!(
                "PostingList invariant violated: doc_ids.len()={} != term_freqs.len()={}",
                raw.doc_ids.len(),
                raw.term_freqs.len()
            )));
        }

        // Invariant: doc_ids must be sorted in strictly ascending order.
        if raw.doc_ids.windows(2).any(|w| w[0] >= w[1]) {
            return Err(D::Error::custom(
                "PostingList invariant violated: doc_ids must be strictly sorted ascending",
            ));
        }

        // Invariant: no sentinel doc IDs (u32::MAX is used as TERMINATED_DOC).
        if raw.doc_ids.contains(&u32::MAX) {
            return Err(D::Error::custom(
                "PostingList invariant violated: doc_id u32::MAX is reserved as a sentinel",
            ));
        }

        Ok(PostingList {
            doc_ids: raw.doc_ids,
            term_freqs: raw.term_freqs,
        })
    }
}

impl PostingList {
    /// Number of postings in this list.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.doc_ids.len()
    }

    /// Whether the posting list is empty.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.doc_ids.is_empty()
    }

    /// Insert a posting at the given position, maintaining sorted order.
    #[inline]
    pub(crate) fn insert(&mut self, index: usize, doc_id: u32, term_freq: u8) {
        self.doc_ids.insert(index, doc_id);
        self.term_freqs.insert(index, term_freq);
    }

    /// Remove the posting at the given position.
    #[inline]
    pub(crate) fn remove(&mut self, index: usize) {
        self.doc_ids.remove(index);
        self.term_freqs.remove(index);
    }

    /// Find the insertion point for a doc_id (binary search).
    #[inline]
    pub(crate) fn partition_point_by_doc_id(&self, target: u32) -> usize {
        self.doc_ids.partition_point(|&id| id < target)
    }

    /// Memory usage in bytes (actual heap allocation, no padding waste).
    #[inline]
    #[allow(dead_code)] // REASON: reserved for memory diagnostics endpoint
    pub(crate) fn heap_bytes(&self) -> usize {
        // Vec<u32> capacity * 4 + Vec<u8> capacity * 1
        // Use len() as approximation (capacity >= len)
        self.doc_ids.len() * 4 + self.term_freqs.len()
    }
}

/// Per-block BM25 upper-bound metadata for a posting list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BlockMaxBlock {
    /// Smallest document id in the block.
    pub(crate) min_doc_id: u32,
    /// Largest document id in the block.
    pub(crate) max_doc_id: u32,
    /// Maximum exact BM25 contribution of this term among postings in the block.
    pub(crate) max_score_contribution: f64,
    /// Suffix maximum of `max_score_contribution` from this block to the end.
    pub(crate) suffix_max_score: f64,
}

/// Block-max metadata for a term posting list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct TermBlockMaxMeta {
    pub(crate) blocks: Vec<BlockMaxBlock>,
}

/// Lazily rebuilt block-max metadata cache.
///
/// `built_epoch` is `None` when the cache is stale (needs rebuild), or
/// `Some(epoch)` when the cache was built for the given epoch value.
/// Using `Option<u64>` avoids the sentinel-collision bug where
/// `postings_epoch == u64::MAX` would equal the old `STALE_BLOCK_MAX_EPOCH`
/// sentinel after `wrapping_add(1)` cycles through all u64 values.
#[derive(Debug, Clone, Default)]
pub(crate) struct BlockMaxState {
    pub(crate) built_epoch: Option<u64>,
    pub(crate) per_term: HashMap<String, TermBlockMaxMeta>,
}

/// BM25 (Okapi BM25) keyword index.
///
/// An in-memory inverted index for keyword search with BM25 scoring.
/// Supports incremental updates (add/remove documents) and efficient search.
///
/// `search()` takes `&self` to allow concurrent reads; the IDF cache and block-max
/// metadata use `RwLock` for interior mutability (RETRIEVAL-08). IDF cache
/// auto-invalidates on doc-count change; block-max metadata uses an epoch counter.
/// See `docs/algorithm.md` for full thread-safety rationale and design alternatives.
///
/// Custom tokenizers can be set via [`with_tokenizer`](Self::with_tokenizer).
///
/// # Deserialization Safety
///
/// Implements custom `Deserialize` that validates structural invariants and rebuilds
/// all derived caches (`doc_lengths_vec`, `doc_lengths_f32`, `forward_index`) after
/// loading from JSON to prevent panics in the SIMD search paths.
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

    /// Reverse map: internal u32 ID -> shared string slice.
    ///
    /// Uses `Arc<str>` instead of `DocumentId` (which wraps `String`) so that
    /// `resolve_internal_id` can hand out a clone in O(1) — an atomic refcount
    /// increment — rather than a heap allocation + memcpy of the UUID string.
    /// All search hot-path callers only need `AsRef<str>` / `Deref<Target=str>`,
    /// which `Arc<str>` satisfies.  The serde wire format is identical to the
    /// old `DocumentId` representation (both serialize as a bare JSON string).
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

    /// IDF cache keyed by document frequency (`df`), auto-invalidated when
    /// `doc_count()` changes. See [`IdfCache`] for design rationale.
    #[serde(skip, default)]
    pub(crate) idf_cache: IdfCache,

    /// Vec-indexed document lengths for O(1) hot-path access during scoring.
    /// Indexed by internal u32 doc_id. Rebuilt from `doc_lengths` on
    /// deserialization. This avoids HashMap lookups in the tight scoring loop.
    #[serde(skip, default)]
    pub(crate) doc_lengths_vec: Vec<usize>,

    /// Pre-converted f32 document lengths for SIMD batch scoring.
    /// Maintained in parallel with `doc_lengths_vec`. Avoids per-scoring
    /// `usize -> f32` conversion in the tight NEON batch loop.
    #[serde(skip, default)]
    pub(crate) doc_lengths_f32: Vec<f32>,

    /// Configuration parameters.
    pub(crate) config: Bm25Config,

    /// Tokenizer for text processing.
    /// Defaults to SimpleTokenizer. Skip serialization as tokenizers may not be serializable.
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
            .map(|state| state.clone())
            .unwrap_or_default();

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
        Self::new(Bm25Config::default())
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

        // Validate: PostingList doc_ids must reference valid internal IDs.
        // Each doc_id in a posting list must exist in doc_lengths.
        for (term, postings) in &wire.inverted_index {
            for &doc_id in &postings.doc_ids {
                if !wire.doc_lengths.contains_key(&doc_id) {
                    return Err(D::Error::custom(format!(
                        "PostingList for term '{}' references doc_id {} not in doc_lengths",
                        term, doc_id
                    )));
                }
            }
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
    /// Create a new empty BM25 index with the given configuration.
    ///
    /// # Panics
    ///
    /// Panics if config validation fails (k1 < 0 or b outside [0, 1]).
    /// Use [`Bm25Index::try_new`] to handle invalid config as an error.
    pub fn new(config: Bm25Config) -> Self {
        if let Err(e) = config.validate() {
            panic!("invalid BM25 config: {e}");
        }
        Self {
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
        }
    }

    /// Non-panicking constructor.  Returns `Err(RetrievalError::Configuration(…))`
    /// if the config is invalid instead of panicking.
    pub fn try_new(config: Bm25Config) -> Result<Self> {
        config
            .validate()
            .map_err(|e| RetrievalError::Configuration(format!("invalid BM25 config: {e}")))?;
        Ok(Self::new(config))
    }

    /// Create a new BM25 index with a custom tokenizer.
    ///
    /// # Panics
    ///
    /// Panics if config validation fails (k1 < 0 or b outside [0, 1]).
    pub fn with_tokenizer(config: Bm25Config, tokenizer: BoxedTokenizer) -> Self {
        if let Err(e) = config.validate() {
            panic!("invalid BM25 config: {e}");
        }
        Self {
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
        }
    }

    /// Set the tokenizer.
    ///
    /// Note: This does not re-tokenize existing documents.
    /// Clear and re-index if you need consistent tokenization.
    pub fn set_tokenizer(&mut self, tokenizer: BoxedTokenizer) {
        self.tokenizer = tokenizer;
    }

    /// Get a reference to the current tokenizer.
    pub fn tokenizer(&self) -> &BoxedTokenizer {
        &self.tokenizer
    }

    /// Attach a metrics sink (builder pattern).
    ///
    /// The sink receives [`MetricEvent`]s from `search` and `index_document`
    /// operations. Pass an `Arc<dyn MetricsSink>` to share a single sink
    /// across multiple indices.
    #[must_use]
    pub fn with_metrics(mut self, sink: Arc<dyn MetricsSink>) -> Self {
        self.metrics = Some(sink);
        self
    }

    /// Set or replace the metrics sink at runtime.
    ///
    /// Pass `Some(sink)` to enable metrics, or `None` to disable.
    pub fn set_metrics(&mut self, sink: Option<Arc<dyn MetricsSink>>) {
        self.metrics = sink;
    }

    /// Get the number of indexed documents.
    pub fn doc_count(&self) -> usize {
        self.doc_lengths.len()
    }

    /// Get the average document length (in tokens).
    ///
    /// Returns 0.0 if no documents are indexed.
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
    ///
    /// Returns `Err(RetrievalError::IdSpaceExhausted)` if the u32 ID space
    /// is fully consumed (more than `u32::MAX` unique document IDs assigned
    /// over the lifetime of this index).
    fn get_or_assign_internal_id(&mut self, doc_id: &DocumentId) -> Result<u32> {
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

    /// Resolve an internal u32 ID back to an `Arc<str>`.
    ///
    /// Returns a cheaply cloneable shared reference.  Callers in the search
    /// hot path can `Arc::clone` this without any heap allocation.
    #[inline]
    fn resolve_internal_id(&self, internal_id: u32) -> Option<Arc<str>> {
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

    /// Rebuild `forward_index` from the inverted index if it appears empty.
    ///
    /// Called after deserialization to restore the forward map so that
    /// `remove_document` can operate in O(|terms_in_doc|) instead of
    /// falling back to the O(|vocabulary|) full scan.
    ///
    /// This mirrors the `ensure_doc_lengths_vec` pattern: the forward index is
    /// not stored on disk (it is fully derivable from the inverted index), but
    /// it must be populated before any removal takes place.
    ///
    /// **Correctness note**: this method returns early only when the forward
    /// index is empty *or* the inverted index is empty. It does NOT treat
    /// "non-empty" as "complete" — use [`ensure_forward_index_complete`] when
    /// completeness must be guaranteed (e.g. after deserialization + partial
    /// insert). The deserialization path calls `ensure_forward_index_complete`
    /// instead of this method.
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

    /// Rebuild `forward_index` ensuring it is complete for all live documents.
    ///
    /// Unlike [`ensure_forward_index`], this checks whether every document in
    /// `doc_lengths` is present in the forward index — not merely whether the
    /// forward index is non-empty.  This is the correct guard to use after
    /// deserialization, where "non-empty" could mean only newly-inserted docs
    /// are tracked while pre-serde docs are missing.
    ///
    /// If the forward index is already complete (every key in `doc_lengths`
    /// has an entry), this is a no-op. Otherwise the entire forward index is
    /// rebuilt from the inverted index.
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

    /// Return the sorted doc_ids for a term's posting list.
    ///
    /// Returns `None` if the term has no postings. Used by integration tests to
    /// verify that posting lists are kept in sorted order after mutations.
    #[doc(hidden)]
    pub fn inverted_index_for_test(&self, term: &str) -> Option<PostingList> {
        self.inverted_index.get(term).cloned()
    }

    /// Invalidate block-max metadata after a corpus mutation.
    ///
    /// Bumps the postings epoch so that the next WAND search lazily rebuilds
    /// block-max metadata. The IDF cache self-invalidates on the next search
    /// when it detects that `doc_count()` has changed.
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

/// Compute IDF from document frequency using the Robertson-Walker variant.
///
/// **PROOF CORRESPONDENCE**: `khive.Retrieval.BM25.idf_nonneg`
/// With +1 inside ln(), IDF(t) >= 0 for all terms regardless of document frequency.
#[inline]
pub(crate) fn idf_from_doc_freq(doc_freq: usize, doc_count: usize) -> f64 {
    let n = doc_count as f64;
    let df = doc_freq as f64;
    ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
}

/// Compute a single-term BM25 contribution for a posting.
///
/// **PROOF CORRESPONDENCE**: `khive.Retrieval.BM25.tf_bounded`
/// TF saturation: tf * (k1 + 1) / (tf + k1 * ...) < k1 + 1 for all tf >= 0.
#[inline]
pub(crate) fn bm25_term_score(
    idf: f64,
    term_freq: u8,
    doc_length: usize,
    avgdl: f64,
    k1: f64,
    b: f64,
) -> f64 {
    if avgdl <= f64::EPSILON {
        return 0.0;
    }

    let tf = term_freq as f64;
    let numerator = tf * (k1 + 1.0);
    let denominator = tf + k1 * (1.0 - b + b * (doc_length as f64 / avgdl));
    idf * (numerator / denominator)
}

/// Pre-computed BM25 scoring constants for a single term.
///
/// Eliminates redundant arithmetic in the tight per-posting scoring loop.
/// The BM25 formula per posting is:
///   score = idf * (tf * (k1+1)) / (tf + k1 * (1 - b + b * dl/avgdl))
///
/// Pre-computing `k1_plus_1`, `k1_times_one_minus_b`, and `k1_times_b_over_avgdl`
/// reduces the per-posting work to: 1 multiply, 1 FMA, 1 add, 1 divide, 1 multiply.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bm25TermScorer {
    pub(crate) idf: f64,
    /// k1 + 1.0
    k1_plus_1: f64,
    /// k1 * (1.0 - b)
    k1_times_one_minus_b: f64,
    /// k1 * b / avgdl
    k1_times_b_over_avgdl: f64,
}

impl Bm25TermScorer {
    #[inline]
    pub(crate) fn new(idf: f64, k1: f64, b: f64, avgdl: f64) -> Self {
        let inv_avgdl = if avgdl > f64::EPSILON {
            1.0 / avgdl
        } else {
            0.0
        };
        Self {
            idf,
            k1_plus_1: k1 + 1.0,
            k1_times_one_minus_b: k1 * (1.0 - b),
            k1_times_b_over_avgdl: k1 * b * inv_avgdl,
        }
    }

    /// IDF value for this term.
    #[inline]
    pub(crate) fn idf_f32(&self) -> f32 {
        self.idf as f32
    }

    /// Pre-computed k1 + 1.
    #[inline]
    pub(crate) fn k1_plus_1_f32(&self) -> f32 {
        self.k1_plus_1 as f32
    }

    /// Pre-computed k1 * (1 - b), the constant portion of the denominator.
    #[inline]
    pub(crate) fn denom_base_f32(&self) -> f32 {
        self.k1_times_one_minus_b as f32
    }

    /// Pre-computed k1 * b / avgdl, the per-doc-length factor in the denominator.
    #[inline]
    pub(crate) fn denom_dl_factor_f32(&self) -> f32 {
        self.k1_times_b_over_avgdl as f32
    }

    /// Score a posting with pre-computed constants.
    #[inline]
    pub(crate) fn score(&self, term_freq: u8, doc_length: usize) -> f64 {
        let tf = term_freq as f64;
        let numerator = tf * self.k1_plus_1;
        let denominator =
            tf + self.k1_times_one_minus_b + self.k1_times_b_over_avgdl * (doc_length as f64);
        self.idf * (numerator / denominator)
    }
}

fn build_term_block_max_meta(
    postings: &PostingList,
    doc_lengths: &HashMap<u32, usize>,
    block_size: usize,
    idf: f64,
    avgdl: f64,
    k1: f64,
    b: f64,
) -> TermBlockMaxMeta {
    if postings.is_empty() {
        return TermBlockMaxMeta::default();
    }

    let n = postings.len();
    let num_blocks = n.div_ceil(block_size);
    let mut blocks = Vec::with_capacity(num_blocks);

    for block_idx in 0..num_blocks {
        let start = block_idx * block_size;
        let end = (start + block_size).min(n);

        let min_doc_id = postings.doc_ids[start];
        let max_doc_id = postings.doc_ids[end - 1];

        let mut max_score_contribution = 0.0;
        for i in start..end {
            let doc_id = postings.doc_ids[i];
            let term_freq = postings.term_freqs[i];
            let doc_length = doc_lengths.get(&doc_id).copied().unwrap_or(0);
            let score = bm25_term_score(idf, term_freq, doc_length, avgdl, k1, b);
            if score > max_score_contribution {
                max_score_contribution = score;
            }
        }

        blocks.push(BlockMaxBlock {
            min_doc_id,
            max_doc_id,
            max_score_contribution,
            suffix_max_score: max_score_contribution,
        });
    }

    // Compute suffix-max scores (back to front).
    let mut suffix_max = 0.0;
    for block in blocks.iter_mut().rev() {
        if block.max_score_contribution > suffix_max {
            suffix_max = block.max_score_contribution;
        }
        block.suffix_max_score = suffix_max;
    }

    TermBlockMaxMeta { blocks }
}

/// Statistics about a BM25 index.
#[derive(Debug, Clone, Default)]
pub struct Bm25Stats {
    /// Number of indexed documents.
    pub doc_count: usize,
    /// Total token count across all documents.
    pub total_tokens: usize,
    /// Average document length (in tokens).
    pub avg_doc_length: f64,
    /// Number of unique terms in the index.
    pub unique_terms: usize,
}

// ── Wire-format fixture: DocumentId ──────────────────────────────────────────
//
// These tests lock the JSON wire representation of `DocumentId` after its
// migration to `transparent_string_newtype!`. Updating this fixture = a
// wire-format migration that requires a PR-level migration plan.
#[cfg(test)]
mod document_id_wire_format {
    use super::DocumentId;

    /// Frozen wire-format fixture: DocumentId must serialize as a bare JSON string.
    ///
    /// Wire format: `"some-document-identifier"` (not `{"0":"..."}` or any other shape).
    /// This is enforced by `#[serde(transparent)]` in the macro expansion.
    #[test]
    fn document_id_serializes_as_plain_string() {
        let id = DocumentId::new("some-document-identifier");
        let json = serde_json::to_string(&id).expect("DocumentId serialize");
        assert_eq!(
            json, r#""some-document-identifier""#,
            "wire format drift detected in DocumentId — must be plain JSON string",
        );
    }

    #[test]
    fn document_id_roundtrip() {
        let id = DocumentId::new("doc_abc_123");
        let json = serde_json::to_string(&id).expect("serialize");
        let back: DocumentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id, "serde roundtrip must produce identical value");
    }

    #[test]
    fn document_id_empty_string_roundtrip() {
        let id = DocumentId::new("");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(
            json, r#""""#,
            "empty DocumentId must serialize as empty JSON string"
        );
        let back: DocumentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn document_id_unicode_roundtrip() {
        let id = DocumentId::new("doc_\u{4e2d}\u{6587}");
        let json = serde_json::to_string(&id).expect("serialize");
        let back: DocumentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }
}
