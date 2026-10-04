//! Filesystem primitives shared across khive crates.
//!
//! This crate depends on no other khive crate. Its modules are `fd_relative`, which holds
//! descriptor-relative helpers and the thread `errno` accessors, and `directory_walk`, which
//! walks a path one pinned directory at a time under a caller-supplied link policy. Both are
//! Unix only and do not exist on other platforms.

#[cfg(unix)]
pub mod directory_walk;
#[cfg(unix)]
pub mod fd_relative;
