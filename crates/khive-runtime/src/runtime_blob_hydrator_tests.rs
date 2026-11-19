#[test]
fn installed_blob_hydrator_is_shared_by_clone_and_core_handles() {
    let main_backend = Arc::new(StorageBackend::memory().expect("main backend"));
    let pack_backend = Arc::new(StorageBackend::memory().expect("pack backend"));
    let mut config = RuntimeConfig::no_embeddings();
    config.backend_id = BackendId::parse("assets").expect("valid backend id");
    let runtime = KhiveRuntime::from_backend(pack_backend, config)
        .with_core_backend(Arc::clone(&main_backend));
    let (_root, hydrator) = test_blob_hydrator();

    runtime
        .install_blob_hydrator(Arc::clone(&hydrator))
        .expect("first install");

    for handle in [runtime.clone(), runtime.core()] {
        let installed = handle.blob_hydrator().expect("installed hydrator");
        assert!(Arc::ptr_eq(&installed, &hydrator));
    }
}

#[test]
fn blob_hydrator_install_is_idempotent_but_rejects_replacement() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let (_first_root, first) = test_blob_hydrator();
    let (_second_root, second) = test_blob_hydrator();

    runtime
        .install_blob_hydrator(Arc::clone(&first))
        .expect("first install");
    runtime
        .install_blob_hydrator(Arc::clone(&first))
        .expect("same Arc reinstall is idempotent");

    let error = runtime
        .install_blob_hydrator(second)
        .expect_err("a different hydrator must not replace the installed pair");
    assert!(error.to_string().contains("already installed"));
    assert!(Arc::ptr_eq(
        &runtime.blob_hydrator().expect("original remains"),
        &first
    ));
}

#[test]
fn blob_hydrator_install_rejects_a_budget_that_disagrees_with_runtime_identity() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let root = tempfile::tempdir().expect("blob root");
    let store = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(root.path().to_path_buf(), 0)
            .expect("fs blob store"),
    );
    let mismatched = Arc::new(
        crate::BlobHydrator::new(store, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .expect("minimum blob budget"),
    );

    let error = runtime
        .install_blob_hydrator(mismatched)
        .expect_err("live admission must match the construction-baked config identity");
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    assert!(runtime.blob_hydrator().is_none());
}

#[test]
fn require_blob_accessors_refuse_with_the_operator_message_when_no_store_is_installed() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let expected = concat!(
        "no BlobStore installed on this server ",
        "(configure [storage.blob] in khive.toml, or KHIVE_BLOB_ROOT)"
    );

    let Err(RuntimeError::Unconfigured(message)) = runtime.require_blob_store() else {
        panic!("require_blob_store must refuse when no store is installed");
    };
    assert_eq!(message, expected);

    let Err(RuntimeError::Unconfigured(message)) = runtime.require_blob_hydrator() else {
        panic!("require_blob_hydrator must refuse when no store is installed");
    };
    assert_eq!(message, expected);
}

#[test]
fn require_blob_accessors_return_the_installed_store_and_hydrator() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let root = tempfile::tempdir().expect("blob root");
    let store = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(root.path().to_path_buf(), 0)
            .expect("fs blob store"),
    );
    runtime.install_blob_store(store).expect("install");

    let required_store = runtime.require_blob_store().expect("store");
    let installed_store = runtime.blob_store().expect("installed store");
    assert!(Arc::ptr_eq(&required_store, &installed_store));
    let required_hydrator = runtime.require_blob_hydrator().expect("hydrator");
    let installed_hydrator = runtime.blob_hydrator().expect("installed hydrator");
    assert!(Arc::ptr_eq(&required_hydrator, &installed_hydrator));
}
