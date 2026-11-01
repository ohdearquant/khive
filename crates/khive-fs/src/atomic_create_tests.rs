use super::*;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        for _ in 0..MAX_ATTEMPTS {
            let path = std::env::temp_dir().join(format!(
                "khive-atomic-create-test-{}-{}",
                std::process::id(),
                NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed),
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create test directory: {error}"),
            }
        }
        panic!("test directory collision limit reached")
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn entries(&self) -> usize {
        fs::read_dir(&self.0).unwrap().count()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fail_before_sync(_: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "pre-sync fault",
    ))
}

fn fail_remove() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "cleanup fault",
    ))
}

#[cfg(unix)]
fn fail_directory_sync(_: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "directory-sync fault",
    ))
}

#[test]
fn complete_creation_and_existing_empty_file_preserve_bytes() {
    let dir = TestDir::new();
    let destination = dir.file("default");
    assert!(create_only(&destination, |file| file.write_all(b"complete default\n")).unwrap());
    assert_eq!(fs::read(&destination).unwrap(), b"complete default\n");
    assert!(!create_only(&destination, |_| panic!("existing file invoked writer")).unwrap());
    assert_eq!(fs::read(&destination).unwrap(), b"complete default\n");
    let empty = dir.file("empty-operator-file");
    fs::write(&empty, b"").unwrap();
    assert!(!create_only(&empty, |_| panic!("empty operator file invoked writer")).unwrap());
    assert_eq!(fs::read(&empty).unwrap(), b"");
    assert_eq!(dir.entries(), 2);
}

#[test]
fn nonempty_prefix_failure_never_publishes_and_complete_retry_succeeds() {
    let dir = TestDir::new();
    let destination = dir.file("default");
    let error = create_only(&destination, |file| {
        file.write_all(b"nonempty partial prefix")?;
        Err(io::Error::other("planted writer failure"))
    })
    .unwrap_err();
    assert!(!error.published());
    assert_eq!(error.io_error().to_string(), "planted writer failure");
    assert!(std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<io::Error>()
        .is_some());
    assert!(!destination.exists());
    assert_eq!(dir.entries(), 0);
    assert!(create_only(&destination, |file| file.write_all(b"whole retry\n")).unwrap());
    assert_eq!(fs::read(&destination).unwrap(), b"whole retry\n");
    assert_eq!(dir.entries(), 1);
}

#[test]
fn competing_destination_created_during_staging_wins_unchanged() {
    let dir = TestDir::new();
    let destination = dir.file("default");
    let created = create_only(&destination, |file| {
        file.write_all(b"new default")?;
        fs::write(&destination, b"operator winner")
    })
    .unwrap();
    assert!(!created);
    assert_eq!(fs::read(&destination).unwrap(), b"operator winner");
    assert_eq!(dir.entries(), 1);
}

#[test]
fn prefix_failure_never_removes_a_competing_destination() {
    let dir = TestDir::new();
    let destination = dir.file("default");
    let error = create_only(&destination, |file| {
        file.write_all(b"partial stage")?;
        fs::write(&destination, b"operator winner")?;
        Err(io::Error::other("planted writer failure"))
    })
    .unwrap_err();
    assert!(!error.published());
    assert_eq!(error.io_error().to_string(), "planted writer failure");
    assert_eq!(fs::read(&destination).unwrap(), b"operator winner");
    assert_eq!(dir.entries(), 1);
}

#[test]
fn failure_before_file_sync_leaves_no_destination() {
    let dir = TestDir::new();
    let destination = dir.file("default");
    let faults = Faults {
        before_sync: Some(fail_before_sync),
        ..Faults::default()
    };
    let error = create_with(
        &destination,
        |file| file.write_all(b"written but not synced"),
        &faults,
        || OsString::from("owned-stage"),
    )
    .unwrap_err();
    assert!(!error.published());
    assert_eq!(error.io_error().to_string(), "pre-sync fault");
    assert!(!destination.exists());
    assert_eq!(dir.entries(), 0);
}

#[test]
fn post_link_cleanup_error_reports_complete_publication() {
    let dir = TestDir::new();
    let destination = dir.file("default");
    let faults = Faults {
        remove: Some(fail_remove),
        ..Faults::default()
    };
    let error = create_with(
        &destination,
        |file| file.write_all(b"complete linked bytes"),
        &faults,
        || OsString::from("owned-stage"),
    )
    .unwrap_err();
    assert!(error.published());
    assert_eq!(error.io_error().to_string(), "cleanup fault");
    assert!(error.to_string().contains("complete file published"));
    assert_eq!(fs::read(&destination).unwrap(), b"complete linked bytes");
    assert_eq!(
        fs::read(dir.file("owned-stage")).unwrap(),
        b"complete linked bytes"
    );
}

#[test]
fn existing_winner_cleanup_error_is_not_a_clean_false_or_publication() {
    let dir = TestDir::new();
    let destination = dir.file("default");
    let faults = Faults {
        remove: Some(fail_remove),
        ..Faults::default()
    };
    let error = create_with(
        &destination,
        |file| {
            file.write_all(b"staged default")?;
            fs::write(&destination, b"operator winner")
        },
        &faults,
        || OsString::from("owned-stage"),
    )
    .unwrap_err();
    assert!(!error.published());
    assert_eq!(error.io_error().to_string(), "cleanup fault");
    assert_eq!(fs::read(&destination).unwrap(), b"operator winner");
    assert_eq!(
        fs::read(dir.file("owned-stage")).unwrap(),
        b"staged default"
    );
}

#[cfg(unix)]
#[test]
fn directory_sync_error_keeps_complete_destination_without_stage() {
    let dir = TestDir::new();
    let destination = dir.file("default");
    let faults = Faults {
        directory: Some(fail_directory_sync),
        ..Faults::default()
    };
    let error = create_with(
        &destination,
        |file| file.write_all(b"complete linked bytes"),
        &faults,
        || OsString::from("owned-stage"),
    )
    .unwrap_err();
    assert!(error.published());
    assert_eq!(error.io_error().to_string(), "directory-sync fault");
    assert_eq!(fs::read(&destination).unwrap(), b"complete linked bytes");
    assert!(!dir.file("owned-stage").exists());
    assert_eq!(dir.entries(), 1);
}

#[test]
fn exclusive_stage_collisions_are_preserved_and_retry_is_bounded() {
    let dir = TestDir::new();
    let collision = dir.file("collision");
    fs::write(&collision, b"another attempt owns this").unwrap();
    let destination = dir.file("default");
    let mut names_called = 0;
    let mut writes = 0;
    let created = create_with(
        &destination,
        |file| {
            writes += 1;
            file.write_all(b"complete default")
        },
        &Faults::default(),
        || {
            names_called += 1;
            OsString::from(if names_called == 1 {
                "collision"
            } else {
                "fresh-stage"
            })
        },
    )
    .unwrap();
    assert!(created);
    assert_eq!(writes, 1);
    assert_eq!(names_called, 2);
    assert_eq!(fs::read(&collision).unwrap(), b"another attempt owns this");
    assert_eq!(fs::read(&destination).unwrap(), b"complete default");
    let mut retries = 0;
    let other = dir.file("other-final");
    let error = create_with(
        &other,
        |_| panic!("collision invoked writer"),
        &Faults::default(),
        || {
            retries += 1;
            OsString::from("collision")
        },
    )
    .unwrap_err();
    assert_eq!(error.io_error().kind(), io::ErrorKind::AlreadyExists);
    assert!(!error.published());
    assert_eq!(retries, MAX_ATTEMPTS);
    assert!(!other.exists());
    assert_eq!(fs::read(&collision).unwrap(), b"another attempt owns this");
}

#[cfg(unix)]
#[test]
fn replaced_stage_identity_is_refused_without_deleting_replacement() {
    let dir = TestDir::new();
    let destination = dir.file("default");
    let error = create_with(
        &destination,
        |file| {
            file.write_all(b"original stage")?;
            fs::rename(dir.file("owned-stage"), dir.file("moved-original"))?;
            fs::write(dir.file("owned-stage"), b"foreign replacement")
        },
        &Faults::default(),
        || OsString::from("owned-stage"),
    )
    .unwrap_err();
    assert!(!error.published());
    assert_eq!(error.io_error().kind(), io::ErrorKind::InvalidData);
    assert!(!destination.exists());
    assert_eq!(
        fs::read(dir.file("owned-stage")).unwrap(),
        b"foreign replacement"
    );
    assert_eq!(
        fs::read(dir.file("moved-original")).unwrap(),
        b"original stage"
    );
}
