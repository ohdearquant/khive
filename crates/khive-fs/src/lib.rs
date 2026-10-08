//! Filesystem primitives shared across khive crates.
//!
//! This crate depends on no other khive crate. Its modules are `fd_relative`, which holds
//! descriptor-relative helpers and the thread `errno` accessors, and `directory_walk`, which
//! walks a path one pinned directory at a time under a caller-supplied link policy and exposes
//! directory opens and descriptor-relative symlink reads. Both modules are Unix only.
//! The `opened_file` module reports the path behind an open file and provides a contained open
//! on every platform; its separate final-component no-follow regular-file open is Unix only.

#[cfg(unix)]
pub mod directory_walk;
#[cfg(unix)]
pub mod fd_relative;
pub mod opened_file;
