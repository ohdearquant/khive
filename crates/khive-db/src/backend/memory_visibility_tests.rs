use super::*;

#[test]
fn memory_visibility_readiness_uses_only_the_reader_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("visibility_readiness.db");
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        writable.prepare_core_schema().unwrap();
        writable.validate_memory_visibility_cutover().unwrap();
    }
    let read_only = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let before = read_only.pool().writer_acquisition_snapshot();
    read_only.validate_memory_visibility_cutover().unwrap();
    assert_eq!(read_only.pool().writer_acquisition_snapshot(), before);
    drop(read_only);
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        let writer = writable.pool().writer().unwrap();
        writer
            .conn()
            .execute_batch("DROP TABLE memory_visibility_epochs")
            .unwrap();
    }
    let read_only = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let before = read_only.pool().writer_acquisition_snapshot();
    assert!(read_only.validate_memory_visibility_cutover().is_err());
    assert_eq!(read_only.pool().writer_acquisition_snapshot(), before);
}
