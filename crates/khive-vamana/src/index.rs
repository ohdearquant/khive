//! Vamana index: build, search, save/load, and snapshot serialization.

#[cfg(all(test, feature = "mmap"))]
use std::{fs, path::Path};

#[cfg(test)]
use crate::config::VamanaConfig;
#[cfg(test)]
use crate::graph::CodesView;
#[cfg(all(test, feature = "mmap"))]
use crate::graph::VamanaGraph;
use crate::{
    error::{Result, VamanaError},
    graph::VisitedSet,
};

#[cfg(feature = "mmap")]
const METADATA_MAGIC: &[u8; 8] = b"KHVVAMM1";
const GRAPH_MAGIC: &[u8; 8] = b"KHVVAMG1";

// v2 commit-record magic written into metadata.bin by save_atomic.
const V2_COMMIT_MAGIC: &[u8; 8] = b"KHVVAMG2";

// lifecycle.bin magic for v2 persistence.
const LIFECYCLE_MAGIC: &[u8; 8] = b"KHVVLIF1";
const PORTABLE_MAGIC: &[u8; 8] = b"KHVVAMAC";
const PORTABLE_VERSION: u32 = 1;
const PORTABLE_IDS_MAGIC: &[u8; 8] = b"KHVEXTID";
const PORTABLE_IDS_VERSION: u32 = 1;

/// Default ops-since-consolidation threshold (ADR-052 §2, OQ5 resolution).
const DEFAULT_CONSOLIDATION_TAU: usize = 40_000;

mod search_visited;
use search_visited::SearchVisitedPool;

mod snapshot_types;
#[cfg(all(test, feature = "mmap", windows))]
use snapshot_types::publication_file_identity;
#[cfg(feature = "mmap")]
use snapshot_types::{
    encode_codes_bin, is_read_only_lock_create_error, parse_codes_bin, publication_file_snapshot,
};
use snapshot_types::{CodeStore, VectorStorage};
pub use snapshot_types::{
    CorpusFingerprint, PersistedFingerprint, VamanaIndexSnapshot, VamanaSnapshot,
    VAMANA_SNAPSHOT_FORMAT, VAMANA_SNAPSHOT_VERSION,
};
#[cfg(test)]
use snapshot_types::{VamanaIndexSnapshotRaw, VamanaSnapshotRaw};
#[cfg(all(test, feature = "mmap"))]
use snapshot_types::{CODES_HEADER_LEN, CODES_MAGIC};

mod index_struct;
#[cfg(feature = "parallel")]
use index_struct::build_pool;
pub use index_struct::VamanaIndex;
#[cfg(all(test, feature = "parallel"))]
use index_struct::{build_thread_count, resolve_build_threads};
use index_struct::{require_finite, train_codec_and_encode, IndexMetadata};

mod index_load_save;
mod index_mutation;

mod repair;
#[cfg(feature = "mmap")]
use repair::capped_reverse_adjacency;
use repair::{
    elect_medoid, exact_search, set_tombstone_bit, tombstone_words_for, validate_reverse_adjacency,
    wolverine_repair,
};

mod portable_codec;
use portable_codec::{
    encode_portable_container, encode_portable_ids, parse_portable_container, parse_portable_ids,
    required_segment,
};

mod v2_commit;
#[cfg(all(test, feature = "mmap"))]
use v2_commit::write_v2_commit_full;
use v2_commit::{
    encode_lifecycle, encode_v2_commit_full, parse_v2_commit, validate_free_slots, ParsedLifecycle,
    V2CorpusFingerprint,
};
#[cfg(feature = "mmap")]
use v2_commit::{reject_checkpoint_sequence_regression, validate_v2_structural};

mod graph_io;
pub use graph_io::corpus_content_hash;
#[cfg(all(test, feature = "mmap"))]
use graph_io::VECTOR_HASH_CHUNK_BYTES;
#[cfg(feature = "mmap")]
use graph_io::{
    encode_graph, encode_metadata, hash_file_mmap, hash_vectors_file, mmap_vectors, read_graph,
    read_metadata, stage_legacy_replacement,
};
use graph_io::{encode_graph_lossless, parse_graph, parse_lifecycle};
#[cfg(feature = "mmap")]
pub use graph_io::{read_commit_fingerprint, read_commit_info, PersistedCommitInfo};

mod search;

#[cfg(test)]
use crate::graph::{greedy_search_inner, greedy_search_inner_sq8};

#[cfg(all(test, feature = "mmap"))]
#[path = "checkpoint_allocation_tests.rs"]
mod checkpoint_allocation_tests;

#[cfg(feature = "mmap")]
#[path = "checkpoint_segment.rs"]
mod checkpoint_segment;
#[cfg(feature = "mmap")]
use checkpoint_segment::{map_checkpoint_segment, MappedCheckpointSegment};

#[cfg(test)]
#[path = "index_perf_compat_tests.rs"]
mod perf_compat_tests;

#[cfg(test)]
#[path = "index_perf_tests.rs"]
mod perf_tests;

#[cfg(test)]
#[path = "index_search_visited_tests.rs"]
mod search_visited_tests;

// These unit tests remain a child module so they can exercise private helpers
// and the internal `VectorStorage` enum without public re-exports.
#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "index_maintenance_tests.rs"]
mod maintenance_tests;
