//! HNSW index implementation.
//!
//! Core structure with insert, delete, search, and rebuild operations.
//! Nodes use dense usize indices internally; NodeId (128-bit) conversion happens at the API boundary.

mod build_batch;
mod index_impl;
mod insert;
mod memory;
mod neighbors;
mod quantized;
mod rebuild;
mod search;

pub use index_impl::HnswIndex;
