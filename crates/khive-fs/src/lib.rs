//! Filesystem primitives shared across khive crates.
//!
//! This crate depends on no other khive crate. Its modules are `fd_relative`, which holds
//! descriptor-relative helpers and the thread `errno` accessors, and `directory_walk`, which
//! walks a path one pinned directory at a time under a caller-supplied link policy and exposes
//! directory opens and descriptor-relative symlink reads. `tree_walk` adds bounded enumeration
//! below a pinned root, with caller filtering and optional within-root link resolution.
//! These three modules are Unix only.
//! The Unix `atomic_publish` module stages and publishes files within a held directory.
//! The `opened_file` module reports the path behind an open file and provides a contained open
//! on every platform; its separate final-component no-follow regular-file open is Unix only.
//! `tree_discovery` adds breadth-first discovery with progress and failure reports on every
//! platform, retaining parent-directory capabilities for selected files on Unix.
//! `canonical_path` resolves pathnames with missing suffixes on every platform by observing
//! the current topology; it does not grant a file or directory capability.

/// Complete create-only publication, preserving competing final entries.
pub mod atomic_create;
#[cfg(unix)]
pub mod atomic_publish;
/// Portable current-topology pathname resolution with missing suffixes.
pub mod canonical_path;
#[cfg(unix)]
pub mod directory_walk;
#[cfg(unix)]
pub mod fd_relative;
pub mod opened_file;
pub mod tree_discovery;
#[cfg(unix)]
pub mod tree_walk;
