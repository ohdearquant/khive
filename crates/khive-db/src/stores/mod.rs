//! Per-substrate SQLite store implementations.
//!
//! Each module provides a concrete store struct implementing one or more
//! `khive-storage` capability traits against the shared connection pool.

pub mod blob;
pub mod entity;
pub mod event;
pub mod graph;
pub mod note;
pub mod sparse;
pub mod text;
pub mod vectors;
