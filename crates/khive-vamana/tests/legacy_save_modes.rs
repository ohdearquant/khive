#![cfg(all(unix, feature = "mmap"))]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use khive_vamana::{VamanaConfig, VamanaIndex};

#[test]
fn legacy_save_preserves_restrictive_segment_modes() {
    const CHILD: &str = "KHIVE_VAMANA_LEGACY_MODE_CHILD";
    const TEST: &str = "legacy_save_preserves_restrictive_segment_modes";

    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("start isolated permission test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("running 1 test"),
            "the subprocess must select the test, not pass an empty filter:\n{stdout}\n{stderr}"
        );
        assert!(
            output.status.success(),
            "isolated mode-preservation regression failed:\n{stdout}\n{stderr}"
        );
        return;
    }

    // umask is process-global: change it only in the child selecting this test.
    struct RestoreUmask(libc::mode_t);
    impl Drop for RestoreUmask {
        fn drop(&mut self) {
            // SAFETY: restoring the process-local value saved below.
            unsafe { libc::umask(self.0) };
        }
    }
    // SAFETY: this child process runs only the selected test; mode is a valid mask.
    let _restore = RestoreUmask(unsafe { libc::umask(0o022) });
    let dir = tempfile::tempdir().expect("private synthetic fixture");
    let probe = dir.path().join("creation-mode-probe");
    fs::File::create(&probe).expect("create ordinary sibling");
    assert_ne!(
        fs::metadata(&probe).unwrap().permissions().mode() & 0o777,
        0o600,
        "fixture requires a default mode distinct from a restricted segment"
    );
    fs::remove_file(&probe).unwrap();

    let vectors = [1.0_f32, 0.0, 0.0, 1.0, -1.0, 0.0, 0.0, -1.0];
    let config = VamanaConfig::with_dimensions(2)
        .with_max_degree(2)
        .with_search_list_size(4);
    let index = VamanaIndex::build(&vectors, config).expect("build synthetic owned index");
    index.save(dir.path()).expect("initial legacy save");
    let segments = ["vectors.bin", "graph.bin", "metadata.bin"];
    for name in segments {
        fs::set_permissions(dir.path().join(name), fs::Permissions::from_mode(0o600))
            .expect("restrict pre-existing segment");
    }

    index.save(dir.path()).expect("overwrite legacy save");
    for name in segments {
        let actual = fs::metadata(dir.path().join(name))
            .expect("published segment")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(actual, 0o600, "overwrite broadened permissions for {name}");
    }
}

#[test]
fn legacy_save_new_segments_match_platform_creation_mode() {
    const CHILD: &str = "KHIVE_VAMANA_LEGACY_NEW_MODE_CHILD";
    const TEST: &str = "legacy_save_new_segments_match_platform_creation_mode";

    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("start isolated permission test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("running 1 test"),
            "the subprocess must select the test, not pass an empty filter:\n{stdout}\n{stderr}"
        );
        assert!(
            output.status.success(),
            "isolated new-segment mode test failed:\n{stdout}\n{stderr}"
        );
        return;
    }

    struct RestoreUmask(libc::mode_t);
    impl Drop for RestoreUmask {
        fn drop(&mut self) {
            // SAFETY: restoring the process-local value saved below.
            unsafe { libc::umask(self.0) };
        }
    }
    // SAFETY: this child process runs only the selected test; mode is a valid mask.
    let _restore = RestoreUmask(unsafe { libc::umask(0o022) });
    let dir = tempfile::tempdir().expect("private synthetic fixture");
    let ordinary = dir.path().join("ordinary-sibling");
    fs::File::create(&ordinary).expect("create ordinary sibling");
    let expected_mode = fs::metadata(&ordinary)
        .expect("ordinary sibling metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_ne!(
        expected_mode, 0o600,
        "fixture requires a default mode distinct from a restricted segment"
    );

    let vectors = [1.0_f32, 0.0, 0.0, 1.0, -1.0, 0.0, 0.0, -1.0];
    let config = VamanaConfig::with_dimensions(2)
        .with_max_degree(2)
        .with_search_list_size(4);
    let index = VamanaIndex::build(&vectors, config).expect("build synthetic owned index");
    index.save(dir.path()).expect("initial legacy save");
    for name in ["vectors.bin", "graph.bin", "metadata.bin"] {
        let actual = fs::metadata(dir.path().join(name))
            .expect("published segment")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(actual, expected_mode, "new segment mode differs for {name}");
    }
}
