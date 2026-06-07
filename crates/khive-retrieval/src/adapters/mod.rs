//! Adapters bridging khive-storage-traits backends to retrieval search traits.

mod storage;

pub use storage::{StorageKeywordSearch, StorageVectorSearch};
