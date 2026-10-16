use super::{
    should_mark_cold, CandidateDispatch, DirectoryKind, DiscoveredKind, DiscoveryIndex,
    TrackedDirectory, COLD_FILE_PROBES_PER_TICK, COLD_STALE_POPS_PER_TICK,
    DIRECTORY_FORCE_RESCAN_PROBES, DIRECTORY_PROBES_PER_TICK,
    DIRECTORY_REFRESH_FAILURES_BEFORE_WARN, FILE_COLD_AGE, FILE_ERROR_POLLS_BEFORE_COLD,
    FILE_UNCHANGED_POLLS_BEFORE_COLD, FILE_UNCHANGED_POLLS_WITHOUT_MTIME,
};
use crate::mirror::ingest::MirrorStats;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn mark_cold(discovery: &mut DiscoveryIndex, path: &Path) {
    let file = discovery.files.get_mut(path).expect("tracked file");
    file.cold = true;
    file.unchanged_polls = FILE_UNCHANGED_POLLS_BEFORE_COLD;
    discovery.hot_files.remove(path);
    discovery.enqueue_cold(path);
}

include!("service_discovery_tests/scheduler_probe_tests.rs");

include!("service_discovery_tests/root_identity_tests.rs");

include!("service_discovery_tests/cold_threshold_tests.rs");

include!("service_discovery_tests/removal_and_retry_tests.rs");

include!("service_discovery_tests/progress_and_recovery_tests.rs");

include!("service_discovery_tests/demotion_and_warning_tests.rs");

include!("service_discovery_tests/pinned_identity_tests.rs");
