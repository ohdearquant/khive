//! Filesystem primitives shared across khive crates.
//!
//! This crate depends on no other khive crate. Its modules are `fd_relative`, which holds
//! descriptor-relative helpers and the thread `errno` accessors, and `directory_walk`, which
//! walks a path one pinned directory at a time under a caller-supplied link policy. Both are
//! Unix only and do not exist on other platforms. Its module `opened_file` reports the path
//! behind an open file and opens a file only when that path stays inside a root; it has an arm
//! for every platform.

#[cfg(unix)]
pub mod directory_walk;
#[cfg(unix)]
pub mod fd_relative;
pub mod opened_file;
