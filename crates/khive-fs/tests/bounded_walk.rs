#![cfg(unix)]

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fs;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_os = "linux")]
use khive_fs::fd_relative::{list_names, list_names_bounded, ListNamesError};
use khive_fs::tree_walk::{
    walk_tree, RelPath, WalkEntryKind, WalkError, WalkLimits, TREE_LINK_BUDGET,
};

include!("bounded_walk/listing_and_entry_limits_tests.rs");

include!("bounded_walk/depth_and_link_policy_tests.rs");

include!("bounded_walk/link_budget_and_root_identity_tests.rs");

include!("bounded_walk/replacement_and_link_targets_tests.rs");
