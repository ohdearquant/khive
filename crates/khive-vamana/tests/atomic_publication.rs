#![cfg(all(unix, feature = "mmap"))]

use std::fs;
use std::io;
use std::os::unix::fs::symlink;

use khive_vamana::{
    write_auxiliary_sidecar_atomic, write_external_ids_sidecar, ExternalIdsWriteError,
    VamanaConfig, VamanaError, VamanaIndex,
};

#[test]
fn later_staging_failure_preserves_the_entire_committed_generation() {
    let config = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    let vectors = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    let original = VamanaIndex::build(&vectors, config.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    original.save_atomic(dir.path()).unwrap();
    let names = [
        "metadata.bin",
        "vectors.bin",
        "graph.bin",
        "lifecycle.bin",
        "codes.bin",
    ];
    let before: Vec<_> = names
        .iter()
        .map(|name| fs::read(dir.path().join(name)).unwrap())
        .collect();

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("precious");
    fs::write(&target, b"outside bytes").unwrap();
    let blocked = dir.path().join("graph.bin.v2new");
    symlink(&target, &blocked).unwrap();
    let changed_vectors: Vec<_> = vectors.iter().map(|value| -value).collect();
    let changed = VamanaIndex::build(&changed_vectors, config).unwrap();
    let error = changed.save_atomic(dir.path()).unwrap_err();
    let VamanaError::Io { source } = error else {
        panic!("expected staging I/O refusal, got {error:?}");
    };
    assert_eq!(source.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        source.to_string(),
        "checkpoint staging entry is not a regular file"
    );
    assert_ne!(
        fs::read(dir.path().join("vectors.bin.v2new")).unwrap(),
        before[1],
        "first changed segment really staged before the later failure"
    );
    for (name, bytes) in names.iter().zip(&before) {
        assert_eq!(
            &fs::read(dir.path().join(name)).unwrap(),
            bytes,
            "committed {name} changed before metadata commit"
        );
    }
    assert_eq!(fs::read(&target).unwrap(), b"outside bytes");
    let query = [1.0, 0.0, 0.0, 0.0];
    assert_eq!(
        VamanaIndex::load(dir.path())
            .unwrap()
            .search(&query, 4)
            .unwrap(),
        original.search(&query, 4).unwrap()
    );

    fs::remove_file(blocked).unwrap();
    changed.save_atomic(dir.path()).unwrap();
    assert_ne!(fs::read(dir.path().join("vectors.bin")).unwrap(), before[1]);
    assert_eq!(
        VamanaIndex::load(dir.path())
            .unwrap()
            .search(&query, 4)
            .unwrap(),
        changed.search(&query, 4).unwrap()
    );
}

#[test]
fn auxiliary_publication_preserves_refusal_and_regular_stale_replacement() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("pack.bin"), b"old final").unwrap();
    fs::write(dir.path().join("target"), b"precious").unwrap();
    symlink("target", dir.path().join("pack.bin.tmp")).unwrap();
    let error = write_auxiliary_sidecar_atomic(dir.path(), "pack.bin", b"new final").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        error.to_string(),
        "checkpoint staging entry is not a regular file"
    );
    assert_eq!(fs::read(dir.path().join("pack.bin")).unwrap(), b"old final");
    assert_eq!(fs::read(dir.path().join("target")).unwrap(), b"precious");
    fs::remove_file(dir.path().join("pack.bin.tmp")).unwrap();
    fs::write(dir.path().join("pack.bin.tmp"), b"stale regular").unwrap();
    write_auxiliary_sidecar_atomic(dir.path(), "pack.bin", b"new final").unwrap();
    assert_eq!(fs::read(dir.path().join("pack.bin")).unwrap(), b"new final");
    assert!(!dir.path().join("pack.bin.tmp").exists());
    for invalid in ["", ".", "..", "a/b", "a\\b", "nul\0suffix"] {
        assert_eq!(
            write_auxiliary_sidecar_atomic(dir.path(), invalid, b"bad")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn external_id_remove_and_rename_failures_keep_context_and_native_source() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("external_ids.bin.tmp")).unwrap();
    let error = write_external_ids_sidecar(dir.path(), &[7; 32], &[]).unwrap_err();
    let ExternalIdsWriteError::Io { context, source } = error else {
        panic!("expected tmp unlink failure, got {error:?}");
    };
    assert_eq!(context, "remove stale external_ids.bin.tmp");
    assert!(source.raw_os_error().is_some());
    assert!(dir.path().join("external_ids.bin.tmp").is_dir());

    fs::remove_dir(dir.path().join("external_ids.bin.tmp")).unwrap();
    fs::create_dir(dir.path().join("external_ids.bin")).unwrap();
    fs::write(dir.path().join("external_ids.bin/child"), b"precious").unwrap();
    let error = write_external_ids_sidecar(dir.path(), &[7; 32], &[]).unwrap_err();
    let ExternalIdsWriteError::Io { context, source } = error else {
        panic!("expected rename failure, got {error:?}");
    };
    assert_eq!(context, "rename external_ids.bin.tmp -> external_ids.bin");
    assert!(source.raw_os_error().is_some());
    assert!(!fs::read(dir.path().join("external_ids.bin.tmp"))
        .unwrap()
        .is_empty());
    assert_eq!(
        fs::read(dir.path().join("external_ids.bin/child")).unwrap(),
        b"precious"
    );
}
