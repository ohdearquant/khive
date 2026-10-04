use super::{
    directory_identity, open_checkpoint_directory, open_checkpoint_directory_for_listing,
    open_checkpoint_read_file, open_verified_directory_with, remove_checkpoint_file,
    rename_checkpoint_file, stage_checkpoint_file, write_via_dir_handle_with,
    ExternalIdsWriteError, FILE_READ_ATTRIBUTES, FINAL_NAME, TMP_NAME,
};

#[test]
fn rename_failure_preserves_original_sidecar() {
    let segment_dir = tempfile::tempdir().expect("create temporary segment directory");
    let original = b"last valid sidecar";
    let replacement = b"replacement sidecar";
    std::fs::write(segment_dir.path().join(FINAL_NAME), original).expect("write original sidecar");

    let error = write_via_dir_handle_with(segment_dir.path(), replacement, |_, _, _| {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    })
    .expect_err("injected rename failure must be returned");

    assert!(error
        .to_string()
        .starts_with("rename external_ids.bin.tmp -> external_ids.bin:"));
    assert_eq!(
        std::fs::read(segment_dir.path().join(FINAL_NAME)).expect("read original sidecar"),
        original
    );
    assert_eq!(
        std::fs::read(segment_dir.path().join(TMP_NAME)).expect("read temporary sidecar"),
        replacement
    );
}

#[test]
fn checkpoint_directory_rejects_ordinary_replacement_before_retained_open() {
    use std::sync::mpsc;
    use std::time::Duration;

    let fixture = tempfile::tempdir().expect("temporary checkpoint fixture");
    let segment = fixture.path().join("segment");
    let retired = fixture.path().join("retired");
    std::fs::create_dir(&segment).unwrap();
    std::fs::write(segment.join("fixture.txt"), b"original fixture").unwrap();
    let expected = std::fs::canonicalize(&segment).unwrap();
    let (pinned_tx, pinned_rx) = mpsc::channel();
    let (replacement_tx, replacement_rx) = mpsc::channel();
    let opening_path = segment.clone();
    let opening = std::thread::spawn(move || {
        open_verified_directory_with(&opening_path, &expected, FILE_READ_ATTRIBUTES, || {
            pinned_tx.send(()).unwrap();
            replacement_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        })
    });

    pinned_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the checked directory must be pinned before substitution");
    std::fs::rename(&segment, &retired).unwrap();
    std::fs::create_dir(&segment).unwrap();
    std::fs::write(segment.join("fixture.txt"), b"replacement fixture").unwrap();
    replacement_tx.send(()).unwrap();
    let error = opening
        .join()
        .unwrap()
        .expect_err("ordinary replacement must refuse");
    assert!(matches!(
        error,
        ExternalIdsWriteError::DirectoryIdentityChanged
    ));
    assert_eq!(
        std::fs::read(retired.join("fixture.txt")).unwrap(),
        b"original fixture"
    );
    assert_eq!(
        std::fs::read(segment.join("fixture.txt")).unwrap(),
        b"replacement fixture"
    );
    assert_eq!(std::fs::read_dir(&segment).unwrap().count(), 1);
}

#[test]
fn checkpoint_sidecar_operations_stay_on_retained_directory_identity() {
    let fixture = tempfile::tempdir().expect("temporary checkpoint fixture");
    let segment = fixture.path().join("segment");
    let retired = fixture.path().join("retired");
    std::fs::create_dir(&segment).unwrap();
    let directory = open_checkpoint_directory(&segment).expect("pin directory");
    let listing = open_checkpoint_directory_for_listing(&segment).expect("pin listing directory");
    let identity = directory_identity(&directory).unwrap();
    assert_eq!(directory_identity(&listing).unwrap(), identity);
    std::fs::rename(&segment, &retired).unwrap();
    std::fs::create_dir(&segment).unwrap();
    std::fs::write(segment.join("fixture.bin"), b"replacement fixture").unwrap();

    stage_checkpoint_file(&directory, "fixture.tmp", b"pinned fixture").unwrap();
    rename_checkpoint_file(&directory, "fixture.tmp", "fixture.bin").unwrap();
    let mut file = open_checkpoint_read_file(&directory, "fixture.bin")
        .unwrap()
        .expect("read through retained handle");
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).unwrap();
    assert_eq!(bytes, b"pinned fixture");
    drop(file);
    let (names, bounded) = super::list_checkpoint_names_bounded(&listing, 10).unwrap();
    assert!(!bounded);
    assert_eq!(names, ["fixture.bin"]);
    assert_eq!(directory_identity(&directory).unwrap(), identity);
    assert_eq!(
        std::fs::read(retired.join("fixture.bin")).unwrap(),
        b"pinned fixture"
    );
    remove_checkpoint_file(&directory, "fixture.bin").unwrap();
    assert!(!retired.join("fixture.bin").exists());
    assert_eq!(
        std::fs::read(segment.join("fixture.bin")).unwrap(),
        b"replacement fixture"
    );
}
