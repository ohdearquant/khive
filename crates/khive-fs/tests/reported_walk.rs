use std::error::Error as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use khive_fs::tree_discovery::{
    walk_tree_reported, ReportedWalkLimits, WalkFailureKind, WalkFile, WalkProgress, WalkReport,
};

include!("reported_walk/listing_and_limits_tests.rs");

include!("reported_walk/progress_and_errors_tests.rs");

include!("reported_walk/unix_identity_tests.rs");

include!("reported_walk/native_names_tests.rs");
