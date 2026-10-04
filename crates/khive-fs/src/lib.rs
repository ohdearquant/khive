//! Filesystem primitives shared across khive crates.
//!
//! This crate depends on no other khive crate. Its one module, `fd_relative`, holds
//! descriptor-relative helpers and the thread `errno` accessors for Unix, and it does not
//! exist on other platforms.

#[cfg(unix)]
pub mod fd_relative;
