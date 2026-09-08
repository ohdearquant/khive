//! Two processes on one segment directory: one publishing, one loading.
//!
//! `save_atomic` renames `metadata.bin` into place as the commit record and only
//! then renames the four segment files. A reader admitted between those renames
//! reads a new commit record against stale segments and fails the checksum gate —
//! observed in production as `lifecycle.bin rev_num_nodes N != num_vectors M` and
//! `v2 codes segment checksum mismatch`, with the recovery from it being a full
//! rebuild that publishes again.
//!
//! Threads in one process would not reproduce the shape that matters: the file
//! lock this relies on is a cross-process primitive, and the deployment that
//! showed the defect is forty processes on one index root. So the writer is a
//! real child process.
//!
//! The window is wide enough to land in without any help, because the commit
//! record is fsynced between its rename and the segment renames and an fsync is
//! milliseconds. Measured with the reader lock reverted: 32 torn reads across
//! 234 loads.

#![cfg(all(feature = "mmap", feature = "parallel"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use khive_vamana::{VamanaConfig, VamanaIndex};

const DIMS: usize = 16;
const VECTORS: usize = 400;
const PUBLICATIONS: u64 = 6;
const DIR_ENV: &str = "KHIVE_VAMANA_PUBLICATION_DIR";

/// A different corpus per publication. This matters: if every publication wrote
/// byte-identical segments, the stale segments left in the window would still
/// match the new commit record's hashes and the torn state would be
/// indistinguishable from a healthy one — the test would pass whether or not the
/// reader held the lock.
fn corpus(round: u64) -> Vec<f32> {
    // Deterministic and non-degenerate: a fixed LCG, unit-ish spread.
    let mut state = 0x2545_F491_4F6C_DD1Du64 ^ round.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (0..VECTORS * DIMS)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f32 / (1u64 << 31) as f32) - 0.5
        })
        .collect()
}

fn config() -> VamanaConfig {
    VamanaConfig::with_dimensions(DIMS)
        .with_max_degree(16)
        .with_search_list_size(32)
}

fn build(seq: u64) -> VamanaIndex {
    let mut index = VamanaIndex::build(&corpus(seq), config()).expect("build");
    index.set_last_applied_seq(Some(seq));
    index
}

/// The publishing half, run as a child process. Ignored so a normal test run
/// never picks it up; the reader test invokes it by name.
#[test]
#[ignore = "child process of publication_never_exposes_a_torn_segment_set"]
fn publication_child() {
    let dir = PathBuf::from(std::env::var(DIR_ENV).expect("child needs the segment directory"));
    for seq in 2..=PUBLICATIONS + 1 {
        build(seq).save_atomic(&dir).expect("child publication");
    }
}

fn spawn_child(dir: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().expect("current exe"))
        .args(["--exact", "publication_child", "--ignored", "--nocapture"])
        .env(DIR_ENV, dir)
        .spawn()
        .expect("spawn publishing child")
}

/// A reader must never observe a new commit record over stale segments while
/// another process publishes.
///
/// The second assertion is the control: a reader that never spanned a
/// publication boundary would pass the first assertion without testing anything,
/// so the test also requires that it observed the sequence advance.
#[test]
fn publication_never_exposes_a_torn_segment_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    build(1).save_atomic(dir.path()).expect("seed publication");

    let mut child = spawn_child(dir.path());

    let mut observed_sequences = std::collections::BTreeSet::new();
    let mut failures: Vec<String> = Vec::new();
    let mut loads = 0u64;
    loop {
        match VamanaIndex::load(dir.path()) {
            Ok(index) => {
                observed_sequences.insert(index.last_applied_seq());
            }
            Err(error) => failures.push(error.to_string()),
        }
        loads += 1;
        if let Some(status) = child.try_wait().expect("child status") {
            assert!(status.success(), "publishing child failed: {status}");
            break;
        }
        assert!(
            loads < 200_000,
            "child never exited; loads={loads}, failures={}",
            failures.len()
        );
    }

    assert!(
        failures.is_empty(),
        "a reader observed {} torn segment set(s) across {loads} loads; first: {}",
        failures.len(),
        failures.first().map(String::as_str).unwrap_or("<none>")
    );
    assert!(
        observed_sequences.len() >= 2,
        "control: the reader never spanned a publication, so this proved nothing \
         (loads={loads}, sequences observed={observed_sequences:?})"
    );
}
