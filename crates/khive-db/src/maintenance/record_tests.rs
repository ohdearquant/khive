use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::record::path;
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
