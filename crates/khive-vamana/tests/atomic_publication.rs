#![cfg(all(unix, feature = "mmap"))]

use std::fs;
use std::io;
use std::os::unix::fs::symlink;

use khive_vamana::{
    write_auxiliary_sidecar_atomic, write_external_ids_sidecar, AuxiliarySidecarCleaner,
    AuxiliarySidecarReader, ExternalIdsWriteError, VamanaConfig, VamanaError, VamanaIndex,
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

#[test]
fn sidecar_reader_and_cleaner_preserve_component_policy_and_missing_entry_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("precious");
    fs::write(&target, b"outside bytes").unwrap();
    fs::write(dir.path().join("pack.bin"), b"sidecar bytes").unwrap();
    symlink(&target, dir.path().join("linked.bin")).unwrap();
    fs::create_dir(dir.path().join("directory.bin")).unwrap();
    let reader = AuxiliarySidecarReader::open(dir.path()).unwrap();
    let cleaner = AuxiliarySidecarCleaner::open(dir.path()).unwrap();

    assert_eq!(
        reader.read_bounded("pack.bin", 13).unwrap(),
        Some(b"sidecar bytes".to_vec())
    );
    assert_eq!(
        reader.read_prefix("pack.bin", 7).unwrap(),
        Some(b"sidecar".to_vec())
    );
    assert_eq!(reader.read_bounded("missing.bin", 13).unwrap(), None);
    assert_eq!(reader.read_prefix("missing.bin", 7).unwrap(), None);
    cleaner.remove_and_sync("missing.bin").unwrap();
    assert!(reader
        .read_bounded("linked.bin", 13)
        .unwrap_err()
        .raw_os_error()
        .is_some());
    let error = reader.read_bounded("directory.bin", 13).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        error.to_string(),
        "checkpoint sidecar is not a regular file"
    );
    assert!(cleaner
        .remove_and_sync("directory.bin")
        .unwrap_err()
        .raw_os_error()
        .is_some());
    assert!(dir.path().join("directory.bin").is_dir());
    assert_eq!(
        reader.read_bounded("pack.bin", 12).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );

    let mut before_names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    before_names.sort();
    for invalid in ["", ".", "..", "a/b", "a\\b", "nul\0suffix"] {
        assert_eq!(
            reader.read_bounded(invalid, 13).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            reader.read_prefix(invalid, 7).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            cleaner.remove_and_sync(invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let mut after_names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        after_names.sort();
        assert_eq!(after_names, before_names);
        assert_eq!(
            fs::read(dir.path().join("pack.bin")).unwrap(),
            b"sidecar bytes"
        );
        assert_eq!(
            fs::read_link(dir.path().join("linked.bin")).unwrap(),
            target
        );
        assert!(dir.path().join("directory.bin").is_dir());
        assert_eq!(fs::read(&target).unwrap(), b"outside bytes");
    }
    cleaner.remove_and_sync("linked.bin").unwrap();
    assert!(fs::symlink_metadata(dir.path().join("linked.bin")).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"outside bytes");
    cleaner.remove_and_sync("pack.bin").unwrap();
    assert_eq!(reader.read_bounded("pack.bin", 13).unwrap(), None);
}
