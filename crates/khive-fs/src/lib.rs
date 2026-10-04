//! Filesystem primitives shared across khive crates.
//!
//! This crate depends on no other khive crate. Its module `fd_relative` holds
//! descriptor-relative helpers and the thread `errno` accessors for Unix, and it does not
//! exist on other platforms. Its module `opened_file` reports the path behind an open file and
//! opens a file only when that path stays inside a root; it has an arm for every platform.

#[cfg(unix)]
pub mod fd_relative;
pub mod opened_file;
