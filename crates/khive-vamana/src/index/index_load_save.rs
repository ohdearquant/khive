use std::borrow::Cow;

#[cfg(feature = "mmap")]
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use bytemuck::cast_slice;
#[cfg(feature = "mmap")]
use memmap2::MmapOptions;

use crate::{
    config::VamanaConfig,
    error::{Result, VamanaError},
    graph::{CodesView, VamanaGraph},
};

#[cfg(feature = "parallel")]
use super::build_pool;
#[cfg(all(test, feature = "mmap"))]
use super::checkpoint_allocation_tests;
#[cfg(feature = "mmap")]
use super::{
    capped_reverse_adjacency, encode_codes_bin, encode_graph, encode_metadata, hash_file_mmap,
    hash_vectors_file, is_read_only_lock_create_error, map_checkpoint_segment, mmap_vectors,
    parse_codes_bin, publication_file_snapshot, read_graph, read_metadata,
    reject_checkpoint_sequence_regression, stage_legacy_replacement, validate_v2_structural,
    METADATA_MAGIC, V2_COMMIT_MAGIC,
};
use super::{
    encode_graph_lossless, encode_lifecycle, encode_portable_container, encode_portable_ids,
    encode_v2_commit_full, exact_search, parse_graph, parse_lifecycle, parse_portable_container,
    parse_portable_ids, parse_v2_commit, require_finite, required_segment, tombstone_words_for,
    train_codec_and_encode, validate_free_slots, validate_reverse_adjacency, CodeStore,
    CorpusFingerprint, SearchVisitedPool, V2CorpusFingerprint, VamanaIndex, VamanaIndexSnapshot,
    VamanaSnapshot, VectorStorage, DEFAULT_CONSOLIDATION_TAU, VAMANA_SNAPSHOT_FORMAT,
    VAMANA_SNAPSHOT_VERSION,
};

impl VamanaIndex {
    /// Build from row-major flat slice. Errors if config invalid, empty, wrong length, non-finite, or N > u32::MAX.
    ///
    /// Uses `GsSq8Codec` for the acquisition-tier distance during graph construction
    /// (ADR-052 §1, Step 2: default-on for Vamana, algebraically exact in code space).
    ///
    /// Runs inside the bounded build pool (see `build_thread_count`), so every
    /// nested `par_iter` in graph construction inherits that bound instead of the
    /// global pool's one-thread-per-core.
    pub fn build(vectors: &[f32], config: VamanaConfig) -> Result<Self> {
        Self::build_buffer(Cow::Borrowed(vectors), config)
    }

    /// Build while retaining the caller's row-major vector buffer.
    pub fn build_owned(vectors: Vec<f32>, config: VamanaConfig) -> Result<Self> {
        Self::build_buffer(Cow::Owned(vectors), config)
    }

    fn build_buffer(vectors: Cow<'_, [f32]>, config: VamanaConfig) -> Result<Self> {
        #[cfg(feature = "parallel")]
        {
            match build_pool() {
                Some(pool) => pool.install(|| Self::build_on_current_pool(vectors, config)),
                None => Self::build_on_current_pool(vectors, config),
            }
        }
        #[cfg(not(feature = "parallel"))]
        {
            Self::build_on_current_pool(vectors, config)
        }
    }

    fn build_on_current_pool(vectors: Cow<'_, [f32]>, config: VamanaConfig) -> Result<Self> {
        config.validate()?;
        if vectors.is_empty() {
            return Err(VamanaError::EmptyInput);
        }
        if !vectors.len().is_multiple_of(config.dimensions) {
            return Err(VamanaError::DimensionMismatch {
                expected: config.dimensions,
                actual: vectors.len() % config.dimensions,
            });
        }
        require_finite(&vectors, "build vectors")?;
        let num_vectors = vectors.len() / config.dimensions;
        if num_vectors > u32::MAX as usize {
            return Err(VamanaError::TooManyVectors { count: num_vectors });
        }

        let (gs_codec, gs_codes) = train_codec_and_encode(&vectors, config.dimensions);

        let graph =
            VamanaGraph::build_sq8(&vectors, CodesView::Owned(&gs_codes), &gs_codec, &config)?;
        let dimensions = config.dimensions;

        Ok(Self {
            vectors: VectorStorage::Owned(vectors.into_owned()),
            graph,
            config,
            num_vectors,
            dimensions,
            tombstones: tombstone_words_for(num_vectors),
            tombstone_count: 0,
            ops_since_consolidation: 0,
            free_slots: Vec::new(),
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            search_visited: SearchVisitedPool::default(),
            gs_codec,
            gs_codes: CodeStore::Owned(gs_codes),
            last_applied_seq: None,
        })
    }

    /// Encode this index into the ADR-110 portable container.
    pub fn to_bytes(&self, external_ids: &[(u32, String)]) -> Result<Vec<u8>> {
        let vectors = khive_types::vector::encode_f32_le(self.vectors()?);
        let graph = encode_graph_lossless(&self.graph)?;
        let lifecycle = encode_lifecycle(
            &self.tombstones,
            &self.free_slots,
            self.graph.reverse_adjacency(),
            self.ops_since_consolidation,
        );

        let vectors_hash = *blake3::hash(&vectors).as_bytes();
        let graph_hash = *blake3::hash(&graph).as_bytes();
        let lifecycle_hash = *blake3::hash(&lifecycle).as_bytes();
        let fingerprint = V2CorpusFingerprint {
            vector_count: self.num_vectors as u64,
            dimensions: self.dimensions as u64,
            content_hash: vectors_hash,
        };
        let metadata = encode_v2_commit_full(
            &vectors_hash,
            &graph_hash,
            &lifecycle_hash,
            &fingerprint,
            self.num_vectors,
            self.dimensions,
            self.config.max_degree,
            self.config.search_list_size,
            self.config.alpha,
            self.last_applied_seq,
            None,
            None,
        );

        let mut segments = vec![
            ("metadata.bin", metadata),
            ("vectors.bin", vectors),
            ("graph.bin", graph),
            ("lifecycle.bin", lifecycle),
        ];
        if !external_ids.is_empty() {
            segments.push(("portable_ids.bin", encode_portable_ids(self, external_ids)?));
        }
        encode_portable_container(&segments)
    }

    /// Decode an ADR-110 portable container into owned storage and its live ID mapping.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, Vec<(u32, String)>)> {
        let segments = parse_portable_container(bytes)?;
        let metadata = required_segment(&segments, "metadata.bin")?;
        let vectors = required_segment(&segments, "vectors.bin")?;
        let graph = required_segment(&segments, "graph.bin")?;
        let lifecycle = required_segment(&segments, "lifecycle.bin")?;

        let index = Self::from_v2_bytes(metadata, vectors, graph, lifecycle)?;
        let external_ids = match segments.get("portable_ids.bin") {
            Some(ids) => parse_portable_ids(ids, &index)?,
            None => Vec::new(),
        };
        Ok((index, external_ids))
    }

    fn from_v2_bytes(
        metadata: &[u8],
        vector_bytes: &[u8],
        graph_bytes: &[u8],
        lifecycle_bytes: &[u8],
    ) -> Result<Self> {
        let commit = parse_v2_commit(metadata)?;
        let vectors_hash = *blake3::hash(vector_bytes).as_bytes();
        if vectors_hash != commit.vectors_hash
            || *blake3::hash(graph_bytes).as_bytes() != commit.graph_hash
            || *blake3::hash(lifecycle_bytes).as_bytes() != commit.lifecycle_hash
        {
            return Err(VamanaError::invalid_format(
                "v2 segment checksum mismatch".into(),
            ));
        }
        if commit.fingerprint.vector_count != commit.index_meta.num_vectors as u64
            || commit.fingerprint.dimensions != commit.index_meta.dimensions as u64
            || commit.fingerprint.content_hash != vectors_hash
        {
            return Err(VamanaError::invalid_format(
                "v2 corpus fingerprint mismatch".into(),
            ));
        }

        let config = VamanaConfig {
            dimensions: commit.index_meta.dimensions,
            max_degree: commit.index_meta.max_degree,
            search_list_size: commit.index_meta.search_list_size,
            alpha: commit.index_meta.alpha,
        };
        config.validate()?;
        let num_vectors = commit.index_meta.num_vectors;
        let expected_floats = num_vectors
            .checked_mul(config.dimensions)
            .ok_or_else(|| VamanaError::invalid_format("v2 metadata overflow".into()))?;
        let expected_bytes = expected_floats
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| {
                VamanaError::invalid_format("vectors.bin byte length overflow".into())
            })?;
        if vector_bytes.len() != expected_bytes {
            return Err(VamanaError::invalid_format(format!(
                "vectors.bin byte length {} != expected {expected_bytes}",
                vector_bytes.len()
            )));
        }
        let vectors = khive_types::vector::decode_f32_le(vector_bytes)
            .map_err(|error| VamanaError::invalid_format(error.to_string()))?;
        require_finite(&vectors, "portable vectors")?;

        let mut graph = parse_graph(graph_bytes, config.max_degree, num_vectors)?;
        let parsed = parse_lifecycle(lifecycle_bytes, num_vectors, config.max_degree)?;
        validate_reverse_adjacency(&graph, &parsed.reverse_adj)?;
        graph.restore_reverse_adj(parsed.reverse_adj);

        let tombstone_count = parsed
            .tombstones
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum();
        if tombstone_count > num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "lifecycle.bin tombstone_count {tombstone_count} exceeds num_vectors {num_vectors}"
            )));
        }
        validate_free_slots(&parsed.free_slots, &parsed.tombstones, num_vectors)?;

        let (gs_codec, gs_codes) = train_codec_and_encode(&vectors, config.dimensions);
        Ok(Self {
            vectors: VectorStorage::Owned(vectors),
            graph,
            dimensions: config.dimensions,
            config,
            num_vectors,
            tombstones: parsed.tombstones,
            tombstone_count,
            ops_since_consolidation: parsed.ops_since_consolidation,
            free_slots: parsed.free_slots,
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            search_visited: SearchVisitedPool::default(),
            gs_codec,
            gs_codes: CodeStore::Owned(gs_codes),
            last_applied_seq: commit.last_applied_seq,
        })
    }

    /// Persist the index to `path` (a directory); writes `metadata.bin`, `graph.bin`, `vectors.bin`.
    /// Each replacement is staged in a fresh inode and renamed into place, so
    /// an existing mmap reader keeps its original vectors even across an overwrite.
    /// The v1 format has no tombstone state, so deleted indexes must use
    /// [`Self::save_atomic`] instead.
    #[cfg(feature = "mmap")]
    pub fn save(&self, path: &Path) -> Result<()> {
        self.reject_lossy_legacy_export()?;
        let vectors: &[u8] = cast_slice(self.vectors()?);
        let graph = encode_graph(&self.graph, self.config.max_degree)?;
        let metadata = encode_metadata(self);

        fs::create_dir_all(path)?;
        let publication_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.join(".checkpoint.lock"))?;
        publication_lock.lock()?;

        // Stage every payload before replacing any canonical name. The lock
        // coordinates with load/save_atomic; already-returned mmap readers do
        // not need to hold it because rename leaves their old inode intact.
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(3);
        let result = (|| -> Result<()> {
            for (name, bytes) in [
                ("vectors.bin", vectors),
                ("graph.bin", graph.as_slice()),
                ("metadata.bin", metadata.as_slice()),
            ] {
                let destination = path.join(name);
                let temporary = stage_legacy_replacement(&destination, bytes)?;
                staged.push((temporary, destination));
            }
            // The v1 metadata is not a checksum-bearing commit record, but
            // publishing it last keeps the format marker behind both segments.
            for (temporary, destination) in &staged {
                fs::rename(temporary, destination)?;
            }
            Ok(())
        })();
        for (temporary, _) in staged {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    /// Load an index from a directory previously written by [`VamanaIndex::save`]
    /// (v1 format) or [`VamanaIndex::save_atomic`] (v2 segmented format); the format is
    /// auto-detected from `metadata.bin`'s magic. Never rebuilds — a corrupt, torn, or
    /// absent index returns an error, leaving the recovery decision to the caller (see
    /// [`Self::load_or_build`] and crates/khive-vamana/docs/api/persistence.md#v2-crash-safe-save-load).
    #[cfg(feature = "mmap")]
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with_lock_hook(path, |lock| lock.lock_shared().map_err(Into::into))
    }

    /// Reader half of the publication protocol, and the seam the concurrency tests
    /// drive. `acquire_lock` receives the open (but unlocked) `.checkpoint.lock` and
    /// must return only once this reader holds a shared lock on it; production passes
    /// the blocking `lock_shared()` call.
    ///
    /// [`Self::save_atomic_with_lock_hook`] renames `metadata.bin` into place as the
    /// commit record and only then renames the four segment files, so a reader that
    /// opens the directory between those renames sees a new commit record against stale
    /// segments and fails the checksum gate. That window is deliberate for crash
    /// recovery, where it is reached once and the caller rebuilds. Under concurrent
    /// processes it is reached on *every* publication, and the rebuild each reader
    /// performs to recover from it publishes again — so the recovery path is also the
    /// amplifier. Taking a shared lock on the same file the writer holds exclusively
    /// puts the whole rename sequence outside anything a reader can observe.
    ///
    /// A writable historical directory joins the publication protocol by
    /// creating a lock before reading any segment. A read-only directory
    /// cannot create the file; that narrow fallback revalidates every segment
    /// identity after loading instead of accepting a mixed generation.
    /// A mapping keeps its original inode if a later publisher renames the path.
    #[cfg(feature = "mmap")]
    pub(super) fn load_with_lock_hook(
        path: &Path,
        acquire_lock: impl FnOnce(&File) -> Result<()>,
    ) -> Result<Self> {
        Self::load_with_lock_hooks(
            path,
            acquire_lock,
            |lock_path| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(lock_path)
            },
            || {},
        )
    }

    #[cfg(feature = "mmap")]
    pub(super) fn load_with_lock_hooks(
        path: &Path,
        acquire_lock: impl FnOnce(&File) -> Result<()>,
        create_lock: impl FnOnce(&Path) -> std::io::Result<File>,
        after_snapshot: impl FnOnce(),
    ) -> Result<Self> {
        match Self::open_publication_lock_with_creator(path, acquire_lock, create_lock, true)? {
            Some(_publication_guard) => Self::load_unlocked(path),
            None => Self::load_without_publication_lock(path, after_snapshot),
        }
    }

    /// Open `path`'s `.checkpoint.lock` and hand it to `acquire_lock`. A first
    /// reader creates the file atomically; if another reader or writer wins that
    /// creation race, reopen and lock it. Only `load` allows permission/read-only
    /// creation failures to fall back to validated lockless loading.
    #[cfg(feature = "mmap")]
    fn open_publication_lock(
        path: &Path,
        acquire_lock: impl FnOnce(&File) -> Result<()>,
    ) -> Result<Option<File>> {
        Self::open_publication_lock_with_creator(
            path,
            acquire_lock,
            |lock_path| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(lock_path)
            },
            false,
        )
    }

    #[cfg(feature = "mmap")]
    fn open_publication_lock_with_creator(
        path: &Path,
        acquire_lock: impl FnOnce(&File) -> Result<()>,
        create_lock: impl FnOnce(&Path) -> std::io::Result<File>,
        allow_read_only_fallback: bool,
    ) -> Result<Option<File>> {
        let lock_path = path.join(".checkpoint.lock");
        let open_existing = || OpenOptions::new().read(true).open(&lock_path);
        let lock = match open_existing() {
            Ok(lock) => lock,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match create_lock(&lock_path) {
                    Ok(lock) => lock,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        open_existing()?
                    }
                    Err(error)
                        if allow_read_only_fallback && is_read_only_lock_create_error(&error) =>
                    {
                        return Ok(None);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        acquire_lock(&lock)?;
        Ok(Some(lock))
    }

    #[cfg(feature = "mmap")]
    fn load_without_publication_lock(path: &Path, after_snapshot: impl FnOnce()) -> Result<Self> {
        let before = publication_file_snapshot(path)?;
        after_snapshot();
        let loaded = Self::load_unlocked(path)?;
        if before != publication_file_snapshot(path)? {
            return Err(VamanaError::invalid_format(
                "index publication changed during read-only unlocked load".into(),
            ));
        }
        Ok(loaded)
    }

    #[cfg(feature = "mmap")]
    fn load_unlocked(path: &Path) -> Result<Self> {
        let metadata_path = path.join("metadata.bin");
        let head = fs::read(&metadata_path)?;
        if head.len() >= 8 && &head[..8] == V2_COMMIT_MAGIC {
            return Self::load_v2_raw(path);
        }

        let meta = read_metadata(&metadata_path)?;
        let config = VamanaConfig {
            dimensions: meta.dimensions,
            max_degree: meta.max_degree,
            search_list_size: meta.search_list_size,
            alpha: meta.alpha,
        };
        config.validate()?;

        let mut graph = read_graph(&path.join("graph.bin"), meta.max_degree, meta.num_vectors)?;

        if graph.node_count() != meta.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "graph node count {} != metadata num_vectors {}",
                graph.node_count(),
                meta.num_vectors
            )));
        }
        if graph.medoid() as usize >= meta.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "medoid {} >= num_vectors {}",
                graph.medoid(),
                meta.num_vectors
            )));
        }

        let expected_len_f32 = meta
            .num_vectors
            .checked_mul(meta.dimensions)
            .ok_or_else(|| VamanaError::invalid_format("metadata overflow".into()))?;
        let storage = mmap_vectors(&path.join("vectors.bin"), expected_len_f32)?;

        // v1 format does not persist reverse_adj; reconstruct O(N*R) from adjacency.
        // This must run before any tombstone call — lazy init would silently skip repair.
        graph.rebuild_reverse_adj_from_adjacency();

        let (gs_codec, gs_codes) = train_codec_and_encode(storage.as_slice()?, meta.dimensions);

        Ok(Self {
            vectors: storage,
            graph,
            config,
            num_vectors: meta.num_vectors,
            dimensions: meta.dimensions,
            tombstones: tombstone_words_for(meta.num_vectors),
            tombstone_count: 0,
            ops_since_consolidation: 0,
            free_slots: Vec::new(),
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            search_visited: SearchVisitedPool::default(),
            gs_codec,
            gs_codes: CodeStore::Owned(gs_codes),
            last_applied_seq: None,
        })
    }

    /// Crash-safe v2 save: stages `vectors.bin`, `graph.bin`, `lifecycle.bin`, and
    /// `codes.bin`, then renames and fsyncs `metadata.bin` before promoting the four
    /// segments. A crash after metadata promotion can leave mixed-generation live
    /// segments; raw [`Self::load`] rejects a checksum mismatch, while
    /// [`Self::load_or_build`] rebuilds from the caller's corpus. See
    /// [ADR-052 Amendment 1](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-052-ann-production-lifecycle.md#amendment-1-2026-09-27-v2-segment-promotion-after-metadata).
    /// Publication is serialized per directory; a candidate whose applied
    /// sequence is lower than the incumbent returns
    /// [`VamanaError::CheckpointSequenceRegression`] before staging any files. See
    /// crates/khive-vamana/docs/api/persistence.md#v2-crash-safe-save-load for the
    /// staging/fsync sequence.
    #[cfg(feature = "mmap")]
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        self.save_atomic_with_lock_hook(path, |lock| lock.lock().map_err(Into::into))
    }

    /// `acquire_lock` owns the entire publication-lock handshake: it receives the open
    /// (but unlocked) `.checkpoint.lock` file and must return only once this writer holds
    /// an exclusive lock on it. Production goes straight to the blocking `lock()` call with
    /// no probing — the same single syscall this always made. Tests substitute a hook that
    /// probes with `try_lock()` first to observe contention deterministically; that probing
    /// code never runs outside test builds.
    #[cfg(feature = "mmap")]
    pub(super) fn save_atomic_with_lock_hook(
        &self,
        path: &Path,
        acquire_lock: impl FnOnce(&File) -> Result<()>,
    ) -> Result<()> {
        fs::create_dir_all(path)?;
        let checkpoint = crate::checkpoint_io::CheckpointDirectory::open(path)?;
        let publication_lock = checkpoint.open_lock()?;
        acquire_lock(&publication_lock)?;
        reject_checkpoint_sequence_regression(path, self.last_applied_seq)?;

        // Stage under .v2new so a crash before the metadata rename leaves the previous
        // segments (v1 or v2) intact and readable. Every staging operation is
        // relative to the pinned directory and rejects planted links.
        let vectors_data: &[u8] = cast_slice(self.vectors()?);
        checkpoint.stage("vectors.bin.v2new", vectors_data)?;

        let graph_data = encode_graph(&self.graph, self.config.max_degree)?;
        checkpoint.stage("graph.bin.v2new", &graph_data)?;

        // encode_graph caps the medoid's forward list at max_degree before serializing;
        // build reverse_adj from that same capped view so lifecycle.bin stays consistent
        // with graph.bin after restore.
        let capped_reverse_adj = capped_reverse_adjacency(self);

        let lifecycle_data = encode_lifecycle(
            &self.tombstones,
            &self.free_slots,
            &capped_reverse_adj,
            self.ops_since_consolidation,
        );
        checkpoint.stage("lifecycle.bin.v2new", &lifecycle_data)?;

        // codes.bin.v2new persists the SQ8 codec + codes so load never retrains.
        let codes_data = encode_codes_bin(&self.gs_codec, self.gs_codes.view());
        checkpoint.stage("codes.bin.v2new", &codes_data)?;

        let vectors_hash = blake3::hash(vectors_data);
        let graph_hash = blake3::hash(&graph_data);
        let lifecycle_hash = blake3::hash(&lifecycle_data);
        let codes_hash = blake3::hash(&codes_data);

        let content_hash = *vectors_hash.as_bytes();
        let fp = V2CorpusFingerprint {
            vector_count: self.num_vectors as u64,
            dimensions: self.dimensions as u64,
            content_hash,
        };

        // A checkpoint publication is an event even when every semantic byte
        // matches its predecessor. Include a fresh nonce in the commit record
        // so long-lived mmap readers can distinguish the new file generation
        // and release the unlinked predecessor (#2081).
        let publication_nonce = *uuid::Uuid::new_v4().as_bytes();
        let metadata_data = encode_v2_commit_full(
            vectors_hash.as_bytes(),
            graph_hash.as_bytes(),
            lifecycle_hash.as_bytes(),
            &fp,
            self.num_vectors,
            self.dimensions,
            self.config.max_degree,
            self.config.search_list_size,
            self.config.alpha,
            self.last_applied_seq,
            Some(codes_hash.as_bytes()),
            Some(&publication_nonce),
        );
        checkpoint.publish_v2(&metadata_data)?;

        Ok(())
    }

    /// Fingerprint-gated restore. On a corpus fingerprint match, loads all segments
    /// (including lifecycle state) in O(N) without rebuilding `reverse_adj`. On mismatch
    /// or a missing/corrupt v2 commit, rebuilds from `corpus_vectors` (the caller's raw
    /// flat f32 slice) using `fallback_config` or the commit's saved config, then
    /// persists via `save_atomic`. Full decision tree:
    /// crates/khive-vamana/docs/api/persistence.md#v2-crash-safe-save-load.
    #[cfg(feature = "mmap")]
    pub fn load_or_build(
        path: &Path,
        corpus_vectors: &[f32],
        fallback_config: VamanaConfig,
    ) -> Result<Self> {
        Self::load_or_build_with_sequence(path, corpus_vectors, fallback_config, None)
    }

    /// Fingerprint-gated restore with the write-log sequence to use if the index
    /// must be rebuilt and persisted. The caller owns the log and must supply the
    /// highest sequence reflected in `corpus_vectors`.
    #[cfg(feature = "mmap")]
    pub fn load_or_build_with_sequence(
        path: &Path,
        corpus_vectors: &[f32],
        fallback_config: VamanaConfig,
        rebuild_last_applied_seq: Option<u64>,
    ) -> Result<Self> {
        // This API may publish a rebuilt checkpoint, so establish the directory
        // before the first lock probe. A clean first run has no directory yet.
        fs::create_dir_all(path)?;
        let metadata_path = path.join("metadata.bin");

        // Every read below — the commit record and all four segments — happens under a
        // shared publication lock, so it cannot observe `save_atomic`'s rename sequence
        // half-applied. Without it a concurrent publication is seen as a new commit
        // record against stale segments, the checksum gate rejects it, and the recovery
        // is `rebuild_and_persist` — which publishes, tearing the next reader. See
        // `load_with_lock_hook`.
        let mut publication_guard =
            Self::open_publication_lock(path, |lock| lock.lock_shared().map_err(Into::into))?;

        // Takes the guard because `save_atomic` acquires the same file exclusively and
        // file locks are not reentrant across descriptors: the read lock is released
        // before anything publishes.
        let rebuild_and_persist = |guard: &mut Option<File>, config| {
            #[cfg(test)]
            checkpoint_allocation_tests::record_rebuild();
            guard.take();
            let mut index = Self::rebuild_from_corpus(corpus_vectors, config)?;
            index.set_last_applied_seq(rebuild_last_applied_seq);
            index.save_atomic(path)?;
            Ok(index)
        };

        let metadata_bytes = match fs::read(&metadata_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Clean first run: no saved state at all. Remove any stale staged segments.
                for suffix in &[
                    "vectors.bin.v2new",
                    "graph.bin.v2new",
                    "lifecycle.bin.v2new",
                ] {
                    let _ = fs::remove_file(path.join(suffix));
                }
                return rebuild_and_persist(&mut publication_guard, fallback_config);
            }
            Err(e) => return Err(e.into()),
        };

        if metadata_bytes.len() < 8 {
            return rebuild_and_persist(&mut publication_guard, fallback_config);
        }

        if &metadata_bytes[..8] == V2_COMMIT_MAGIC {
            let commit = match parse_v2_commit(&metadata_bytes) {
                Ok(c) => c,
                Err(_) => {
                    return rebuild_and_persist(&mut publication_guard, fallback_config);
                }
            };

            // Keep all temporary segment mappings inside the read phase. A
            // corrupt snapshot releases them before rebuild_and_persist takes
            // the writer lock and replaces the incumbent files.
            let restored = (|| -> Result<Option<Self>> {
                let (vhash, _) = match hash_vectors_file(&path.join("vectors.bin")) {
                    Ok(d) => d,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                };
                let graph_data = match map_checkpoint_segment(&path.join("graph.bin")) {
                    Ok(d) => d,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                };
                let lifecycle_data = match map_checkpoint_segment(&path.join("lifecycle.bin")) {
                    Ok(d) => d,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                };

                let ghash = *blake3::hash(&graph_data).as_bytes();
                let lhash = *blake3::hash(&lifecycle_data).as_bytes();
                if vhash != commit.vectors_hash
                    || ghash != commit.graph_hash
                    || lhash != commit.lifecycle_hash
                {
                    return Ok(None);
                }

                // A checksum-invalid codes segment never reaches load_v2_fast.
                if let Some(expected) = commit.codes_hash {
                    let codes_ok = hash_file_mmap(&path.join("codes.bin"))
                        .map(|hash| hash == expected)
                        .unwrap_or(false);
                    if !codes_ok {
                        return Ok(None);
                    }
                }

                // Verify corpus fingerprint: dimensions, count, and content hash.
                let dim = commit.index_meta.dimensions;
                if dim == 0 || !corpus_vectors.len().is_multiple_of(dim) {
                    return Ok(None);
                }
                let live_count = corpus_vectors.len() / dim;
                let live_content_hash = *blake3::hash(cast_slice(corpus_vectors)).as_bytes();
                let fp_matches = commit.fingerprint.vector_count == live_count as u64
                    && commit.fingerprint.dimensions == dim as u64
                    && commit.fingerprint.content_hash == live_content_hash;
                if !fp_matches {
                    return Ok(None);
                }

                // Checksum-valid structural corruption also rebuilds, while
                // ordinary storage errors retain the strict error path.
                match Self::load_v2_fast(path, &lifecycle_data) {
                    Ok(index) => Ok(Some(index)),
                    Err(VamanaError::InvalidFormat { .. }) => Ok(None),
                    Err(e) => Err(e),
                }
            })()?;
            match restored {
                Some(index) => Ok(index),
                None => {
                    let config = VamanaConfig {
                        dimensions: commit.index_meta.dimensions,
                        max_degree: commit.index_meta.max_degree,
                        search_list_size: commit.index_meta.search_list_size,
                        alpha: commit.index_meta.alpha,
                    };
                    rebuild_and_persist(&mut publication_guard, config)
                }
            }
        } else if &metadata_bytes[..8] == METADATA_MAGIC {
            // V1 format: upgrade to v2. Remove any stale staged segments first.
            for suffix in &[
                "vectors.bin.v2new",
                "graph.bin.v2new",
                "lifecycle.bin.v2new",
            ] {
                let _ = fs::remove_file(path.join(suffix));
            }
            let mut index = Self::load(path)?;
            // Release the mmap before save_atomic overwrites the same files.
            index.ensure_owned()?;
            let mut stored_config = index.config().clone();
            let dimension = index.dimensions();
            let stored_bytes: &[u8] = cast_slice(index.vectors()?);
            let corpus_bytes: &[u8] = cast_slice(corpus_vectors);
            let corpus_matches = dimension == fallback_config.dimensions
                && dimension != 0
                && corpus_vectors.len().is_multiple_of(dimension)
                && index.num_vectors() == corpus_vectors.len() / dimension
                && stored_bytes == corpus_bytes;
            if !corpus_matches {
                stored_config.dimensions = fallback_config.dimensions;
                return rebuild_and_persist(&mut publication_guard, stored_config);
            }
            index.set_last_applied_seq(rebuild_last_applied_seq);
            // Same reason as in `rebuild_and_persist`: this branch publishes, so the
            // shared read lock must be gone before `save_atomic` asks for it exclusively.
            publication_guard.take();
            index.save_atomic(path)?;
            Ok(index)
        } else {
            // Unknown or garbage magic: treat as corrupt snapshot and rebuild.
            // VamanaIndex::load (direct v1 callers) remains strict; load_or_build always
            // recovers because the caller supplies a corpus and fallback config.
            rebuild_and_persist(&mut publication_guard, fallback_config)
        }
    }

    /// Load a committed v2 index from `path` without a corpus and without rebuilding;
    /// errors (never rebuilds) if no valid v2 commit is present. See
    /// crates/khive-vamana/docs/api/persistence.md#v2-crash-safe-save-load for the full
    /// decision tree shared with `load_or_build`.
    #[cfg(feature = "mmap")]
    fn load_v2_raw(path: &Path) -> Result<Self> {
        let metadata_bytes = fs::read(path.join("metadata.bin"))?;
        if metadata_bytes.len() < 8 || &metadata_bytes[..8] != V2_COMMIT_MAGIC {
            return Err(VamanaError::invalid_format(
                "metadata.bin is not a v2 commit".into(),
            ));
        }
        let commit = parse_v2_commit(&metadata_bytes)?;

        let (vectors_hash, _) = hash_vectors_file(&path.join("vectors.bin"))?;
        let graph_data = map_checkpoint_segment(&path.join("graph.bin"))?;
        let lifecycle_data = map_checkpoint_segment(&path.join("lifecycle.bin"))?;

        if vectors_hash != commit.vectors_hash
            || *blake3::hash(&graph_data).as_bytes() != commit.graph_hash
            || *blake3::hash(&lifecycle_data).as_bytes() != commit.lifecycle_hash
        {
            return Err(VamanaError::invalid_format(
                "v2 segment checksum mismatch".into(),
            ));
        }
        if let Some(expected) = commit.codes_hash {
            if hash_file_mmap(&path.join("codes.bin"))? != expected {
                return Err(VamanaError::invalid_format(
                    "v2 codes segment checksum mismatch".into(),
                ));
            }
        }

        Self::load_v2_fast(path, &lifecycle_data)
    }

    /// Load all v2 segments from `path` and restore lifecycle state from `lifecycle_data`.
    #[cfg(feature = "mmap")]
    fn load_v2_fast(path: &Path, lifecycle_data: &[u8]) -> Result<Self> {
        let meta_bytes = fs::read(path.join("metadata.bin"))?;
        let commit = parse_v2_commit(&meta_bytes)?;

        let config = VamanaConfig {
            dimensions: commit.index_meta.dimensions,
            max_degree: commit.index_meta.max_degree,
            search_list_size: commit.index_meta.search_list_size,
            alpha: commit.index_meta.alpha,
        };
        config.validate()?;

        let num_vectors = commit.index_meta.num_vectors;

        let dimensions = config.dimensions;
        let max_degree = config.max_degree;
        let mut graph = read_graph(&path.join("graph.bin"), max_degree, num_vectors)?;

        let expected_len_f32 = num_vectors
            .checked_mul(dimensions)
            .ok_or_else(|| VamanaError::invalid_format("v2 metadata overflow".into()))?;
        let storage = mmap_vectors(&path.join("vectors.bin"), expected_len_f32)?;

        // Parse lifecycle.bin and restore state directly (no O(N*R) rebuild).
        let lifecycle = parse_lifecycle(lifecycle_data, num_vectors, max_degree)?;

        // Structural validity (node count, bidirectional reverse_adj, tombstone count) —
        // see `validate_v2_structural` for why a checksum match alone isn't enough.
        let tombstone_count = validate_v2_structural(&graph, &lifecycle, num_vectors)?;

        graph.restore_reverse_adj(lifecycle.reverse_adj);

        // Codes segment: mmap `codes.bin` when the commit record carries its
        // checksum (extended format); otherwise retrain from the corpus — the
        // compatibility path for segments written before the codes segment
        // existed. Retraining touches every vector page; the extended format
        // exists precisely to avoid that on the steady-state load.
        let (gs_codec, gs_codes) = match commit.codes_hash {
            Some(_) => {
                let codes_path = path.join("codes.bin");
                let file = File::open(&codes_path)?;
                let byte_len = usize::try_from(file.metadata()?.len()).map_err(|_| {
                    VamanaError::invalid_format("codes.bin file size exceeds usize".into())
                })?;
                // SAFETY: read-only mapping; callers must not mutate or
                // truncate codes.bin while this index is alive (same contract
                // as the vectors.bin mapping).
                let mmap = unsafe { MmapOptions::new().len(byte_len).map(&file)? };
                let codec = parse_codes_bin(mmap.as_ref(), dimensions, num_vectors)?;
                (
                    codec,
                    CodeStore::Mmap {
                        mmap: std::sync::Arc::new(mmap),
                        dims: dimensions,
                        len: num_vectors,
                    },
                )
            }
            None => {
                let (codec, codes) = train_codec_and_encode(storage.as_slice()?, dimensions);
                (codec, CodeStore::Owned(codes))
            }
        };

        Ok(Self {
            vectors: storage,
            graph,
            config,
            num_vectors,
            dimensions,
            tombstones: lifecycle.tombstones,
            tombstone_count,
            ops_since_consolidation: lifecycle.ops_since_consolidation,
            free_slots: lifecycle.free_slots,
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            search_visited: SearchVisitedPool::default(),
            gs_codec,
            gs_codes,
            last_applied_seq: commit.last_applied_seq,
        })
    }

    /// Build a fresh VamanaIndex from `corpus_vectors` using the supplied `config`.
    /// Used when fingerprint mismatches, metadata is corrupt/missing, or on a clean first run.
    #[cfg(feature = "mmap")]
    fn rebuild_from_corpus(corpus_vectors: &[f32], config: VamanaConfig) -> Result<Self> {
        VamanaIndex::build(corpus_vectors, config)
    }

    /// Mean recall@k across `queries` vs. exact brute-force. Errors on empty, bad dim, or non-finite.
    pub fn recall_at_k(&self, queries: &[f32], k: usize) -> Result<f64> {
        if queries.is_empty() {
            return Err(VamanaError::EmptyInput);
        }
        if k == 0 {
            return Err(VamanaError::invalid_config(
                "k must be > 0 for recall_at_k".into(),
            ));
        }
        if !queries.len().is_multiple_of(self.dimensions) {
            return Err(VamanaError::DimensionMismatch {
                expected: self.dimensions,
                actual: queries.len() % self.dimensions,
            });
        }

        let vecs = self.vectors()?;
        let num_queries = queries.len() / self.dimensions;
        let live_count = self.num_vectors - self.tombstone_count;
        let denom = k.min(live_count) as f64;

        let tombstones = if self.tombstone_count > 0 {
            Some(self.tombstones.as_slice())
        } else {
            None
        };

        let total_recall: f64 = (0..num_queries).try_fold(0.0f64, |acc, qi| {
            let query = &queries[qi * self.dimensions..(qi + 1) * self.dimensions];
            let exact = exact_search(vecs, self.dimensions, query, k, tombstones);
            let ann = self.search(query, k)?;

            let exact_ids: std::collections::HashSet<u32> =
                exact.iter().map(|(id, _)| *id).collect();
            let ann_ids: std::collections::HashSet<u32> = ann.iter().map(|(id, _)| *id).collect();

            let overlap = exact_ids.intersection(&ann_ids).count() as f64;
            Ok::<f64, VamanaError>(acc + overlap / denom)
        })?;

        Ok(total_recall / num_queries as f64)
    }

    /// Serialise this index into a self-validating `VamanaSnapshot`.
    /// This v1 snapshot cannot represent tombstones; use [`Self::to_bytes`]
    /// for an index with deletions.
    pub fn to_snapshot(
        &self,
        namespace: impl Into<String>,
        model: impl Into<String>,
        fingerprint: CorpusFingerprint,
        external_ids: Vec<String>,
    ) -> Result<VamanaSnapshot> {
        self.reject_lossy_legacy_export()?;
        if external_ids.len() != self.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "external_ids length {} != num_vectors {}",
                external_ids.len(),
                self.num_vectors
            )));
        }
        let num_vectors_u64 = u64::try_from(self.num_vectors)
            .map_err(|_| VamanaError::invalid_format("num_vectors overflows u64".into()))?;
        let dimensions_u32 = u32::try_from(self.dimensions)
            .map_err(|_| VamanaError::invalid_format("dimensions overflows u32".into()))?;
        let max_degree_u32 = u32::try_from(self.config.max_degree)
            .map_err(|_| VamanaError::invalid_format("max_degree overflows u32".into()))?;
        let search_list_size_u32 = u32::try_from(self.config.search_list_size)
            .map_err(|_| VamanaError::invalid_format("search_list_size overflows u32".into()))?;
        // Cap the medoid's adjacency at max_degree before serializing.
        // The medoid-pin in insert() may transiently allow the medoid to exceed
        // max_degree (by one edge per orphan-pinned insert; K consecutive inserts
        // can accumulate K overflow edges). Capping here ensures the snapshot
        // satisfies from_snapshot()'s degree constraint.
        let medoid = self.graph.medoid();
        let max_degree_usize = self.config.max_degree;
        let adjacency: Vec<Vec<u32>> = self
            .graph
            .adjacency()
            .iter()
            .enumerate()
            .map(|(i, neighbors)| {
                if i == medoid as usize && neighbors.len() > max_degree_usize {
                    neighbors[..max_degree_usize].to_vec()
                } else {
                    neighbors.clone()
                }
            })
            .collect();
        Ok(VamanaSnapshot {
            format: VAMANA_SNAPSHOT_FORMAT.to_string(),
            version: VAMANA_SNAPSHOT_VERSION,
            namespace: namespace.into(),
            model: model.into(),
            fingerprint,
            index: VamanaIndexSnapshot {
                num_vectors: num_vectors_u64,
                dimensions: dimensions_u32,
                max_degree: max_degree_u32,
                search_list_size: search_list_size_u32,
                alpha: self.config.alpha,
                medoid,
                adjacency,
                vectors: self.vectors()?.to_vec(),
            },
            external_ids,
        })
    }

    fn reject_lossy_legacy_export(&self) -> Result<()> {
        if self.tombstone_count > 0 {
            return Err(VamanaError::invalid_format(
                "v1 save/to_snapshot cannot represent tombstones; use save_atomic or to_bytes"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Reconstruct a `VamanaIndex` from a `VamanaSnapshot`.
    pub fn from_snapshot(snapshot: &VamanaSnapshot) -> Result<Self> {
        if snapshot.format != VAMANA_SNAPSHOT_FORMAT {
            return Err(VamanaError::invalid_format(format!(
                "unsupported Vamana snapshot format: {}",
                snapshot.format
            )));
        }
        if snapshot.version != VAMANA_SNAPSHOT_VERSION {
            return Err(VamanaError::invalid_format(format!(
                "unsupported Vamana snapshot version: {}",
                snapshot.version
            )));
        }
        let ix = &snapshot.index;
        let num_vectors = usize::try_from(ix.num_vectors)
            .map_err(|_| VamanaError::invalid_format("num_vectors overflow".into()))?;
        let dimensions = usize::try_from(ix.dimensions)
            .map_err(|_| VamanaError::invalid_format("dimensions overflow".into()))?;
        let max_degree = usize::try_from(ix.max_degree)
            .map_err(|_| VamanaError::invalid_format("max_degree overflow".into()))?;
        let search_list_size = usize::try_from(ix.search_list_size)
            .map_err(|_| VamanaError::invalid_format("search_list_size overflow".into()))?;

        if snapshot.external_ids.len() != num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "external_ids length {} != num_vectors {}",
                snapshot.external_ids.len(),
                num_vectors
            )));
        }
        if ix.adjacency.len() != num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "adjacency length {} != num_vectors {}",
                ix.adjacency.len(),
                num_vectors
            )));
        }
        let expected_floats = num_vectors
            .checked_mul(dimensions)
            .ok_or_else(|| VamanaError::invalid_format("snapshot vector length overflow".into()))?;
        if ix.vectors.len() != expected_floats {
            return Err(VamanaError::invalid_format(
                "snapshot vector data length mismatch".into(),
            ));
        }
        require_finite(&ix.vectors, "snapshot vectors")?;

        let config = VamanaConfig {
            dimensions,
            max_degree,
            search_list_size,
            alpha: ix.alpha,
        };
        config.validate()?;

        let mut graph = VamanaGraph::new(num_vectors, ix.medoid)?;
        for (node, neighbors) in ix.adjacency.iter().enumerate() {
            if neighbors.len() > max_degree {
                return Err(VamanaError::invalid_format(format!(
                    "node {node} degree {} exceeds max_degree {max_degree}",
                    neighbors.len()
                )));
            }
            for &nb in neighbors {
                if nb as usize >= num_vectors {
                    return Err(VamanaError::invalid_format(format!(
                        "neighbor {nb} >= num_vectors {num_vectors}"
                    )));
                }
                if nb as usize == node {
                    return Err(VamanaError::invalid_format(format!(
                        "self-loop at node {node}"
                    )));
                }
            }
            // Reject duplicate neighbors.
            let mut sorted = neighbors.clone();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            if sorted.len() != before {
                return Err(VamanaError::invalid_format(format!(
                    "snapshot node {node} has duplicate neighbors"
                )));
            }
            graph.adjacency_mut_for_load()[node] = neighbors.clone();
        }

        // v1 snapshot format does not persist reverse_adj; reconstruct O(N*R) from adjacency.
        // This must run before any tombstone call — lazy init would silently skip repair.
        graph.rebuild_reverse_adj_from_adjacency();

        let (gs_codec, gs_codes) = train_codec_and_encode(&ix.vectors, dimensions);

        Ok(Self {
            vectors: VectorStorage::Owned(ix.vectors.clone()),
            graph,
            config,
            num_vectors,
            dimensions,
            tombstones: tombstone_words_for(num_vectors),
            tombstone_count: 0,
            ops_since_consolidation: 0,
            free_slots: Vec::new(),
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            search_visited: SearchVisitedPool::default(),
            gs_codec,
            gs_codes: CodeStore::Owned(gs_codes),
            last_applied_seq: None,
        })
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn portable_vector_segment_is_fixed_little_endian() {
        let vectors = [1.0, -2.5, 0.25, 2.0];
        let config = VamanaConfig::with_dimensions(2)
            .with_max_degree(1)
            .with_search_list_size(2);
        let index = VamanaIndex::build(&vectors, config).unwrap();
        let bytes = index.to_bytes(&[]).unwrap();
        let segments = parse_portable_container(&bytes).unwrap();
        assert_eq!(
            required_segment(&segments, "vectors.bin").unwrap(),
            [0, 0, 0x80, 0x3f, 0, 0, 0x20, 0xc0, 0, 0, 0x80, 0x3e, 0, 0, 0, 0x40,]
        );
        let (restored, ids) = VamanaIndex::from_bytes(&bytes).unwrap();
        assert!(ids.is_empty());
        assert_eq!(restored.vectors().unwrap(), vectors);
    }
}
