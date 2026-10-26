use std::ffi::{CString, OsStr};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::record::{encode, path};
use super::record_io::validate_metadata;
use super::*;

const EPOCH: &str = "c7b078c0-bb86-49fa-b1a9-28d7056cef76";

fn uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory preconditions.
    unsafe { libc::geteuid() }
}
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    (temp, root)
}

fn stores(root: &Path) -> StoreSet {
    let main = root.join("main.db");
    StoreSet::from_canonical(main.clone(), vec![main, root.join("events.db")]).unwrap()
}
fn target() -> TargetIdentity {
    TargetIdentity::new(
        PathBuf::from("/installed/khive"),
        ImageIdentity::Inode {
            device: 1,
            inode: 2,
        },
        "config".into(),
    )
    .unwrap()
}
fn coordinator() -> CoordinatorIdentity {
    CoordinatorIdentity {
        uid: uid(),
        pid: 42,
        boot_id: EPOCH.into(),
        start_ticks: 123,
        image: ImageIdentity::Inode {
            device: 2,
            inode: 3,
        },
    }
}
fn sample_record(root: &Path, phase: Phase) -> Declaration {
    Declaration {
        epoch: EPOCH.into(),
        sequence: 1,
        phase,
        stores: stores(root),
        target: target(),
        coordinator: coordinator(),
        capability_digest: [17; 32],
        phase_started_unix_ms: 100,
        phase_deadline_unix_ms: 200,
        failure_code: (phase == Phase::FailedClosed).then(|| "repair_required".into()),
    }
}

fn wire_value(record: &Declaration) -> serde_json::Value {
    serde_json::from_slice(&record.encode_untrusted(uid()).unwrap()).unwrap()
}
fn refuses_value(value: &serde_json::Value) {
    assert!(
        Declaration::parse_untrusted(&serde_json::to_vec(value).unwrap(), uid()).is_err(),
        "accepted {value}"
    );
}

#[test]
fn wire_roundtrip_preserves_non_utf8_paths_and_image_identity() {
    let (_temp, root) = fixture();
    let mut value = sample_record(&root, Phase::Closing);
    let main = path(b"/stores/\xff.db".to_vec());
    value.stores = StoreSet::from_canonical(main.clone(), vec![main]).unwrap();
    for image in [
        ImageIdentity::Inode {
            device: u64::MAX,
            inode: 1,
        },
        ImageIdentity::Modified {
            before_epoch: true,
            seconds: 17,
            nanoseconds: 123_456_789,
        },
        ImageIdentity::Modified {
            before_epoch: false,
            seconds: 0,
            nanoseconds: 0,
        },
    ] {
        value.target = TargetIdentity::new(
            path(b"/installed/\xfe".to_vec()),
            image.clone(),
            "config".into(),
        )
        .unwrap();
        value.coordinator.image = image;
        let decoded =
            Declaration::parse_untrusted(&value.encode_untrusted(uid()).unwrap(), uid()).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(
            decoded.stores().main().as_os_str().as_bytes(),
            b"/stores/\xff.db"
        );
        assert_eq!(
            decoded.target().start_path().as_os_str().as_bytes(),
            b"/installed/\xfe"
        );
    }
}

#[test]
fn wire_refuses_malformed_duplicate_unknown_and_wrong_typed_fields() {
    let (_temp, root) = fixture();
    let record = sample_record(&root, Phase::Closing);
    let good = record.encode_untrusted(uid()).unwrap();
    assert_eq!(Declaration::parse_untrusted(&good, uid()).unwrap(), record);
    let text = String::from_utf8(good).unwrap();
    for bad in [
        "{".to_owned(),
        format!("{text} true"),
        text.replacen('{', "{\"version\":1,", 1),
        text.replacen("\"uid\":", "\"uid\":0,\"uid\":", 1),
        text.replacen(
            "\"kind\":\"inode\"",
            "\"kind\":\"inode\",\"kind\":\"inode\"",
            1,
        ),
    ] {
        assert!(
            Declaration::parse_untrusted(bad.as_bytes(), uid()).is_err(),
            "accepted {bad}"
        );
    }
    let good = wire_value(&record);
    for field in good.as_object().unwrap().keys() {
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove(field);
        refuses_value(&missing);
        let mut wrong = good.clone();
        wrong[field] = serde_json::json!(true);
        refuses_value(&wrong);
    }
    for object in [None, Some("stores"), Some("target"), Some("coordinator")] {
        let mut unknown = good.clone();
        let value = match object {
            Some(name) => &mut unknown[name],
            None => &mut unknown,
        };
        value["unexpected"] = serde_json::json!(1);
        refuses_value(&unknown);
        if let Some(name) = object {
            for field in good[name].as_object().unwrap().keys() {
                let mut missing = good.clone();
                missing[name].as_object_mut().unwrap().remove(field);
                refuses_value(&missing);
            }
        }
    }
    for (field, bad) in [
        ("version", serde_json::json!(2)),
        ("sequence", serde_json::json!(0)),
        ("sequence", serde_json::json!(-1)),
        ("sequence", serde_json::json!(1.5)),
        ("phase", serde_json::json!("open")),
        ("epoch", serde_json::json!(EPOCH.to_uppercase())),
        ("epoch", serde_json::json!(EPOCH.replace('-', ""))),
        ("failure_code", serde_json::json!("not_failed")),
    ] {
        let mut bad_value = good.clone();
        bad_value[field] = bad;
        refuses_value(&bad_value);
    }
    for image in [
        serde_json::json!({"kind":"inode","device":1,"inode":0}),
        serde_json::json!({"kind":"inode","device":1,"inode":2,"extra":0}),
        serde_json::json!({"kind":"modified","before_epoch":true,"seconds":0,"nanoseconds":0}),
        serde_json::json!({"kind":"modified","before_epoch":false,"seconds":1,"nanoseconds":1_000_000_000}),
    ] {
        let mut bad = good.clone();
        bad["target"]["image"] = image;
        refuses_value(&bad);
    }
    for (field, value) in [
        ("pid", serde_json::json!(0)),
        ("start_ticks", serde_json::json!(0)),
        ("uid", serde_json::json!(uid().wrapping_add(1))),
        ("boot_id", serde_json::json!("unavailable")),
    ] {
        let mut bad = good.clone();
        bad["coordinator"][field] = value;
        refuses_value(&bad);
    }
    for code in ["", "UPPER", "raw error!", &"x".repeat(65)] {
        let mut bad = wire_value(&record);
        bad["phase"] = serde_json::json!("failed_closed");
        bad["failure_code"] = serde_json::json!(code);
        refuses_value(&bad);
    }
    let mut failed = record.clone();
    failed.phase = Phase::FailedClosed;
    failed.failure_code = Some("x".repeat(64));
    assert_eq!(
        Declaration::parse_untrusted(&failed.encode_untrusted(uid()).unwrap(), uid()).unwrap(),
        failed
    );
}

#[test]
fn wire_enforces_size_config_and_deadline_bounds() {
    let (_temp, root) = fixture();
    let mut record = sample_record(&root, Phase::Closing);
    record.target = TargetIdentity::new(
        PathBuf::from("/khive"),
        ImageIdentity::Inode {
            device: 1,
            inode: 1,
        },
        "x".repeat(MAX_CONFIG_ID_BYTES),
    )
    .unwrap();
    record.phase_started_unix_ms = 0;
    record.phase_deadline_unix_ms = MAX_PHASE_INTERVAL_MS;
    let mut exact = record.encode_untrusted(uid()).unwrap();
    exact.resize(MAX_RECORD_BYTES, b' ');
    assert_eq!(Declaration::parse_untrusted(&exact, uid()).unwrap(), record);
    exact.push(b' ');
    assert!(Declaration::parse_untrusted(&exact, uid()).is_err());
    let mut bad = wire_value(&record);
    bad["target"]["config_id"] = serde_json::json!("x".repeat(MAX_CONFIG_ID_BYTES + 1));
    refuses_value(&bad);
    assert!(TargetIdentity::new(
        PathBuf::from("/khive"),
        ImageIdentity::Inode {
            device: 1,
            inode: 1
        },
        "x".repeat(MAX_CONFIG_ID_BYTES + 1)
    )
    .is_err());
    for (start, end) in [
        (0, 0),
        (100, 99),
        (0, MAX_PHASE_INTERVAL_MS + 1),
        (0, u64::MAX),
    ] {
        record.phase_started_unix_ms = start;
        record.phase_deadline_unix_ms = end;
        assert!(record.encode_untrusted(uid()).is_err());
        let mut bad = bad.clone();
        bad["target"]["config_id"] = serde_json::json!("");
        bad["phase_started_unix_ms"] = serde_json::json!(start);
        bad["phase_deadline_unix_ms"] = serde_json::json!(end);
        refuses_value(&bad);
    }
    record.phase_started_unix_ms = u64::MAX - 1;
    record.phase_deadline_unix_ms = u64::MAX;
    assert_eq!(
        Declaration::parse_untrusted(&record.encode_untrusted(uid()).unwrap(), uid()).unwrap(),
        record
    );
    let mut oversized = record.clone();
    let huge = path(format!("/{}", "x".repeat(MAX_RECORD_BYTES)).into_bytes());
    oversized.stores = StoreSet::from_canonical(huge.clone(), vec![huge]).unwrap();
    assert!(oversized.encode_untrusted(uid()).is_err());
}

#[test]
fn wire_refuses_store_digest_path_and_owner_mismatches() {
    let (_temp, root) = fixture();
    let record = sample_record(&root, Phase::Ready);
    assert_eq!(
        Declaration::parse_untrusted(&record.encode_untrusted(uid()).unwrap(), uid()).unwrap(),
        record
    );
    let mut bad = wire_value(&record);
    bad["stores"]["digest"][0] =
        serde_json::json!(bad["stores"]["digest"][0].as_u64().unwrap() ^ 1);
    refuses_value(&bad);
    for bad_path in [
        b"relative.db".to_vec(),
        b"/stores/../main.db".to_vec(),
        b"/stores/a\0.db".to_vec(),
    ] {
        let mut bad = wire_value(&record);
        bad["stores"]["main"] = serde_json::json!(bad_path);
        refuses_value(&bad);
    }
    let mut wrong_owner = record.clone();
    wrong_owner.coordinator.uid = uid().wrapping_add(1);
    assert!(wrong_owner.encode_untrusted(uid()).is_err());
    assert_eq!(
        Declaration::parse_untrusted(&record.encode_untrusted(uid()).unwrap(), uid()).unwrap(),
        record
    );
}

#[test]
fn wire_decode_refuses_foreign_expected_owner_uid() {
    let record = sample_record(Path::new("/stores"), Phase::Ready);
    let owner = record.coordinator().uid();
    let encoded = record.encode_untrusted(owner).unwrap();
    assert_eq!(
        Declaration::parse_untrusted(&encoded, owner).unwrap(),
        record
    );
    assert!(matches!(
        Declaration::parse_untrusted(&encoded, owner.wrapping_add(1)),
        Err(RecordRefusal::Invalid("invalid coordinator attribution"))
    ));
}

#[test]
fn wire_decode_refuses_unsorted_store_members() {
    let record = sample_record(Path::new("/stores"), Phase::Ready);
    let owner = record.coordinator().uid();
    let encoded = record.encode_untrusted(owner).unwrap();
    assert_eq!(
        Declaration::parse_untrusted(&encoded, owner).unwrap(),
        record
    );
    let mut wire: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    let paths = wire["stores"]["paths"].as_array_mut().unwrap();
    assert_eq!(paths.len(), 2);
    paths.swap(0, 1);
    assert!(matches!(
        Declaration::parse_untrusted(&serde_json::to_vec(&wire).unwrap(), owner),
        Err(RecordRefusal::Invalid("noncanonical store paths"))
    ));
}

#[test]
fn wire_decode_refuses_duplicate_store_members() {
    let record = sample_record(Path::new("/stores"), Phase::Ready);
    let owner = record.coordinator().uid();
    let encoded = record.encode_untrusted(owner).unwrap();
    assert_eq!(
        Declaration::parse_untrusted(&encoded, owner).unwrap(),
        record
    );
    let mut wire: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    let paths = wire["stores"]["paths"].as_array_mut().unwrap();
    assert_eq!(paths.len(), 2);
    let duplicate = paths.last().unwrap().clone();
    paths.push(duplicate);
    assert!(matches!(
        Declaration::parse_untrusted(&serde_json::to_vec(&wire).unwrap(), owner),
        Err(RecordRefusal::Invalid("noncanonical store paths"))
    ));
}

const NEXT_EPOCH: &str = "c7b078c0-bb86-49fa-b1a9-28d7056cef77";
const NAME: &str = "maintenance.json";

fn location(root: &Path) -> RecordLocation {
    RecordLocation::open_discovered(root, RecordSlot::new(OsStr::new(NAME)).unwrap()).unwrap()
}

fn expectation(root: &Path) -> ReadExpectation {
    ReadExpectation::participant(root.join("main.db")).unwrap()
}

fn write_bytes(root: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(root.join(NAME))
        .unwrap();
    file.write_all(bytes).unwrap();
}

fn write_record(root: &Path, record: &Declaration) {
    write_bytes(root, &encode(record, uid()).unwrap());
}

fn verified(root: &Path) -> Declaration {
    match read_record(&location(root), &expectation(root)) {
        ReadObservation::Verified(value) => *value,
        other => panic!("expected verified record: {other:?}"),
    }
}

fn rejects(root: &Path, expected: &ReadExpectation) {
    assert!(matches!(
        read_record(&location(root), expected),
        ReadObservation::Rejected(_)
    ));
}

#[cfg(target_os = "linux")]
fn draft(root: &Path, epoch: &str) -> EpochDraft {
    EpochDraft {
        epoch: epoch.into(),
        stores: stores(root),
        target: target(),
        capability_digest: [18; 32],
        phase_started_unix_ms: 100,
        phase_deadline_unix_ms: 200,
    }
}

#[cfg(target_os = "linux")]
fn next(phase: Phase) -> PhaseAdvance {
    PhaseAdvance {
        phase,
        phase_started_unix_ms: 110,
        phase_deadline_unix_ms: 210,
        failure_code: None,
    }
}

#[test]
fn record_read_refuses_untrusted_entries_and_parent_metadata() {
    let (_temp, root) = fixture();
    let record = sample_record(&root, Phase::Closing);
    write_record(&root, &record);
    assert_eq!(verified(&root), record);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(verified(&root), record, "owned 0755 parent is safe");
    fs::set_permissions(root.join(NAME), fs::Permissions::from_mode(0o640)).unwrap();
    rejects(&root, &expectation(&root));
    fs::remove_file(root.join(NAME)).unwrap();
    let target = root.join("other");
    fs::write(&target, b"private").unwrap();
    symlink(&target, root.join(NAME)).unwrap();
    rejects(&root, &expectation(&root));
    fs::remove_file(root.join(NAME)).unwrap();
    fs::create_dir(root.join(NAME)).unwrap();
    rejects(&root, &expectation(&root));
    fs::remove_dir(root.join(NAME)).unwrap();
    let fifo = CString::new(root.join(NAME).as_os_str().as_bytes()).unwrap();
    // SAFETY: the owned, NUL-terminated test path is valid for the call.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    rejects(&root, &expectation(&root));
    fs::remove_file(root.join(NAME)).unwrap();
    write_record(&root, &record);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(
        RecordLocation::open_discovered(&root, RecordSlot::new(OsStr::new(NAME)).unwrap()).is_err()
    );
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let linked = root.join("link");
    symlink(&root, &linked).unwrap();
    assert!(
        RecordLocation::open_discovered(&linked, RecordSlot::new(OsStr::new(NAME)).unwrap())
            .is_err()
    );
    for name in ["", ".", "..", "a/b", "/absolute", "nul\0name"] {
        assert!(RecordSlot::new(OsStr::new(name)).is_err());
    }
    for (mode, owner, directory) in [
        (libc::S_IFREG | 0o600, uid().wrapping_add(1), false),
        (libc::S_IFREG | 0o601, uid(), false),
        (libc::S_IFDIR | 0o700, uid(), false),
        (libc::S_IFDIR | 0o777, uid(), true),
        (libc::S_IFDIR | 0o755, uid().wrapping_add(1), true),
    ] {
        assert!(validate_metadata(mode, owner, uid(), directory).is_err());
    }
}

#[test]
fn verified_missing_is_distinct_from_io_refusal() {
    let (_temp, root) = fixture();
    let mut location = location(&root);
    assert!(matches!(
        read_record(&location, &expectation(&root)),
        ReadObservation::Missing
    ));
    for kind in [
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::Other,
        std::io::ErrorKind::NotFound,
    ] {
        location.faults.read_error = Some(kind);
        assert!(matches!(
            read_record(&location, &expectation(&root)),
            ReadObservation::Rejected(_)
        ));
    }
    location.faults.read_error = None;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        read_record(&location, &expectation(&root)),
        ReadObservation::Rejected(_)
    ));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        read_record(&location, &expectation(&root)),
        ReadObservation::Missing
    ));
    write_bytes(&root, b"not JSON");
    assert!(matches!(
        read_record(&location, &expectation(&root)),
        ReadObservation::Rejected(_)
    ));

    // Exercise the transferred wire cap through the actual bounded file reader.
    let mut record = sample_record(&root, Phase::Closing);
    record.target = TargetIdentity::new(
        PathBuf::from("/khive"),
        ImageIdentity::Inode {
            device: 1,
            inode: 1,
        },
        "x".repeat(MAX_CONFIG_ID_BYTES),
    )
    .unwrap();
    record.phase_started_unix_ms = 0;
    record.phase_deadline_unix_ms = MAX_PHASE_INTERVAL_MS;
    let mut exact = encode(&record, uid()).unwrap();
    exact.resize(MAX_RECORD_BYTES, b' ');
    write_bytes(&root, &exact);
    assert_eq!(
        verified(&root),
        record,
        "valid-at-cap file read must succeed"
    );
    exact.push(b' ');
    write_bytes(&root, &exact);
    rejects(&root, &expectation(&root));
}

#[test]
fn store_and_expected_epoch_target_bindings_are_checked() {
    let (_temp, root) = fixture();
    let record = sample_record(&root, Phase::Ready);
    write_record(&root, &record);
    let exact = expectation(&root)
        .with_epoch(EPOCH.into())
        .unwrap()
        .with_stores(record.stores.clone())
        .with_target(record.target.clone())
        .with_coordinator(record.coordinator.clone());
    assert!(matches!(
        read_record(&location(&root), &exact),
        ReadObservation::Verified(_)
    ));
    rejects(
        &root,
        &ReadExpectation::participant(root.join("absent.db")).unwrap(),
    );
    rejects(
        &root,
        &expectation(&root).with_epoch(NEXT_EPOCH.into()).unwrap(),
    );
    let main = root.join("main.db");
    rejects(
        &root,
        &expectation(&root)
            .with_stores(StoreSet::from_canonical(main.clone(), vec![main]).unwrap()),
    );
    let changed_target = TargetIdentity::new(
        PathBuf::from("/other/image"),
        target().image().clone(),
        "different".into(),
    )
    .unwrap();
    rejects(&root, &expectation(&root).with_target(changed_target));
    let mut owner = coordinator();
    owner.start_ticks += 1;
    rejects(&root, &expectation(&root).with_coordinator(owner));
    let (_other_temp, other) = fixture();
    let foreign_location = sample_record(&other, Phase::Ready);
    write_record(&root, &foreign_location);
    rejects(&root, &expectation(&root));
    write_record(&root, &record);
    assert_eq!(verified(&root), record);
}

#[test]
fn writer_capture_refuses_changed_or_unavailable_incarnation() {
    let stable = coordinator();
    assert!(super::process::same_incarnation(&stable, Ok(stable.clone())).is_ok());
    for field in 0..5 {
        let mut changed = stable.clone();
        match field {
            0 => changed.uid = changed.uid.wrapping_add(1),
            1 => changed.pid += 1,
            2 => changed.start_ticks += 1,
            3 => changed.boot_id = NEXT_EPOCH.into(),
            _ => {
                changed.image = ImageIdentity::Inode {
                    device: 2,
                    inode: 4,
                }
            }
        }
        assert!(super::process::same_incarnation(&stable, Ok(changed)).is_err());
    }
    assert!(super::process::same_incarnation(
        &stable,
        Err(RecordRefusal::Io(std::io::Error::from_raw_os_error(
            libc::EIO
        )))
    )
    .is_err());
    let fields = format!("R {} 987654321", vec!["0"; 18].join(" "));
    assert_eq!(
        super::process::parse_stat(&format!("42 (comm ) with spaces) {fields}")).unwrap(),
        (42, 987654321)
    );
    for bad in ["", "0 (x) R", "42 (x) R 0", "42 x R", "no (x) R"] {
        assert!(super::process::parse_stat(bad).is_err());
    }
    #[cfg(target_os = "linux")]
    {
        let real = super::process::capture().unwrap();
        assert_eq!(real.pid(), std::process::id());
        assert_eq!(real.uid(), uid());
        assert!(real.start_ticks() > 0);
        assert!(super::process::same_incarnation(&real, super::process::capture()).is_ok());
    }
    #[cfg(not(target_os = "linux"))]
    assert!(matches!(
        super::process::capture(),
        Err(RecordRefusal::UnsupportedAttribution)
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn retained_ready_does_not_require_a_live_coordinator() {
    const CHILD: &str = "KHIVE_RECORD_READY_TEST_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let mut writer = DeclarationWriter::claim(location(&root)).unwrap();
        writer.begin(draft(&root, EPOCH)).unwrap();
        for phase in [Phase::Quiescent, Phase::Migrating, Phase::Ready] {
            writer.advance(next(phase)).unwrap();
        }
        return;
    }
    // Polling bounds process lifetime; only observed exit establishes ordering.
    struct OwnedChild {
        child: std::process::Child,
        reaped: bool,
    }
    impl OwnedChild {
        fn until(
            &mut self,
            deadline: std::time::Instant,
        ) -> std::io::Result<Option<std::process::ExitStatus>> {
            loop {
                if let Some(status) = self.child.try_wait()? {
                    self.reaped = true;
                    return Ok(Some(status));
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Ok(None);
                }
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
            }
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if !self.reaped {
                // Panic/error cleanup must not introduce another blocking wait.
                let _ = self.child.kill();
                let _ = self.child.try_wait();
            }
        }
    }
    let (_temp, root) = fixture();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "maintenance::record_tests::retained_ready_does_not_require_a_live_coordinator",
            "--test-threads=1",
        ])
        .env(CHILD, &root)
        .spawn()
        .unwrap();
    let mut child = OwnedChild {
        child,
        reaped: false,
    };
    let pid = child.child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = match child.until(deadline).expect("poll retained-Ready child") {
        Some(status) => status,
        None => {
            let killed = child.child.kill();
            let reaped = child.until(std::time::Instant::now() + std::time::Duration::from_secs(2));
            panic!("retained-Ready child timed out; kill={killed:?}; bounded reap={reaped:?}");
        }
    };
    assert!(status.success());
    let ready = verified(&root);
    assert_eq!(ready.phase(), Phase::Ready);
    assert_eq!(ready.coordinator().pid(), pid);
    assert!(matches!(
        read_record(
            &location(&root),
            &expectation(&root).with_coordinator(ready.coordinator().clone())
        ),
        ReadObservation::Verified(_)
    ));
    let mut successor = DeclarationWriter::claim(location(&root)).unwrap();
    assert!(
        successor.begin(draft(&root, EPOCH)).is_err(),
        "same epoch may not restart"
    );
    assert_eq!(
        successor
            .begin(draft(&root, NEXT_EPOCH))
            .unwrap()
            .declaration()
            .sequence(),
        1
    );
}

#[test]
fn closed_dead_or_expired_owner_never_grants_release() {
    let (_temp, root) = fixture();
    for phase in [
        Phase::Closing,
        Phase::Quiescent,
        Phase::Migrating,
        Phase::FailedClosed,
    ] {
        let mut record = sample_record(&root, phase);
        record.coordinator.pid = u32::MAX;
        record.phase_started_unix_ms = 0;
        record.phase_deadline_unix_ms = 1;
        write_record(&root, &record);
        let observed = verified(&root);
        assert_eq!(observed.phase(), phase);
        assert_ne!(observed.phase(), Phase::Ready);
        #[cfg(target_os = "linux")]
        {
            let mut writer = DeclarationWriter::claim(location(&root)).unwrap();
            assert!(writer.begin(draft(&root, NEXT_EPOCH)).is_err());
        }
    }
    // Missing and closed are observations; there is deliberately no Allow API.
}

#[cfg(target_os = "linux")]
#[test]
fn stable_claim_serializes_independent_openers_across_publish() {
    let (_temp, root) = fixture();
    let mut first = DeclarationWriter::claim(location(&root)).unwrap();
    assert!(matches!(
        DeclarationWriter::claim(location(&root)),
        Err(ClaimError::Busy)
    ));
    let claim_before = fs::metadata(root.join(format!("{NAME}.claim"))).unwrap();
    first.begin(draft(&root, EPOCH)).unwrap();
    first.advance(next(Phase::Quiescent)).unwrap();
    assert!(matches!(
        DeclarationWriter::claim(location(&root)),
        Err(ClaimError::Busy)
    ));
    use std::os::unix::fs::MetadataExt;
    let claim_after = fs::metadata(root.join(format!("{NAME}.claim"))).unwrap();
    assert_eq!(
        (claim_before.dev(), claim_before.ino()),
        (claim_after.dev(), claim_after.ino())
    );
    assert_eq!(claim_after.mode() & 0o777, 0o600);
    drop(first);
    let recovered = DeclarationWriter::claim(location(&root)).unwrap();
    drop(recovered);
    fs::set_permissions(
        root.join(format!("{NAME}.claim")),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(matches!(
        DeclarationWriter::claim(location(&root)),
        Err(ClaimError::Refused(_))
    ));
    let claim = root.join(format!("{NAME}.claim"));
    fs::remove_file(&claim).unwrap();
    symlink(root.join(NAME), &claim).unwrap();
    assert!(matches!(
        DeclarationWriter::claim(location(&root)),
        Err(ClaimError::Refused(_))
    ));
    fs::remove_file(&claim).unwrap();
    let fifo = CString::new(claim.as_os_str().as_bytes()).unwrap();
    // SAFETY: the owned, NUL-terminated fixture path is valid for mkfifo.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        DeclarationWriter::claim(location(&root)),
        Err(ClaimError::Refused(_))
    ));
    fs::remove_file(&claim).unwrap();
    fs::create_dir(&claim).unwrap();
    assert!(matches!(
        DeclarationWriter::claim(location(&root)),
        Err(ClaimError::Refused(_))
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn transitions_preserve_binding_and_refuse_shortcuts() {
    let (_temp, root) = fixture();
    let mut writer = DeclarationWriter::claim(location(&root)).unwrap();
    let original = writer
        .begin(draft(&root, EPOCH))
        .unwrap()
        .declaration()
        .clone();
    assert_eq!((original.phase(), original.sequence()), (Phase::Closing, 1));
    let before = fs::read(root.join(NAME)).unwrap();
    assert!(writer.advance(next(Phase::Ready)).is_err());
    assert_eq!(fs::read(root.join(NAME)).unwrap(), before);
    let quiescent = writer
        .advance(next(Phase::Quiescent))
        .unwrap()
        .declaration()
        .clone();
    let mut stale = quiescent.clone();
    stale.sequence += 1;
    write_record(&root, &stale);
    assert!(writer.advance(next(Phase::Migrating)).is_err());
    write_record(&root, &quiescent);
    let mut retargeted = quiescent.clone();
    retargeted.target = TargetIdentity::new(
        PathBuf::from("/different"),
        target().image().clone(),
        "config".into(),
    )
    .unwrap();
    write_record(&root, &retargeted);
    assert!(writer.advance(next(Phase::Migrating)).is_err());
    write_record(&root, &quiescent);
    let mut overflow = quiescent.clone();
    overflow.sequence = u64::MAX;
    writer.current = Some(overflow.clone());
    write_record(&root, &overflow);
    assert!(writer.advance(next(Phase::Migrating)).is_err());
    writer.current = Some(quiescent.clone());
    write_record(&root, &quiescent);
    for (phase, sequence) in [(Phase::Migrating, 3), (Phase::Ready, 4)] {
        let current = writer.advance(next(phase)).unwrap().declaration().clone();
        assert_eq!(current.sequence(), sequence);
        assert_eq!(current.epoch(), original.epoch());
        assert_eq!(current.stores(), original.stores());
        assert_eq!(current.target(), original.target());
        assert_eq!(current.coordinator(), original.coordinator());
        assert_eq!(current.capability_digest(), original.capability_digest());
    }
    assert!(writer.advance(next(Phase::FailedClosed)).is_err());
    assert!(
        writer.begin(draft(&root, EPOCH)).is_err(),
        "same epoch cannot reset sequence"
    );
    let mut changed = draft(&root, NEXT_EPOCH);
    changed.stores =
        StoreSet::from_canonical(root.join("main.db"), vec![root.join("main.db")]).unwrap();
    assert!(
        writer.begin(changed).is_err(),
        "membership cannot change at old slot"
    );
    let mut changed = draft(&root, NEXT_EPOCH);
    changed.stores =
        StoreSet::from_canonical(root.join("events.db"), stores(&root).paths().to_vec()).unwrap();
    assert!(writer.begin(changed).is_err(), "main role cannot change");
    writer.begin(draft(&root, NEXT_EPOCH)).unwrap();
    assert!(
        writer.begin(draft(&root, EPOCH)).is_err(),
        "closed epoch cannot be replaced"
    );
    assert!(
        writer.advance(next(Phase::FailedClosed)).is_err(),
        "failure code required"
    );
    let mut failed = next(Phase::FailedClosed);
    failed.failure_code = Some("readiness_failed".into());
    assert_eq!(
        writer.advance(failed).unwrap().declaration().phase(),
        Phase::FailedClosed
    );
    assert!(writer.begin(draft(&root, EPOCH)).is_err());
    let (_temp, other) = fixture();
    let mut wrong_location = draft(&other, EPOCH);
    wrong_location.stores = stores(&root);
    assert!(DeclarationWriter::claim(location(&other))
        .unwrap()
        .begin(wrong_location)
        .is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn post_rename_sync_failure_retains_pending_visibility() {
    let (_temp, root) = fixture();
    let mut writer = DeclarationWriter::claim(location(&root)).unwrap();
    writer.location.faults.write_error = true;
    let error = writer.begin(draft(&root, EPOCH)).unwrap_err();
    assert_eq!(
        error.phase,
        PublishPhase::Atomic(khive_fs::atomic_publish::AtomicPublishPhase::WriteTmp)
    );
    assert_eq!(error.visibility, Visibility::Unchanged);
    assert!(matches!(
        read_record(&location(&root), &expectation(&root)),
        ReadObservation::Missing
    ));
    writer.location.faults.write_error = false;
    writer.begin(draft(&root, EPOCH)).unwrap();
    writer.location.faults.publish_sync_error = true;
    let error = writer.advance(next(Phase::Quiescent)).unwrap_err();
    assert_eq!(
        error.phase,
        PublishPhase::Atomic(khive_fs::atomic_publish::AtomicPublishPhase::SyncDirectory)
    );
    assert_eq!(error.visibility, Visibility::DestinationMayBeVisible);
    assert!(matches!(error.cause, PublishCause::Io(ref e) if e.raw_os_error() == Some(libc::EIO)));
    assert_eq!(verified(&root).phase(), Phase::Quiescent);
    assert_eq!(
        writer
            .advance(next(Phase::Migrating))
            .unwrap_err()
            .visibility,
        Visibility::DestinationMayBeVisible
    );
    assert!(writer.begin(draft(&root, NEXT_EPOCH)).is_err());
    assert!(matches!(
        DeclarationWriter::claim(location(&root)),
        Err(ClaimError::Busy)
    ));
    writer.location.faults.publish_sync_error = false;
    writer.reconcile_pending().unwrap();
    writer.advance(next(Phase::Migrating)).unwrap();
    writer.location.faults.publish_sync_error = true;
    assert!(writer.advance(next(Phase::Ready)).is_err());
    drop(writer);
    assert_eq!(
        verified(&root).phase(),
        Phase::Ready,
        "visible Ready survives writer exit without a hidden pending marker"
    );
    DeclarationWriter::claim(location(&root))
        .unwrap()
        .begin(draft(&root, NEXT_EPOCH))
        .unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn reconcile_requires_exact_readback_and_durable_sync() {
    let (_temp, root) = fixture();
    let mut writer = DeclarationWriter::claim(location(&root)).unwrap();
    writer.begin(draft(&root, EPOCH)).unwrap();
    writer.location.faults.publish_sync_error = true;
    writer.advance(next(Phase::Quiescent)).unwrap_err();
    let exact = fs::read(root.join(NAME)).unwrap();
    let visible = verified(&root);
    write_bytes(&root, b"not JSON");
    assert!(writer.reconcile_pending().is_err());
    let mut changed = visible.clone();
    changed.capability_digest[0] ^= 1;
    write_record(&root, &changed);
    assert!(writer.reconcile_pending().is_err());
    let mut equivalent = exact.clone();
    equivalent.push(b' ');
    write_bytes(&root, &equivalent);
    assert!(
        writer.reconcile_pending().is_err(),
        "semantic equality is insufficient"
    );
    write_bytes(&root, &exact);
    let saved = root.join("saved-attempt");
    fs::rename(root.join(NAME), &saved).unwrap();
    assert!(writer.reconcile_pending().is_err(), "missing is uncertain");
    write_bytes(&root, &exact);
    assert!(
        writer.reconcile_pending().is_err(),
        "byte-identical replacement inode is not the attempted publication"
    );
    fs::remove_file(root.join(NAME)).unwrap();
    fs::rename(&saved, root.join(NAME)).unwrap();
    writer.location.faults.read_error = Some(std::io::ErrorKind::PermissionDenied);
    assert!(writer.reconcile_pending().is_err());
    writer.location.faults.read_error = None;
    writer.location.faults.reconcile_sync_error = true;
    let error = writer.reconcile_pending().unwrap_err();
    assert_eq!(error.phase, PublishPhase::ReconcileSync);
    assert_eq!(error.visibility, Visibility::DestinationMayBeVisible);
    assert!(writer.advance(next(Phase::Migrating)).is_err());
    assert_eq!(
        fs::read(root.join(NAME)).unwrap(),
        exact,
        "no rollback or guessed recovery write"
    );
    writer.location.faults.reconcile_sync_error = false;
    assert_eq!(writer.reconcile_pending().unwrap().declaration(), &visible);
    writer.location.faults.publish_sync_error = false;
    assert_eq!(
        writer
            .advance(next(Phase::Migrating))
            .unwrap()
            .declaration()
            .sequence(),
        3
    );
}
