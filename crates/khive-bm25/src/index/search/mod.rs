//! BM25 search: brute-force SIMD and Block-Max WAND paths.

mod context;
mod cursor;
mod engine;
mod helpers;
mod idf;
mod simd;

pub use context::SearchContext;

pub(crate) use context::{HeapEntry, ShallowBlockInfo, TERMINATED_DOC};

#[cfg(test)]
mod tests;
