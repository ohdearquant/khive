use super::{write_via_dir_handle_with, FINAL_NAME, TMP_NAME};

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
