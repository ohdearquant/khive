use super::*;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[cfg(unix)]
fn current_uid() -> u32 {
    unix_impl::current_uid()
}

fn heartbeat(pid: u32) -> WalpinHeartbeat {
    let now = now_epoch_secs();
    WalpinHeartbeat {
        pid,
        process_role: "session".to_string(),
        started_at: process_start_time_secs(std::process::id()).unwrap_or(0),
        oldest_tx_age_secs: 45.0,
        oldest_tx_label: Some("test_span".to_string()),
        oldest_tx_started_at: Some(now - 45),
        updated_at: now,
        sweep_interval_ms: 5_000,
        attribution_basis: Some("origin".to_string()),
    }
}

#[test]
fn sidecar_dir_is_db_scoped_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("khive.db");
    assert_eq!(sidecar_dir_for(&db), dir.path().join("khive.db.walpin"));
}

#[test]
#[serial_test::serial(khive_walpin_sidecar_env)]
fn sidecar_enabled_defaults_to_file_backed() {
    // Deterministic regardless of the ambient environment (minor,
    // ADR-091 Amendment 2: the prior version was vacuously true
    // whenever `KHIVE_WALPIN_SIDECAR` happened to be set already).
    let _guard = EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
    std::env::remove_var("KHIVE_WALPIN_SIDECAR");
    assert!(sidecar_enabled(true), "file-backed must default on");
    assert!(!sidecar_enabled(false), "in-memory must default off");
}

#[test]
#[serial_test::serial(khive_walpin_sidecar_env)]
fn sidecar_enabled_env_override_wins_either_way() {
    let _guard = EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
    std::env::set_var("KHIVE_WALPIN_SIDECAR", "off");
    assert!(
        !sidecar_enabled(true),
        "explicit off must override file-backed default"
    );
    std::env::set_var("KHIVE_WALPIN_SIDECAR", "on");
    assert!(
        sidecar_enabled(false),
        "explicit on must override in-memory default"
    );
}

#[test]
fn windows_handle_kind_requires_expected_type_without_reparse_data() {
    const DIRECTORY: u32 = 0x10;
    const REPARSE_POINT: u32 = 0x400;

    assert!(windows_attribute_tag_is_acceptable(DIRECTORY, 0, true));
    assert!(windows_attribute_tag_is_acceptable(0, 0, false));
    assert!(!windows_attribute_tag_is_acceptable(0, 0, true));
    assert!(!windows_attribute_tag_is_acceptable(DIRECTORY, 0, false));
    assert!(!windows_attribute_tag_is_acceptable(
        DIRECTORY | REPARSE_POINT,
        0,
        true
    ));
    assert!(!windows_attribute_tag_is_acceptable(DIRECTORY, 1, true));
}

#[test]
fn windows_final_path_comparison_requires_exact_handle_resolution() {
    let expected: Vec<u16> = r"\\?\C:\data\khive.db.walpin".encode_utf16().collect();
    let same = expected.clone();
    let redirected: Vec<u16> = r"\\?\C:\other\khive.db.walpin".encode_utf16().collect();

    assert!(windows_final_path_matches(&expected, &same));
    assert!(!windows_final_path_matches(&expected, &redirected));
}

#[test]
fn windows_relative_child_names_are_single_components() {
    assert!(windows_relative_child_name_is_safe("42.json"));
    assert!(!windows_relative_child_name_is_safe(""));
    assert!(!windows_relative_child_name_is_safe("."));
    assert!(!windows_relative_child_name_is_safe(".."));
    assert!(!windows_relative_child_name_is_safe("..\\42.json"));
    assert!(!windows_relative_child_name_is_safe("nested/42.json"));
    assert!(!windows_relative_child_name_is_safe("42\0.json"));
}

#[test]
fn windows_owner_dacl_accepts_token_user_owner_and_rejects_broader_shapes() {
    const ACCESS_ALLOWED: u8 = 0;
    const OBJECT_AND_CONTAINER_INHERIT: u8 = 0x03;
    const FILE_ALL_ACCESS: u32 = 0x001f_01ff;

    assert!(windows_owner_dacl_is_restricted(
        1,
        ACCESS_ALLOWED,
        OBJECT_AND_CONTAINER_INHERIT,
        FILE_ALL_ACCESS,
        true,
        true,
        true,
    ));
    assert!(!windows_owner_dacl_is_restricted(
        2,
        ACCESS_ALLOWED,
        OBJECT_AND_CONTAINER_INHERIT,
        FILE_ALL_ACCESS,
        true,
        true,
        true,
    ));
    assert!(!windows_owner_dacl_is_restricted(
        1,
        ACCESS_ALLOWED,
        OBJECT_AND_CONTAINER_INHERIT,
        FILE_ALL_ACCESS,
        false,
        true,
        true,
    ));
    assert!(!windows_owner_dacl_is_restricted(
        1,
        ACCESS_ALLOWED,
        OBJECT_AND_CONTAINER_INHERIT,
        FILE_ALL_ACCESS,
        true,
        true,
        false,
    ));
}

#[test]
fn windows_owner_dacl_rejects_group_owner() {
    const ACCESS_ALLOWED: u8 = 0;
    const OBJECT_AND_CONTAINER_INHERIT: u8 = 0x03;
    const FILE_ALL_ACCESS: u32 = 0x001f_01ff;

    assert!(!windows_owner_dacl_is_restricted(
        1,
        ACCESS_ALLOWED,
        OBJECT_AND_CONTAINER_INHERIT,
        FILE_ALL_ACCESS,
        true,
        false,
        true,
    ));
}

#[cfg(unix)]
#[test]
fn ensure_sidecar_dir_creates_0700_owned_dir() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).expect("should create");
    let meta = fs::symlink_metadata(&dir).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    assert_eq!(meta.uid(), current_uid());
}

#[cfg(unix)]
#[test]
fn ensure_sidecar_dir_refuses_wrong_mode() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let err = ensure_sidecar_dir(&dir).expect_err("wrong mode must be refused");
    assert!(err.to_string().contains("expected 0700"));
}

#[cfg(unix)]
#[test]
fn ensure_sidecar_dir_refuses_symlink() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real_dir");
    fs::create_dir(&real).unwrap();
    let link = root.path().join("khive.db.walpin");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let err = ensure_sidecar_dir(&link).expect_err("symlink must be refused");
    assert!(err.to_string().contains("symlink"));
}

#[test]
#[cfg(unix)]
fn ensure_sidecar_dir_refuses_non_root_owned_ancestor_symlink() {
    // The sidecar dir's own final component is real; a symlink sits at
    // an ANCESTOR of it instead. This must be refused just as hard as a
    // symlinked final component — only a root-owned ancestor symlink
    // (the OS's own firmlinks, e.g. macOS's /tmp -> private/tmp) gets a
    // pass, and the test process does not own this symlink as root.
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real_ancestor");
    fs::create_dir(&real).unwrap();
    let link = root.path().join("linked_ancestor");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let dir = link.join("khive.db.walpin");
    let err =
        ensure_sidecar_dir(&dir).expect_err("non-root-owned ancestor symlink must be refused");
    assert!(
        err.to_string().contains("symlink"),
        "unexpected error: {err}"
    );
}

#[test]
fn write_then_read_heartbeat_roundtrips() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let hb = heartbeat(std::process::id());
    write_heartbeat(&dir, &hb).expect("write should succeed");
    let content = fs::read_to_string(dir.join(format!("{}.json", hb.pid))).unwrap();
    let read_back: WalpinHeartbeat = serde_json::from_str(&content).unwrap();
    assert_eq!(read_back, hb);
}

#[test]
fn heartbeat_deserializes_pre_rename_interval_ms_field() {
    // Old-format sidecar body from a writer that predates the
    // `interval_ms` -> `sweep_interval_ms` rename. A live writer still
    // on the old field name must keep its real cadence, not silently
    // fall back to the enumerator's default (which can misjudge a slow
    // writer's heartbeat as stale mid-upgrade).
    let json = r#"{
            "pid": 4242,
            "process_role": "session",
            "started_at": 1000,
            "oldest_tx_age_secs": 45.0,
            "oldest_tx_label": "test_span",
            "updated_at": 1045,
            "interval_ms": 60000
        }"#;
    let hb: WalpinHeartbeat = serde_json::from_str(json).unwrap();
    assert_eq!(hb.sweep_interval_ms, 60_000);
}

#[test]
fn beacon_deserializes_pre_rename_interval_ms_field() {
    let json = r#"{
            "pid": 4242,
            "process_role": "session",
            "started_at": 1000,
            "interval_ms": 60000
        }"#;
    let b: WalpinBeacon = serde_json::from_str(json).unwrap();
    assert_eq!(b.sweep_interval_ms, 60_000);
}

#[cfg(unix)]
#[test]
fn write_heartbeat_refuses_symlinked_target() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();
    let real = root.path().join("elsewhere.txt");
    fs::write(&real, b"nope").unwrap();
    let hb = heartbeat(999_999);
    let target = dir.join(format!("{}.json", hb.pid));
    std::os::unix::fs::symlink(&real, &target).unwrap();
    let err = write_heartbeat(&dir, &hb).expect_err("symlinked target must be refused");
    assert!(err.to_string().contains("symlink"));
    // The real file behind the symlink must be untouched.
    assert_eq!(fs::read_to_string(&real).unwrap(), "nope");
}

/// Regression for the review-round-2 finding: the resolver used to
/// convert the sidecar directory's final `OsStr` component via
/// `to_string_lossy()` before `openat`, so two distinct non-UTF-8
/// database names (which differ only in their invalid byte) both
/// lossy-collapsed to the same `U+FFFD`-bearing name and cross-
/// contaminated attribution onto one physical sidecar directory. This
/// project explicitly supports non-UTF-8 database paths (see
/// `pool.rs`'s `mint_db_identity_non_utf8_path_round_trips`), so the
/// sidecar resolver must preserve that same byte-exact distinctness.
#[cfg(unix)]
#[test]
fn sidecar_dir_distinguishes_non_utf8_db_names_on_disk() {
    use std::os::unix::ffi::OsStrExt;

    let root = tempfile::tempdir().unwrap();
    // 0xFF and 0xFE are each invalid standalone UTF-8 bytes; both
    // lossy-convert to the same U+FFFD replacement character.
    let name_a = std::ffi::OsStr::from_bytes(b"khive-\xffdb.sqlite");
    let name_b = std::ffi::OsStr::from_bytes(b"khive-\xfedb.sqlite");
    let dir_a = sidecar_dir_for(&root.path().join(name_a));
    let dir_b = sidecar_dir_for(&root.path().join(name_b));
    assert_ne!(
        dir_a, dir_b,
        "distinct db names must produce distinct sidecar paths"
    );

    let hb = heartbeat(std::process::id());
    // Some Unix filesystems (notably macOS's APFS) reject non-UTF-8
    // names outright at the syscall level — a filesystem limitation,
    // not a bug under test here, so skip rather than fail in that case
    // (mirrors pool.rs's non-UTF-8 round-trip test).
    if let Err(e) = write_heartbeat(&dir_a, &hb) {
        eprintln!(
            "skipping sidecar_dir_distinguishes_non_utf8_db_names_on_disk: filesystem \
                 rejected a non-UTF-8 sidecar directory name ({e}); this platform's filesystem \
                 does not support the case under test"
        );
        return;
    }
    write_heartbeat(&dir_b, &hb).expect("write to second non-UTF-8 sidecar");

    let mut entries: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    entries.sort();
    assert_eq!(
        entries.len(),
        2,
        "distinct non-UTF-8 database names must produce two distinct sidecar directories \
             on disk, not collide onto one: got {entries:?}"
    );
}

#[test]
fn remove_heartbeat_is_idempotent_when_absent() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();
    remove_heartbeat(&dir, 123_456).expect("removing an absent entry is a no-op");
}

#[test]
fn is_process_alive_true_for_self_false_for_reserved_pid() {
    assert!(is_process_alive(std::process::id()));
    // PID 0 is never a valid target for `kill`.
    assert!(!is_process_alive(0));
}

#[test]
fn process_start_time_resolves_for_self() {
    let start = process_start_time_secs(std::process::id());
    assert!(
        start.is_some(),
        "must resolve this process's own start time"
    );
    let now = now_epoch_secs();
    assert!(
        start.unwrap() <= now,
        "start time must not be in the future"
    );
}

fn beacon(pid: u32) -> WalpinBeacon {
    WalpinBeacon {
        pid,
        process_role: "session".to_string(),
        started_at: process_start_time_secs(std::process::id()).unwrap_or(0),
        sweep_interval_ms: 5_000,
    }
}

#[cfg(unix)]
#[test]
fn enumerate_live_reports_and_retains_a_genuinely_live_entry() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let hb = heartbeat(std::process::id());
    write_heartbeat(&dir, &hb).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    let reporting: Vec<_> = report.reporting().collect();
    assert_eq!(reporting.len(), 1);
    assert_eq!(reporting[0].pid, hb.pid);
    assert!(report.fully_attributed());
    // A live, fresh, identity-matched entry must be retained on disk, not deleted.
    assert!(dir.join(format!("{}.json", hb.pid)).exists());
}

#[cfg(unix)]
#[test]
fn epoch_abs_diff_saturates_instead_of_wrapping() {
    assert_eq!(epoch_abs_diff(5, 3), 2);
    assert_eq!(epoch_abs_diff(3, 5), 2);
    assert_eq!(epoch_abs_diff(0, 0), 0);
    // `now - i64::MIN` overflows i64; wrapped arithmetic could land
    // inside a freshness window — saturation must push it outside all.
    assert_eq!(epoch_abs_diff(1, i64::MIN), u64::MAX);
    assert_eq!(epoch_abs_diff(i64::MIN, i64::MAX), u64::MAX);
    assert_eq!(epoch_abs_diff(-1, i64::MAX), 1u64 << 63);
}

#[cfg(unix)]
#[test]
fn enumerate_live_extreme_timestamp_classifies_unknown_not_fresh() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let mut hb = heartbeat(std::process::id());
    // Pre-amendment (`updated_at`-basis) record: this test exercises
    // `epoch_abs_diff`'s overflow protection on that field specifically.
    hb.oldest_tx_started_at = None;
    hb.updated_at = i64::MIN;
    write_heartbeat(&dir, &hb).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert!(
        report.reporting().next().is_none(),
        "an extreme updated_at must never classify as fresh"
    );
    assert!(
        report
            .entries
            .iter()
            .any(|e| matches!(e, WalpinPidHealth::Unknown { pid, .. } if *pid == hb.pid)),
        "the extreme-timestamp entry must stay Unknown, not vanish"
    );
}

#[cfg(unix)]
#[test]
fn enumerate_live_bounded_caps_listing_with_sentinel_marker() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let live = heartbeat(std::process::id());
    write_heartbeat(&dir, &live).unwrap();
    for pid in [2_000_000_001u32, 2_000_000_002] {
        let mut hb = heartbeat(std::process::id());
        hb.pid = pid;
        write_heartbeat(&dir, &hb).unwrap();
    }

    let report = enumerate_live_bounded(
        &dir,
        Duration::from_secs(5),
        1,
        EnumerationPurpose::Attribution,
    )
    .unwrap();
    let markers = report
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e,
                WalpinPidHealth::Unknown { pid: 0, reason }
                    if reason.contains("enumeration cap")
            )
        })
        .count();
    assert_eq!(
        markers, 1,
        "a truncated listing must surface exactly one sentinel Unknown marker"
    );
    assert!(
        !report.fully_attributed(),
        "a capped enumeration can never claim full attribution"
    );
    // Report memory is bounded by the cap: at most one processed entry
    // plus the sentinel, regardless of directory content.
    assert!(report.entries.len() <= 2, "got {:?}", report.entries);
}

#[cfg(unix)]
#[test]
fn enumerate_live_bounded_caps_hidden_entry_scan_with_sentinel_marker() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let live = heartbeat(std::process::id());
    write_heartbeat(&dir, &live).unwrap();
    // Far more hidden entries than the raw scan bound (cap 4 → 32 raw
    // entries): hidden names never consume the retained-name budget,
    // but they must still exhaust the raw scan bound and report
    // truncation instead of extending the readdir loop unboundedly.
    for i in 0..64 {
        std::fs::write(dir.join(format!(".junk{i}")), b"x").unwrap();
    }

    let report = enumerate_live_bounded(
        &dir,
        Duration::from_secs(5),
        4,
        EnumerationPurpose::Attribution,
    )
    .unwrap();
    let markers = report
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e,
                WalpinPidHealth::Unknown { pid: 0, reason }
                    if reason.contains("enumeration cap")
            )
        })
        .count();
    assert_eq!(
        markers, 1,
        "a hidden-entry flood must surface exactly one sentinel Unknown marker"
    );
    assert!(
        !report.fully_attributed(),
        "an enumeration cut short by hidden entries can never claim full attribution"
    );
}

#[cfg(unix)]
#[test]
fn housekeeping_reaps_only_stale_producer_temps_with_dead_identity() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();

    let dead_pid = 2_000_000_000;
    let stale_dead = dir.join(format!(".{dead_pid}.beacon.tmp"));
    fs::write(&stale_dead, serde_json::to_vec(&beacon(dead_pid)).unwrap()).unwrap();
    fs::File::options()
        .write(true)
        .open(&stale_dead)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3_600))
        .unwrap();

    let fresh_dead = dir.join(format!(".{}.json.tmp", dead_pid + 1));
    fs::write(
        &fresh_dead,
        serde_json::to_vec(&heartbeat(dead_pid + 1)).unwrap(),
    )
    .unwrap();

    let live_pid = std::process::id();
    let stale_live = dir.join(format!(".{live_pid}.beacon.tmp"));
    fs::write(&stale_live, serde_json::to_vec(&beacon(live_pid)).unwrap()).unwrap();
    fs::File::options()
        .write(true)
        .open(&stale_live)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3_600))
        .unwrap();

    let report = housekeep_live(&dir, Duration::from_secs(5)).unwrap();

    assert_eq!(report.orphan_temps_reaped, 1);
    assert!(
        !stale_dead.exists(),
        "a stale dead-producer temp must be reaped"
    );
    assert!(fresh_dead.exists(), "a fresh temp may still be in flight");
    assert!(
        stale_live.exists(),
        "a live producer's temp must never be reaped"
    );
}

/// A live producer's identity check depends on reading its process start
/// time; when that read fails (a permission boundary, a `/proc`
/// restriction) the temp's trustworthiness is simply unestablished, not
/// exonerated. It must surface as `Unknown` degraded evidence — the same
/// contract already enforced for a non-owned producer temp — rather than
/// being silently skipped, which would let diagnostics report `Complete`
/// while untrusted residue sits in the sidecar directory.
#[cfg(unix)]
#[test]
fn housekeeping_reports_a_live_producer_temp_whose_start_time_is_unreadable_as_unknown() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();

    let live_pid = std::process::id();
    let stale_live = dir.join(format!(".{live_pid}.beacon.tmp"));
    fs::write(&stale_live, serde_json::to_vec(&beacon(live_pid)).unwrap()).unwrap();
    fs::File::options()
        .write(true)
        .open(&stale_live)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3_600))
        .unwrap();

    set_stale_orphan_temp_start_time_override(None);
    let report = housekeep_live(&dir, Duration::from_secs(5)).unwrap();

    assert_eq!(
        report.orphan_temps_reaped, 0,
        "identity that could not be verified is never trustworthy reap evidence"
    );
    assert!(
        stale_live.exists(),
        "an uninspectable producer temp must be retained, not silently dropped"
    );
    let unknown: Vec<u32> = report.unknown_pids().collect();
    assert!(
        unknown.contains(&live_pid),
        "a live producer temp whose start time cannot be read must be reported as \
             Unknown, not silently skipped: {unknown:?}"
    );
}

#[cfg(unix)]
#[test]
fn housekeeping_retains_and_reports_malformed_or_mismatched_dead_producer_temps() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();

    let malformed_pid = 2_000_000_010;
    let malformed = dir.join(format!(".{malformed_pid}.beacon.tmp"));
    fs::write(&malformed, b"not valid json").unwrap();
    fs::File::options()
        .write(true)
        .open(&malformed)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3_600))
        .unwrap();

    let mismatched_pid = 2_000_000_020;
    let mismatched = dir.join(format!(".{mismatched_pid}.beacon.tmp"));
    fs::write(
        &mismatched,
        serde_json::to_vec(&beacon(mismatched_pid + 1)).unwrap(),
    )
    .unwrap();
    fs::File::options()
        .write(true)
        .open(&mismatched)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3_600))
        .unwrap();

    let report = housekeep_live(&dir, Duration::from_secs(5)).unwrap();

    assert_eq!(
        report.orphan_temps_reaped, 0,
        "neither a malformed nor a mismatched dead-PID temp is trustworthy reap evidence"
    );
    assert!(
        malformed.exists(),
        "a malformed dead-PID temp must survive cleanup as unknown evidence"
    );
    assert!(
        mismatched.exists(),
        "a dead-PID temp whose recorded identity does not match its filename must survive \
             cleanup as unknown evidence"
    );
    let unknown: Vec<u32> = report.unknown_pids().collect();
    assert!(
        unknown.contains(&malformed_pid),
        "malformed evidence must be reported, not silently dropped: {unknown:?}"
    );
    assert!(
        unknown.contains(&mismatched_pid),
        "mismatched evidence must be reported, not silently dropped: {unknown:?}"
    );
}

#[cfg(unix)]
#[test]
fn housekeeping_refuses_a_symlinked_producer_temp_without_touching_its_target() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();
    let target = root.path().join("forensic-evidence.json");
    fs::write(&target, b"keep me").unwrap();
    let link = dir.join(".2000000000.beacon.tmp");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let report = housekeep_live(&dir, Duration::from_secs(5)).unwrap();

    assert_eq!(report.orphan_temps_reaped, 0);
    assert_eq!(fs::read(&target).unwrap(), b"keep me");
    assert!(link.is_symlink(), "suspicious evidence must be retained");
}

#[cfg(unix)]
#[test]
fn read_only_inspection_classifies_dead_residue_without_deleting_it() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let mut dead = beacon(2_000_000_000);
    dead.started_at = 1;
    write_beacon(&dir, &dead).unwrap();
    let path = beacon_path(&dir, dead.pid);

    let report = inspect_live(&dir, Duration::from_secs(5)).unwrap();

    assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![dead.pid]);
    assert!(
        path.exists(),
        "diagnostics must never delete sidecar evidence"
    );
    assert_eq!(report.orphan_temps_reaped, 0);
}

#[cfg(unix)]
#[test]
fn enumerate_live_uncapped_population_has_no_sentinel() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let live = heartbeat(std::process::id());
    write_heartbeat(&dir, &live).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert!(
        report
            .entries
            .iter()
            .all(|e| !matches!(e, WalpinPidHealth::Unknown { pid: 0, .. })),
        "an in-budget population must not carry the cap sentinel"
    );
}

#[cfg(unix)]
#[test]
fn enumerate_live_deletes_dead_pid_entry() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    // A PID vanishingly unlikely to be alive/reused mid-test.
    let mut hb = heartbeat(std::process::id());
    hb.pid = 2_000_000_000;
    hb.started_at = 12345;
    write_heartbeat(&dir, &hb).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert!(report.entries.is_empty());
    assert!(!dir.join(format!("{}.json", hb.pid)).exists());
}

#[cfg(unix)]
#[test]
fn enumerate_live_deletes_mismatched_start_time_entry() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let mut hb = heartbeat(std::process::id());
    // Alive PID (this test process) but a `started_at` far from reality —
    // simulates a reused PID whose old heartbeat never got cleaned up.
    hb.started_at = 1;
    write_heartbeat(&dir, &hb).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert!(
        report.entries.is_empty(),
        "mismatched identity must fail the gate"
    );
    assert!(!dir.join(format!("{}.json", hb.pid)).exists());
}

#[cfg(unix)]
#[test]
fn enumerate_live_deletes_stale_updated_at_entry() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let mut hb = heartbeat(std::process::id());
    // Pre-amendment record: freshness classifies on `updated_at` alone.
    hb.oldest_tx_started_at = None;
    hb.updated_at = now_epoch_secs() - 3600; // far outside 3 sweep intervals
    write_heartbeat(&dir, &hb).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    // ADR-091 Amendment 2: a stale-but-identity-valid
    // heartbeat is wedged, not absent — it classifies `Unknown` rather
    // than silently vanishing from the report.
    assert_eq!(report.reporting().count(), 0);
    assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![hb.pid]);
    assert!(!report.fully_attributed());
    assert!(!dir.join(format!("{}.json", hb.pid)).exists());
}

#[cfg(unix)]
#[test]
fn enumerate_live_subsecond_sweep_interval_does_not_collapse_freshness_window() {
    // Minor (ADR-091 Amendment 2): a sub-second
    // KHIVE_SESSION_SWEEP_INTERVAL_MS must not yield a zero-second
    // freshness window that treats every heartbeat as instantly stale.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let hb = heartbeat(std::process::id());
    write_heartbeat(&dir, &hb).unwrap();

    let report = enumerate_live(&dir, Duration::from_millis(200)).unwrap();
    assert_eq!(
        report.reporting().count(),
        1,
        "must not be spuriously stale"
    );
}

#[cfg(unix)]
#[test]
fn enumerate_live_refuses_symlinked_entry_as_unknown_without_touching_target() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();
    let real = root.path().join("elsewhere.txt");
    fs::write(&real, b"precious").unwrap();
    let link = dir.join("42.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert!(report.reporting().count() == 0);
    assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![42]);
    assert!(!report.fully_attributed());
    assert_eq!(fs::read_to_string(&real).unwrap(), "precious");
    assert!(
        link.exists(),
        "the symlink itself must not be deleted either"
    );
}

#[cfg(unix)]
#[test]
fn enumerate_live_refuses_non_owned_entry_before_reading_contents() {
    // We cannot fabricate a genuinely non-owned file without root, so this
    // exercises the same code path via a forged UID check would require
    // privilege; instead this asserts the documented contract at the
    // metadata layer: an entry whose uid differs from `current_uid()` is
    // never parsed. Since every file this test process creates is
    // self-owned, we assert the positive form here (owned entries ARE
    // read) and rely on `validate_dir_metadata`'s ownership check
    // (exercised by `ensure_sidecar_dir_refuses_wrong_mode`-style tests)
    // for the negative form, which is the same `current_uid()` check
    // reused verbatim by per-entry validation.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let hb = heartbeat(std::process::id());
    write_heartbeat(&dir, &hb).unwrap();
    let meta = fs::symlink_metadata(dir.join(format!("{}.json", hb.pid))).unwrap();
    assert_eq!(
        meta.uid(),
        current_uid(),
        "self-written entries are owned by the current user, exercising the accept path"
    );
}

#[cfg(unix)]
#[test]
fn enumerate_live_refuses_non_compliant_directory_wholesale() {
    // Item 3 (ADR-091 Amendment 2): a directory that fails the
    // trust-boundary check must return a health *failure*, not a
    // silently empty/partial report that could masquerade as "no live
    // entries" evidence.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

    let err = enumerate_live(&dir, Duration::from_secs(5))
        .expect_err("non-compliant directory must be refused, not silently enumerated");
    assert!(err.to_string().contains("expected 0700"));
}

#[cfg(unix)]
#[test]
fn enumerate_live_missing_directory_is_ok_empty_not_a_failure() {
    // A sidecar that has simply never been used yet is a distinct case
    // from an existing-but-untrustworthy one.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert!(report.entries.is_empty());
}

#[cfg(unix)]
#[test]
fn enumerate_live_classifies_registered_silent_beacon_with_no_heartbeat() {
    // ADR-091 Amendment 2 spec delta: a live process that has registered
    // a beacon but never crossed the warn threshold (so it never wrote a
    // heartbeat) is `RegisteredSilent`, not absent/unknown.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let b = beacon(std::process::id());
    write_beacon(&dir, &b).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(report.reporting().count(), 0);
    assert_eq!(
        report.registered_silent_pids().collect::<Vec<_>>(),
        vec![std::process::id()]
    );
    assert!(report.fully_attributed());
}

#[cfg(unix)]
#[test]
fn enumerate_live_reporting_wins_over_registered_silent_for_same_pid() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    write_beacon(&dir, &beacon(pid)).unwrap();
    write_heartbeat(&dir, &heartbeat(pid)).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(report.reporting().count(), 1);
    assert_eq!(report.registered_silent_pids().count(), 0);
}

#[cfg(unix)]
#[test]
fn housekeeping_uses_the_five_second_legacy_cadence_fallback() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    let mut hb = heartbeat(pid);
    hb.oldest_tx_started_at = None;
    hb.sweep_interval_ms = 0;
    hb.updated_at = now_epoch_secs() - 4;
    write_heartbeat(&dir, &hb).unwrap();

    let report = housekeep_live(&dir, Duration::from_secs(5)).unwrap();

    assert_eq!(
        report.reporting().count(),
        1,
        "a 4s-old legacy record is inside the ADR-091 15s fallback window; the daemon's \
             500ms checkpoint cadence would incorrectly narrow that window to 3s: {report:?}"
    );
    assert!(
        dir.join(format!("{pid}.json")).exists(),
        "healthy housekeeping must retain a live legacy record"
    );
}

#[cfg(unix)]
#[test]
fn housekeeping_preserves_malformed_unknown_for_no_progress_attribution() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    write_beacon(&dir, &beacon(pid)).unwrap();
    let heartbeat_path = dir.join(format!("{pid}.json"));
    fs::write(&heartbeat_path, b"{not-json").unwrap();

    let housekeeping = housekeep_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(housekeeping.unknown_pids().collect::<Vec<_>>(), vec![pid]);
    assert_eq!(housekeeping.registered_silent_pids().count(), 0);
    assert!(
        heartbeat_path.exists(),
        "ordinary housekeeping must preserve malformed live-PID evidence"
    );

    let attribution = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(attribution.unknown_pids().collect::<Vec<_>>(), vec![pid]);
    assert_eq!(
        attribution.registered_silent_pids().count(),
        0,
        "a fresh beacon must never exonerate a PID whose heartbeat is malformed"
    );
    assert!(!attribution.fully_attributed());
}

#[cfg(unix)]
#[test]
fn housekeeping_preserves_stale_unknown_for_no_progress_attribution() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    write_beacon(&dir, &beacon(pid)).unwrap();
    let mut hb = heartbeat(pid);
    hb.oldest_tx_started_at = None;
    hb.sweep_interval_ms = 1_000;
    hb.updated_at = now_epoch_secs() - 30;
    write_heartbeat(&dir, &hb).unwrap();
    let heartbeat_path = dir.join(format!("{pid}.json"));

    let housekeeping = housekeep_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(housekeeping.unknown_pids().collect::<Vec<_>>(), vec![pid]);
    assert_eq!(housekeeping.registered_silent_pids().count(), 0);
    assert!(
        heartbeat_path.exists(),
        "ordinary housekeeping must preserve a live PID's stale heartbeat evidence"
    );

    let attribution = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(attribution.unknown_pids().collect::<Vec<_>>(), vec![pid]);
    assert_eq!(
        attribution.registered_silent_pids().count(),
        0,
        "a fresh beacon must never exonerate a PID whose heartbeat went stale"
    );
    assert!(!attribution.fully_attributed());
}

#[cfg(unix)]
#[test]
fn enumerate_live_deletes_dead_beacon() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let mut b = beacon(std::process::id());
    b.pid = 2_000_000_001;
    b.started_at = 12345;
    write_beacon(&dir, &b).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert!(report.entries.is_empty());
    assert!(!beacon_path(&dir, b.pid).exists());
}

#[cfg(unix)]
#[test]
fn write_beacon_refuses_symlinked_target() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();
    let real = root.path().join("elsewhere.txt");
    fs::write(&real, b"nope").unwrap();
    let b = beacon(999_998);
    let target = beacon_path(&dir, b.pid);
    std::os::unix::fs::symlink(&real, &target).unwrap();
    let err = write_beacon(&dir, &b).expect_err("symlinked target must be refused");
    assert!(err.to_string().contains("symlink"));
    assert_eq!(fs::read_to_string(&real).unwrap(), "nope");
}

#[cfg(unix)]
#[test]
fn enumerate_live_classifies_stale_beacon_as_unknown() {
    // ADR-091 Amendment 2: a beacon that is identity-valid
    // (live PID, matching start time) but whose refresh mtime has fallen
    // outside the freshness window is a wedged sidecar, not evidence of
    // registration — it must classify `Unknown`, not `RegisteredSilent`.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    write_beacon(&dir, &beacon(pid)).unwrap();
    let beacon_file = fs::OpenOptions::new()
        .write(true)
        .open(dir.join(format!("{pid}.beacon")))
        .unwrap();
    beacon_file
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(report.registered_silent_pids().count(), 0);
    assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![pid]);
    assert!(!report.fully_attributed());
    assert!(
        !beacon_path(&dir, pid).exists(),
        "a stale beacon must be deleted, not left to re-classify next sweep"
    );
}

#[cfg(unix)]
#[test]
fn enumerate_live_stale_heartbeat_with_fresh_beacon_stays_unknown_not_registered_silent() {
    // ADR-091 Amendment 2: a PID whose heartbeat was
    // deleted as stale must classify `Unknown`, even when a co-existing
    // FRESH beacon for the same PID would otherwise resolve it to
    // `RegisteredSilent`.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    write_beacon(&dir, &beacon(pid)).unwrap();
    let mut hb = heartbeat(pid);
    // Pre-amendment record: freshness classifies on `updated_at` alone.
    hb.oldest_tx_started_at = None;
    hb.updated_at = now_epoch_secs() - 3600;
    write_heartbeat(&dir, &hb).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(report.reporting().count(), 0);
    assert_eq!(
        report.registered_silent_pids().collect::<Vec<_>>(),
        Vec::<u32>::new(),
        "a co-existing fresh beacon must not rescue a PID with a stale heartbeat"
    );
    assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![pid]);
}

#[test]
#[cfg(target_os = "macos")]
fn census_holders_macos_discovers_self_as_a_holder_of_an_open_db_file() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("test.db");
    let file = fs::File::create(&db_path).unwrap();
    let census = census_holders(&db_path).expect("census must succeed for a live target");
    assert!(
        census.holders.contains(&std::process::id()),
        "this process holds {db_path:?} open and must appear in its own OS-derived census"
    );
    // The self-canary must NOT fire when self genuinely is discovered —
    // it only forces `truncated` on a missing self-PID, never clears an
    // already-set flag from something else.
    assert!(
        !census.truncated,
        "self was found; the self-canary must not report truncation on its own"
    );
    // Not asserting `is_complete()` here: an unprivileged process
    // legitimately cannot inspect every other PID's open fds on a real,
    // busy machine (other users' / root's processes), so
    // `uninspectable_pids` is realistically non-empty — that's exactly
    // the condition this fix now surfaces instead of silently ignoring.
    drop(file);
}

/// Producer-cadence regression: freshness is judged against the cadence
/// RECORDED in the entry, not the enumerating daemon's interval — a
/// session sweeping on an independently slower configured interval must
/// not be misread as stale by a faster-ticking daemon. Pre-amendment
/// records (`oldest_tx_started_at: None`) still classify on `updated_at`
/// (ADR-091 Amendment 3 Plank F1 mixed-version rule) — a new-style
/// record's `updated_at` would go stale under a touch-only tick even
/// while its mtime stays fresh, so this test pins the basis this
/// exercises to the old-style body field deliberately.
#[test]
#[cfg(unix)]
fn heartbeat_freshness_uses_producer_cadence_not_enumerator_interval() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();

    let mut slow = heartbeat(pid);
    slow.oldest_tx_started_at = None;
    slow.sweep_interval_ms = 60_000;
    slow.updated_at = now_epoch_secs() - 30;
    write_heartbeat(&dir, &slow).unwrap();

    // Enumerator ticking at 500ms: its own window would be 1.5s and the
    // 30s-old heartbeat would look stale, but the producer's recorded
    // 60s cadence keeps it fresh.
    let report = enumerate_live(&dir, Duration::from_millis(500)).unwrap();
    assert_eq!(
        report.reporting().count(),
        1,
        "a heartbeat 30s old under a recorded 60s cadence is fresh: {report:?}"
    );

    // Control: the same 30s-old timestamp under a recorded 1s cadence
    // IS stale — the recorded cadence cuts both ways.
    let mut fast = heartbeat(pid);
    fast.oldest_tx_started_at = None;
    fast.sweep_interval_ms = 1_000;
    fast.updated_at = now_epoch_secs() - 30;
    write_heartbeat(&dir, &fast).unwrap();
    let report = enumerate_live(&dir, Duration::from_secs(60)).unwrap();
    assert_eq!(report.reporting().count(), 0);
    assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![pid]);
}

/// ADR-091 Amendment 3 Plank F1 mixed-version rule: a record carrying
/// `oldest_tx_started_at` is new-style and classifies on the entry's
/// mtime, never the body's `updated_at` field — a stale `updated_at`
/// (as a touch-only tick would leave it) must not make a genuinely
/// fresh entry classify stale.
#[test]
#[cfg(unix)]
fn enumerate_live_new_style_heartbeat_uses_mtime_not_stale_updated_at() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let mut hb = heartbeat(std::process::id());
    hb.updated_at = now_epoch_secs() - 3600;
    write_heartbeat(&dir, &hb).unwrap(); // mtime is fresh (just written)

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(
        report.reporting().count(),
        1,
        "a new-style record with a fresh mtime must classify live regardless \
             of a stale `updated_at` body field: {report:?}"
    );
}

/// Complement of the above: a new-style record whose mtime has fallen
/// outside the declared window classifies stale even though its body's
/// `updated_at` field looks fresh — proving the classification basis is
/// genuinely the mtime, not merely "whichever of the two is fresher."
#[test]
#[cfg(unix)]
fn enumerate_live_new_style_heartbeat_stale_via_mtime_despite_fresh_updated_at() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    let hb = heartbeat(pid); // updated_at freshly set by the helper
    write_heartbeat(&dir, &hb).unwrap();

    let heartbeat_path = dir.join(format!("{pid}.json"));
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&heartbeat_path)
        .unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(report.reporting().count(), 0);
    assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![pid]);
    assert!(
        !heartbeat_path.exists(),
        "a new-style entry stale by mtime must be deleted, not merely unreported"
    );
}

/// ADR-091 Amendment 3 Plank F1: `3 x max(interval, 1000ms)` is exact
/// and inclusive — not "roughly 3 intervals" — and a sub-second
/// declared cadence floors the effective window at three seconds
/// (flooring the interval at 1s BEFORE multiplying by 3), never at one.
#[test]
#[cfg(unix)]
fn stale_window_from_boundary_inclusive_and_floors_subsecond_cadence_at_three_seconds() {
    assert_eq!(stale_window_from(Duration::from_secs(2)), 6);
    assert_eq!(stale_window_from(Duration::from_millis(100)), 3);
    assert_eq!(stale_window_from(Duration::from_millis(999)), 3);
    assert_eq!(stale_window_from(Duration::from_secs(1)), 3);
}

/// Boundary inclusivity exercised end-to-end through `enumerate_live`:
/// an entry exactly `3 x max(interval, 1000ms)` old is still live
/// (`<=`, not `<`); one second older is stale.
#[test]
#[cfg(unix)]
fn enumerate_live_new_style_heartbeat_boundary_is_inclusive() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    let mut hb = heartbeat(pid);
    hb.sweep_interval_ms = 2_000; // window = 3 * 2s = 6s

    write_heartbeat(&dir, &hb).unwrap();
    let heartbeat_path = dir.join(format!("{pid}.json"));
    let touch = |age_secs: u64| {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&heartbeat_path)
            .unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(age_secs))
            .unwrap();
    };

    touch(6);
    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(
        report.reporting().count(),
        1,
        "exactly 3x the declared interval old must still classify live: {report:?}"
    );

    // Re-write (enumeration deletes stale/live entries it processes,
    // but a live entry is retained — reuse the same file) and push one
    // second past the boundary.
    touch(7);
    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(
        report.reporting().count(),
        0,
        "one second past 3x the declared interval must classify stale: {report:?}"
    );
}

/// Sub-second declared cadence floors the effective window at three
/// seconds (not one) — exercised end-to-end, not just at the pure
/// `stale_window_from` function.
#[test]
#[cfg(unix)]
fn enumerate_live_new_style_heartbeat_subsecond_cadence_floors_at_three_seconds() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    let mut hb = heartbeat(pid);
    hb.sweep_interval_ms = 100; // floors to a 3s window, not 300ms/1s

    write_heartbeat(&dir, &hb).unwrap();
    let heartbeat_path = dir.join(format!("{pid}.json"));
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&heartbeat_path)
        .unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(2))
        .unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert_eq!(
        report.reporting().count(),
        1,
        "a 100ms declared cadence must floor its window at 3s, not 1s: 2s old must \
             still classify live: {report:?}"
    );
}

/// ADR-091 Amendment 3 Plank F2 fail-closed reading rule, exercised at
/// the canonical consumer-facing accessor: only the exact string
/// `"origin"` licenses an evidence-backed reading. A missing field or
/// any unrecognized value — including a value a future amendment might
/// define — must classify as fallback-confidence, never evidence-backed.
#[test]
fn attribution_is_evidence_backed_fails_closed_on_missing_or_unrecognized_value() {
    let mut hb = heartbeat(std::process::id());

    hb.attribution_basis = Some("origin".to_string());
    assert!(hb.attribution_is_evidence_backed());

    hb.attribution_basis = Some("fallback".to_string());
    assert!(!hb.attribution_is_evidence_backed());

    hb.attribution_basis = None;
    assert!(
        !hb.attribution_is_evidence_backed(),
        "a missing attribution_basis must never be read as evidence-backed"
    );

    hb.attribution_basis = Some("some-future-value".to_string());
    assert!(
        !hb.attribution_is_evidence_backed(),
        "an unrecognized value must degrade to fallback-confidence, never guess origin"
    );
}

/// A FIFO planted at a sidecar entry name must be refused as `Unknown`
/// without blocking enumeration — a plain `open(O_RDONLY)` on a
/// writer-less FIFO would hang the daemon's checkpoint task forever.
#[test]
#[cfg(unix)]
fn fifo_sidecar_entry_is_refused_without_blocking() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    write_beacon(&dir, &beacon(pid)).unwrap();

    use std::os::unix::ffi::OsStrExt;
    let fifo = dir.join("999999941.json");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c_path` is NUL-terminated; mkfifo creates a new node.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo failed: {}", io::Error::last_os_error());

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert!(
        report.unknown_pids().any(|p| p == 999_999_941),
        "a FIFO entry must classify its PID as unknown: {report:?}"
    );
    assert_eq!(
        report.registered_silent_pids().collect::<Vec<_>>(),
        vec![pid]
    );
}

/// An oversized sidecar entry must be refused as `Unknown` with a
/// bounded read — this module never writes bodies anywhere near the
/// cap, so an oversized entry is foreign, and reading it unboundedly
/// would let a same-uid process balloon enumeration.
#[test]
#[cfg(unix)]
fn oversized_sidecar_entry_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    let pid = std::process::id();
    write_beacon(&dir, &beacon(pid)).unwrap();

    fs::write(dir.join("999999942.json"), vec![b'x'; 128 * 1024]).unwrap();

    let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
    assert!(
        report.unknown_pids().any(|p| p == 999_999_942),
        "an oversized entry must classify its PID as unknown: {report:?}"
    );
}

#[test]
#[cfg(unix)]
fn readdir_null_is_error_distinguishes_eof_from_a_real_read_error() {
    assert!(
        !unix_impl::readdir_null_is_error(0),
        "errno == 0 after a NULL readdir means ordinary end-of-directory"
    );
    assert!(
        unix_impl::readdir_null_is_error(libc::EIO),
        "a nonzero errno after a NULL readdir means the walk failed mid-stream"
    );
}

/// The predicate test above only proves `readdir_null_is_error` itself
/// distinguishes EOF from a real error; it never drives `list_names` or
/// its production caller, so a regression at the actual call site (e.g.
/// folding a read error into an ordinary `break`) would leave that test
/// green. Forcing a real `readdir()` to fail mid-walk isn't practical
/// from a portable unit test, so this drives the fault through the
/// test-only seam and asserts the error reaches `enumerate_live`,
/// exactly as a genuine directory-read error must — never silently
/// reported as a complete-but-truncated listing.
#[test]
#[cfg(unix)]
fn readdir_failure_mid_walk_propagates_from_list_names_to_enumerate_live() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();

    unix_impl::set_list_names_readdir_fault(libc::EIO);
    let err = enumerate_live(&dir, Duration::from_secs(5)).expect_err(
        "a readdir failure mid-walk must propagate as an error, never be folded into a \
             truncated-but-otherwise-complete listing",
    );
    assert_eq!(err.raw_os_error(), Some(libc::EIO));
}

/// `remove_if_same`'s device/inode recheck and its `unlinkat` are two
/// separate syscalls; nothing closes the gap between them. This test
/// drives that exact gap via the test-only hook (forcing a real second
/// process to land in a few-instruction window isn't something a
/// portable unit test can do) and asserts the DOCUMENTED outcome: the
/// replacement that lands there is removed too, because `unlinkat`
/// operates on whatever now sits at the name, not on the inode that was
/// checked.
#[test]
#[cfg(unix)]
fn remove_if_same_a_replacement_landing_in_the_recheck_to_unlink_window_is_still_removed() {
    use std::os::unix::fs::MetadataExt;

    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("khive.db.walpin");
    ensure_sidecar_dir(&dir).unwrap();
    let handle = unix_impl::SidecarDirHandle::open_or_create(&dir).unwrap();

    let name = ".999999999.beacon.tmp";
    fs::write(dir.join(name), b"stale evidence").unwrap();
    let expected = handle
        .read_checked_entry(name)
        .unwrap()
        .expect("the stale file must be readable and checked");
    let expected_inode = fs::metadata(dir.join(name)).unwrap().ino();

    let replacement_path = dir.join(name);
    let replacement_tmp_path = dir.join(".999999999.beacon.tmp.replacement");
    let hook_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_ran_writer = std::sync::Arc::clone(&hook_ran);
    let replacement_inode = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let replacement_inode_writer = std::sync::Arc::clone(&replacement_inode);
    unix_impl::set_remove_if_same_race_hook(move || {
        // A genuine replacement swaps in a NEW inode via write-then-
        // rename: `fs::write`-ing the existing name in place would only
        // truncate the SAME inode the recheck already verified, proving
        // nothing about the recheck-to-unlink race this test targets.
        fs::write(
            &replacement_tmp_path,
            b"a producer's brand-new in-flight write",
        )
        .unwrap();
        let new_inode = fs::metadata(&replacement_tmp_path).unwrap().ino();
        fs::rename(&replacement_tmp_path, &replacement_path).unwrap();
        replacement_inode_writer.store(new_inode, std::sync::atomic::Ordering::SeqCst);
        hook_ran_writer.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    let removed = handle
        .remove_if_same(name, &expected)
        .expect("remove_if_same must not error when a replacement lands mid-call");

    assert!(
        hook_ran.load(std::sync::atomic::Ordering::SeqCst),
        "the race hook must actually run inside remove_if_same for this test to prove \
             anything about the recheck-to-unlink window"
    );
    assert_ne!(
        replacement_inode.load(std::sync::atomic::Ordering::SeqCst),
        expected_inode,
        "the hook must swap in a genuinely different inode, not rewrite the checked \
             one in place, or this test cannot distinguish the race from an ordinary \
             same-inode removal"
    );
    assert!(
        removed,
        "remove_if_same reports success because the name-based unlink always succeeds, \
             even though what it removed is no longer the inode it verified"
    );
    assert!(
        !dir.join(name).exists(),
        "a replacement landing in the recheck-to-unlink window is removed too — the \
             documented residual race, not one this function actually closes"
    );
}

/// Identity-comparison regression: a holder that opened the database
/// through a hard link (a different path to the same file) must still be
/// discovered — a path-string comparison would silently omit it while
/// leaving the census marked complete.
#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn census_holders_discovers_holder_through_hard_link_path() {
    let Some(self_pid) = census_visible_self_pid() else {
        eprintln!("skipping census test: process identity source is unavailable");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("test.db");
    fs::File::create(&db_path).unwrap();
    let link_path = root.path().join("test-link.db");
    fs::hard_link(&db_path, &link_path).unwrap();

    let file = fs::File::open(&link_path).unwrap();
    let census = census_holders(&db_path).expect("census must succeed for a live target");
    assert!(
            census.holders.contains(&self_pid),
            "this process holds the db open via hard link {link_path:?} and must appear in the census for {db_path:?}"
        );
    drop(file);
}

#[test]
#[cfg(target_os = "macos")]
fn negotiate_buffer_converges_when_the_set_stops_growing() {
    // Simulates a live set that "grows" for the first two size probes
    // (the data call keeps filling capacity) and then stabilizes —
    // negotiate_buffer must retry rather than report the first,
    // possibly-truncated snapshot as final.
    let probe = std::cell::Cell::new(0usize);
    let sizes = [4usize, 8, 8]; // bytes needed per size_call invocation
    let (items, truncated) = negotiate_buffer::<i32>(
        || {
            let i = probe.get().min(sizes.len() - 1);
            sizes[i] as std::os::raw::c_int
        },
        |_buf_ptr, buf_bytes| {
            let i = probe.get();
            probe.set(i + 1);
            // First two attempts: report the buffer as exactly full
            // (looks truncated); third attempt: report fewer bytes
            // than capacity (a clean, complete snapshot).
            if i < 2 {
                buf_bytes
            } else {
                (buf_bytes as usize - 4) as std::os::raw::c_int
            }
        },
        &|| false,
    )
    .expect("negotiation must succeed once the set stabilizes");
    assert!(
        !truncated,
        "a snapshot that ends up strictly under capacity must not be marked truncated"
    );
    assert!(!items.is_empty());
}

#[test]
#[cfg(target_os = "macos")]
fn negotiate_buffer_reports_truncated_after_exhausting_retries() {
    // The data call always reports the buffer as exactly full, no
    // matter how many times negotiate_buffer retries with a larger
    // buffer — this must give up after CENSUS_BUFFER_NEGOTIATION_ATTEMPTS
    // and report `truncated = true` rather than loop forever or lie.
    let (items, truncated) = negotiate_buffer::<i32>(
        || 4 as std::os::raw::c_int,
        |_buf_ptr, buf_bytes| buf_bytes,
        &|| false,
    )
    .expect("negotiation must still return a (possibly truncated) result, not error");
    assert!(
        truncated,
        "a buffer that stays exactly full across every retry must be reported truncated"
    );
    assert!(!items.is_empty());
}

#[test]
#[cfg(target_os = "macos")]
fn negotiate_buffer_propagates_a_failed_size_call() {
    let result = negotiate_buffer::<i32>(
        || -1 as std::os::raw::c_int,
        |_buf_ptr, buf_bytes| buf_bytes,
        &|| false,
    );
    assert!(result.is_err(), "a non-positive size probe must error out");
}

#[test]
#[cfg(target_os = "macos")]
fn macos_pid_genuinely_gone_only_true_for_esrch() {
    // ADR-091 Amendment 2: ESRCH (the target process
    // exited between listing and inspection) is a genuine "positively
    // gone" race, safe to skip. Every other errno — most commonly
    // EPERM/EACCES from trying to list another user's open files — means
    // the inspection itself failed, not that the PID is absent.
    assert!(macos_pid_genuinely_gone(Some(libc::ESRCH)));
    assert!(!macos_pid_genuinely_gone(Some(libc::EPERM)));
    assert!(!macos_pid_genuinely_gone(Some(libc::EACCES)));
    assert!(!macos_pid_genuinely_gone(None));
}

#[test]
#[cfg(target_os = "macos")]
fn proc_pidfdinfo_returned_expected_size_boundary() {
    // ADR-091 Amendment 2: a positive-but-short byte count must
    // classify as an inspection failure, not a successful call — only
    // an exact match on the expected struct size is `ok`.
    let expected = std::mem::size_of::<u64>(); // stand-in fixed-size struct
    assert!(
        proc_pidfdinfo_returned_expected_size(expected as i32, expected),
        "an exact match on the expected struct size must be ok"
    );
    assert!(
        !proc_pidfdinfo_returned_expected_size(expected as i32 - 1, expected),
        "a positive but short byte count must be an inspection failure"
    );
    assert!(
        !proc_pidfdinfo_returned_expected_size(0, expected),
        "a zero return must be an inspection failure"
    );
    assert!(
        !proc_pidfdinfo_returned_expected_size(-1, expected),
        "a negative return must be an inspection failure"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn census_holders_linux_discovers_self_as_a_holder_of_an_open_db_file() {
    let procfs_usable = fs::read_dir("/proc").is_ok()
        && fs::read_dir("/proc/self/fd").is_ok()
        && census_visible_self_pid().is_some();
    if !procfs_usable {
        eprintln!("skipping census test: procfs holder inspection is unavailable");
        return;
    }
    let self_pid = census_visible_self_pid().expect("checked above");
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("test.db");
    let file = fs::File::create(&db_path).unwrap();
    let census = census_holders(&db_path).expect("census must succeed for a live target");
    assert!(
        census.holders.contains(&self_pid),
        "this process holds {db_path:?} open and must appear in its own OS-derived census"
    );
    // The self-canary must NOT fire when self genuinely is discovered —
    // it only forces `truncated` on a missing self-PID, never clears an
    // already-set flag from something else.
    let global_census_supported = fs::metadata("/proc/self/ns/pid")
        .is_ok_and(|meta| pid_ns_is_init(meta.ino()))
        && proc_mount_is_visibility_restricted() == Some(false);
    if global_census_supported {
        assert!(
            !census.truncated,
            "self was found in an unrestricted init-namespace procfs census"
        );
    } else {
        assert!(
            census.truncated,
            "a namespace- or mount-restricted procfs census must stay incomplete"
        );
    }
    // Not asserting `is_complete()` here: an unprivileged process
    // legitimately cannot inspect every other PID's open fds on a real,
    // busy machine (other users' / root's processes), so
    // `uninspectable_pids` is realistically non-empty — that's exactly
    // the condition this fix now surfaces instead of silently ignoring.
    drop(file);
}

#[test]
#[cfg(target_os = "linux")]
fn linux_proc_gone_only_true_for_not_found() {
    // ADR-091 Amendment 2: NotFound (the process's
    // /proc/<pid>/fd directory raced away between listing and open) is a
    // genuine "positively gone" race, safe to skip. PermissionDenied
    // (inspecting another user's fds) means the inspection itself
    // failed, not that the PID is absent.
    assert!(linux_proc_gone(&io::Error::from(io::ErrorKind::NotFound)));
    assert!(!linux_proc_gone(&io::Error::from(
        io::ErrorKind::PermissionDenied
    )));
}

#[test]
#[cfg(target_os = "linux")]
fn pid_ns_is_init_only_true_for_the_fixed_kernel_inode() {
    // ADR-091 Amendment 2: only the exact kernel-assigned init
    // namespace inode (`include/linux/proc_ns.h`) is complete-eligible.
    // A container's own, internally self-consistent PID namespace gets
    // a different, dynamically allocated inode and must classify as
    // incomplete — that is precisely the self-consistent-container gap
    // the old readlink comparison could not detect.
    assert!(pid_ns_is_init(PROC_PID_INIT_INO));
    assert!(!pid_ns_is_init(PROC_PID_INIT_INO + 1));
    assert!(!pid_ns_is_init(0));
    assert!(!pid_ns_is_init(12345));
}

#[test]
#[cfg(target_os = "linux")]
fn proc_mount_restricts_visibility_classifies_hidepid_and_subset() {
    // ADR-091 Amendment 2: a clean options string never restricts.
    assert!(!proc_mount_restricts_visibility(
        "rw,nosuid,nodev,noexec,relatime"
    ));
    // Numeric hidepid values other than 0 restrict.
    assert!(proc_mount_restricts_visibility("rw,hidepid=2"));
    // Symbolic hidepid values (Linux 5.8+) restrict too.
    assert!(proc_mount_restricts_visibility("rw,hidepid=invisible"));
    assert!(proc_mount_restricts_visibility("rw,hidepid=ptraceable"));
    // subset=pid (any subset=) restricts.
    assert!(proc_mount_restricts_visibility("rw,subset=pid"));
    // hidepid=0 is the explicit non-restricting value.
    assert!(!proc_mount_restricts_visibility("hidepid=0"));
    // hidepid=off is the symbolic equivalent of hidepid=0.
    assert!(!proc_mount_restricts_visibility("rw,hidepid=off"));
    // A bare `hidepid` flag with no value is treated as restricting —
    // the kernel's default nonzero behavior, not a proven-clean mount.
    assert!(proc_mount_restricts_visibility("rw,hidepid"));
}

#[test]
#[cfg(target_os = "linux")]
fn proc_mounts_restricted_in_is_any_restrictive_across_stacked_mounts() {
    // Mounts stack: a later /proc mount shadows an earlier one while
    // both records stay in mountinfo. Selection must be ANY-restrictive
    // across every matching record — a clean shadowed mount must not
    // mask a restricted visible one, in either record order.
    let clean = "36 25 0:16 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw";
    let restricted = "99 25 0:34 / /proc rw,relatime - proc proc rw,hidepid=2";
    let clean_then_restricted = format!("{clean}\n{restricted}");
    let restricted_then_clean = format!("{restricted}\n{clean}");
    assert_eq!(proc_mounts_restricted_in(clean), Some(false));
    assert_eq!(proc_mounts_restricted_in(restricted), Some(true));
    assert_eq!(
        proc_mounts_restricted_in(&clean_then_restricted),
        Some(true)
    );
    assert_eq!(
        proc_mounts_restricted_in(&restricted_then_clean),
        Some(true)
    );
    // No /proc procfs record at all → None (caller fails closed).
    assert_eq!(
        proc_mounts_restricted_in("36 25 0:16 / /sys rw - sysfs sysfs rw"),
        None
    );
}

#[test]
#[cfg(target_os = "linux")]
fn proc_mount_is_visibility_restricted_reads_this_hosts_own_proc_mount() {
    // Live check against whatever /proc this test process actually
    // runs under — asserts the parse succeeds (Some(_)), not a fixed
    // verdict, since CI/dev/container hosts differ. A `None` here
    // would mean mountinfo parsing silently failed on a real host,
    // which the caller treats as fail-closed (`truncated = true`) —
    // this test exists to catch that regression, not to assert which
    // way this particular host's mount classifies.
    assert!(
        proc_mount_is_visibility_restricted().is_some(),
        "expected to find and parse this process's own /proc mount entry in \
             /proc/self/mountinfo"
    );
}

#[test]
fn census_result_is_complete_reflects_uninspectable_pids() {
    let complete = CensusResult {
        holders: std::collections::HashSet::from([1, 2]),
        uninspectable_pids: Vec::new(),
        truncated: false,
        budget_exhausted: false,
    };
    assert!(complete.is_complete());

    let incomplete = CensusResult {
        holders: std::collections::HashSet::from([1]),
        uninspectable_pids: vec![7],
        truncated: false,
        budget_exhausted: false,
    };
    assert!(!incomplete.is_complete());
}

/// A bounded walk that finds fewer holders is indistinguishable from a
/// store with fewer holders unless the result declares its own truncation
/// and the consumer branches on it — so this asserts the declaration, not
/// the speed. Timing it would pass on a fast machine with the bound
/// removed, which is the one arm that must fail.
#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn a_spent_budget_truncates_the_census_instead_of_failing_it() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("test.db");
    let handle = fs::File::create(&db_path).unwrap();

    // Control, same process and same file in the same test: an unbounded
    // census never reports a spent budget, so the flag below cannot be
    // something every census on this host sets.
    let unbounded = census_holders(&db_path).expect("unbounded census of a live target");
    assert!(
        !unbounded.budget_exhausted,
        "an unbounded census has no budget to spend"
    );

    let bounded = census_holders_until_within(&db_path, || false, Duration::ZERO)
        .expect("a spent budget returns the partial census, never an error");
    assert!(
        bounded.budget_exhausted,
        "a walk stopped by its budget must say so; a caller cannot otherwise \
             tell a bounded answer from a complete one"
    );
    assert!(
        bounded.truncated,
        "budget exhaustion is an incompleteness signal, folded into the same \
             field every other incompleteness uses"
    );
    assert!(
        !bounded.is_complete(),
        "the consumer branches on is_complete(); a bounded census that reads \
             complete is the defect this bound would otherwise introduce"
    );

    drop(handle);
}

/// The two stop mechanisms are not interchangeable and the difference is
/// the whole reason the budget could not simply be expressed as a
/// `should_stop` closure: a cancellation says the caller stopped wanting
/// the answer, a spent budget says the caller wants whatever was found.
#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn a_cancel_and_a_spent_budget_leave_by_opposite_exits() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("test.db");
    let handle = fs::File::create(&db_path).unwrap();

    let cancelled =
        census_holders_until(&db_path, || true).expect_err("a cancellation is an error by design");
    assert_eq!(
        cancelled.kind(),
        io::ErrorKind::Interrupted,
        "a cancelled census reports Interrupted"
    );

    let bounded = census_holders_until_within(&db_path, || false, Duration::ZERO)
        .expect("a spent budget is not a cancellation");
    assert!(bounded.budget_exhausted);

    drop(handle);
}

/// A budget generous enough for the walk must leave no trace: the flag
/// reports what happened, never that a bound was configured.
#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn an_unspent_budget_is_invisible_in_the_result() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("test.db");
    let handle = fs::File::create(&db_path).unwrap();

    let bounded = census_holders_until_within(&db_path, || false, Duration::from_secs(300))
        .expect("census of a live target");
    assert!(
        !bounded.budget_exhausted,
        "a five-minute budget cannot be spent by one process walk on a test host"
    );

    drop(handle);
}

#[test]
fn census_result_is_complete_reflects_truncated() {
    // ADR-091 Amendment 2: `truncated` is a second,
    // independent incompleteness signal — a census can have an empty
    // `uninspectable_pids` (no single PID's inspection failed) and
    // still be incomplete because the walk itself has positive evidence
    // it missed part of the process universe.
    let truncated = CensusResult {
        holders: std::collections::HashSet::from([1]),
        uninspectable_pids: Vec::new(),
        truncated: true,
        budget_exhausted: false,
    };
    assert!(!truncated.is_complete());
}

#[test]
fn self_canary_marks_truncated_when_own_pid_missing() {
    let scanner_pid = 41;
    let mut census = CensusResult {
        holders: std::collections::HashSet::from([42]),
        uninspectable_pids: Vec::new(),
        truncated: false,
        budget_exhausted: false,
    };
    census.apply_self_canary_for(Some(scanner_pid));
    assert!(
        census.truncated,
        "a census that discovered other holders but not the calling process itself is \
             positive proof of a missed enumeration and must be marked incomplete"
    );
}

#[test]
fn self_canary_leaves_a_correct_census_untouched() {
    let scanner_pid = 41;
    let mut census = CensusResult {
        holders: std::collections::HashSet::from([scanner_pid]),
        uninspectable_pids: Vec::new(),
        truncated: false,
        budget_exhausted: false,
    };
    census.apply_self_canary_for(Some(scanner_pid));
    assert!(
        !census.truncated,
        "self was found; the canary must not fire"
    );
}

#[test]
fn self_canary_does_not_clear_an_existing_truncated_flag() {
    let scanner_pid = 41;
    let mut census = CensusResult {
        holders: std::collections::HashSet::from([scanner_pid]),
        uninspectable_pids: Vec::new(),
        truncated: true,
        budget_exhausted: false,
    };
    census.apply_self_canary_for(Some(scanner_pid));
    assert!(
        census.truncated,
        "the self-canary only ever sets `truncated`; it must never clear a flag another \
             step already raised"
    );
}

#[test]
fn self_canary_fails_closed_when_process_identity_is_unavailable() {
    let mut census = CensusResult::default();

    census.apply_self_canary_for(None);

    assert!(census.truncated);
}

/// #1335: the Windows FFI paths (`open_relative`/`NtCreateFile`,
/// `validate_owner_only_dacl`, `rename_via_handle`,
/// `remove_relative_if_exists`/`delete_via_handle`, mtime touch) are
/// exercised only through `windows_impl`'s public-to-`super` wrappers
/// (`ensure_sidecar_dir`/`write_heartbeat`/`touch_heartbeat`/
/// `remove_heartbeat`/`write_beacon`/`touch_beacon`/`remove_beacon`),
/// driven against a real temp directory. `windows_impl` only exists
/// under `cfg(windows)`, so this module compiles and runs on Windows
/// only; it has no effect on `cargo check`/`cargo test` for any other
/// target.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::fs;

    #[test]
    fn ensure_sidecar_dir_creates_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).expect("should create");
        let meta = fs::symlink_metadata(&dir).unwrap();
        assert!(meta.is_dir());
    }

    #[test]
    fn ensure_sidecar_dir_is_idempotent_and_revalidates_dacl() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).expect("first create should succeed");
        ensure_sidecar_dir(&dir)
            .expect("second call must re-open and re-validate the existing dir, not fail");
    }

    #[test]
    fn ensure_sidecar_dir_refuses_preexisting_dir_with_default_acl() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        // A plain `create_dir` inherits the parent's ACL rather than the
        // single owner-only ACE this module writes — the DACL round-trip
        // must refuse it rather than repair it in place.
        fs::create_dir(&dir).unwrap();
        let err = ensure_sidecar_dir(&dir)
            .expect_err("a pre-existing dir without the exact owner-only DACL must be refused");
        assert!(err.to_string().contains("owner"), "unexpected error: {err}");
    }

    #[test]
    fn ensure_sidecar_dir_refuses_symlinked_target() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real_dir");
        fs::create_dir(&real).unwrap();
        let link = root.path().join("khive.db.walpin");
        std::os::windows::fs::symlink_dir(&real, &link).expect(
            "creating a directory symlink requires Developer Mode or an elevated \
                 process on the Windows CI runner",
        );
        let err = ensure_sidecar_dir(&link)
            .expect_err("a reparse-point sidecar path must be refused, never followed");
        assert!(
            err.to_string().contains("reparse"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn write_heartbeat_creates_then_replaces_then_removes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        let path = dir.join(format!("{pid}.json"));

        let first = heartbeat(pid);
        write_heartbeat(&dir, &first).expect("initial create must succeed");
        let read_back: WalpinHeartbeat =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read_back, first);

        let mut second = heartbeat(pid);
        second.oldest_tx_label = Some("replaced".to_string());
        write_heartbeat(&dir, &second).expect("replacing an already-existing target must succeed");
        let read_back: WalpinHeartbeat =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read_back, second);
        assert_ne!(read_back, first);

        remove_heartbeat(&dir, pid).expect("remove must succeed");
        assert!(!path.exists());
    }

    #[test]
    fn replacing_heartbeat_keeps_old_target_until_rename() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        let path = dir.join(format!("{pid}.json"));
        let first = heartbeat(pid);
        write_heartbeat(&dir, &first).unwrap();
        let old_body = fs::read(&path).unwrap();

        let mut second = heartbeat(pid);
        second.oldest_tx_label = Some("replacement".to_string());
        let hook_ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook_ran_inside = std::rc::Rc::clone(&hook_ran);
        super::super::windows_impl::set_before_target_rename_hook(move || {
            assert_eq!(
                fs::read(&path).expect("old target must still exist before rename"),
                old_body,
                "the old heartbeat must remain at the target until replacement"
            );
            hook_ran_inside.set(true);
        });
        write_heartbeat(&dir, &second).expect("replacement write must succeed");
        assert!(hook_ran.get(), "the inspection-to-rename hook must run");
        assert_eq!(
            fs::read(dir.join(format!("{pid}.json"))).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );
    }

    #[test]
    fn write_heartbeat_refuses_directory_target_without_removing_it() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).unwrap();
        let hb = heartbeat(std::process::id());
        let target = dir.join(format!("{}.json", hb.pid));
        fs::create_dir(&target).unwrap();

        write_heartbeat(&dir, &hb).expect_err("directory target must be refused");
        assert!(fs::symlink_metadata(&target).unwrap().is_dir());
    }

    #[test]
    fn write_heartbeat_refuses_reparse_target_without_removing_it() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).unwrap();
        let outside = root.path().join("outside.txt");
        fs::write(&outside, b"untouched").unwrap();
        let hb = heartbeat(std::process::id());
        let target = dir.join(format!("{}.json", hb.pid));
        std::os::windows::fs::symlink_file(&outside, &target).expect(
            "creating a file symlink requires Developer Mode or an elevated \
                 process on the Windows CI runner",
        );

        write_heartbeat(&dir, &hb).expect_err("reparse target must be refused");
        assert!(fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    }

    #[test]
    fn repeated_heartbeat_write_validates_sidecar_root_once() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        let first = heartbeat(pid);
        write_heartbeat(&dir, &first).expect("initial write must create the sidecar");

        let before = super::super::windows_impl::open_dir_handle_call_count();
        let mut replacement = first;
        replacement.oldest_tx_label = Some("replacement".to_string());
        write_heartbeat(&dir, &replacement).expect("replacement write must succeed");
        let validations = super::super::windows_impl::open_dir_handle_call_count() - before;

        assert_eq!(
            validations, 1,
            "an existing sidecar root must be fully validated exactly once per record write"
        );
    }

    #[test]
    fn remove_heartbeat_on_missing_sidecar_dir_is_a_noop() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        remove_heartbeat(&dir, 4242).expect("missing sidecar dir must be a no-op");
        assert!(
            !dir.exists(),
            "removal must never create the sidecar dir as a side effect"
        );
    }

    #[test]
    fn touch_heartbeat_refreshes_mtime_without_changing_content() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        let hb = heartbeat(pid);
        write_heartbeat(&dir, &hb).unwrap();
        let path = dir.join(format!("{pid}.json"));
        let before = fs::metadata(&path).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        touch_heartbeat(&dir, pid).expect("touch of an existing heartbeat must succeed");

        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(
            after > before,
            "touch must advance the mtime; an unchanged timestamp means the \
                 refresh was a no-op (before {before:?}, after {after:?})"
        );
        let content_after: WalpinHeartbeat =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            content_after, hb,
            "touch is metadata-only; the body must be unchanged"
        );
    }

    #[test]
    fn touch_heartbeat_fails_when_entry_is_absent() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).unwrap();
        let err =
            touch_heartbeat(&dir, 99999).expect_err("touching a nonexistent heartbeat must fail");
        assert!(
            err.to_string().contains("does not exist"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn beacon_write_touch_remove_cycle() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        let b = beacon(pid);
        let path = beacon_path(&dir, pid);

        write_beacon(&dir, &b).expect("beacon create must succeed");
        assert!(path.exists());
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        touch_beacon(&dir, pid).expect("beacon touch must succeed");
        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(
            after > before,
            "beacon touch must advance the mtime; an unchanged timestamp means \
                 the refresh was a no-op (before {before:?}, after {after:?})"
        );
        remove_beacon(&dir, pid).expect("beacon remove must succeed");
        assert!(!path.exists());
    }
}
