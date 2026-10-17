#[derive(Debug, Default)]
struct WholeBufferOnlyStore {
    puts: AtomicUsize,
}

#[async_trait]
impl BlobStore for WholeBufferOnlyStore {
    async fn put(&self, _bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.puts.fetch_add(1, Ordering::Relaxed);
        Err(StorageError::Internal(
            "whole-file put must not be used".into(),
        ))
    }
    async fn get_bounded_verified(
        &self,
        _reference: &ContentRef,
        _max: u64,
    ) -> StorageResult<Vec<u8>> {
        Err(StorageError::Internal("unused fixture read".into()))
    }
    async fn exists(&self, _reference: &ContentRef) -> StorageResult<bool> {
        Ok(false)
    }
    async fn size(&self, _reference: &ContentRef) -> StorageResult<Option<u64>> {
        Ok(None)
    }
    async fn delete(&self, _reference: &ContentRef) -> StorageResult<bool> {
        Ok(false)
    }
}

#[tokio::test]
async fn unsupported_staged_import_refuses_without_whole_file_put() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    std::fs::write(roots.path("imports/source.bin"), b"stream this file").unwrap();
    let store = Arc::new(WholeBufferOnlyStore::default());
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_blob_store(store.clone()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&["blob".into()], runtime, &mut builder).unwrap();
    let error = builder
        .build()
        .unwrap()
        .dispatch("blob.import", json!({"path": "source.bin"}))
        .await
        .expect_err("unsupported staging must refuse without buffering fallback");
    assert!(
        error.to_string().contains("begin_upload"),
        "specific backend capability refusal: {error}"
    );
    assert_eq!(store.puts.load(Ordering::Relaxed), 0);
}

async fn transfers_disabled(registry: &VerbRegistry, reference: &ContentRef) {
    for (verb, params) in [
        ("blob.import", json!({"path": "source.bin"})),
        (
            "blob.export",
            json!({"content_ref": reference, "path": "output.bin"}),
        ),
    ] {
        let error = registry
            .dispatch(verb, params)
            .await
            .expect_err("file transfers must require explicit opt-in");
        match error {
            khive_runtime::RuntimeError::InvalidInput(text) => {
                assert!(
                    text.contains("server file transfers are disabled"),
                    "{text}"
                );
                assert!(text.contains("[blob] file_transfers = true"), "{text}");
                assert!(text.contains("KHIVE_FILE_TRANSFERS=1"), "{text}");
            }
            other => panic!("expected explicit opt-in InvalidInput, got {other}"),
        }
    }
}

#[tokio::test]
async fn file_transfers_refuse_when_opt_in_is_unset_without_disabling_put() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    std::env::remove_var("KHIVE_FILE_TRANSFERS");
    std::fs::remove_dir(roots.path("imports")).unwrap();
    std::fs::remove_dir(roots.path("exports")).unwrap();
    let (registry, store) = registry(&roots);
    let put = registry
        .dispatch("blob.put", json!({"bytes": "c3RpbGwgcHV0"}))
        .await
        .unwrap();
    let reference = ContentRef::from_hex(put["content_ref"].as_str().unwrap()).unwrap();
    assert_eq!(
        put["content_ref"],
        blake3::hash(b"still put").to_hex().as_str()
    );
    assert_eq!(store.size(&reference).await.unwrap(), Some(9));
    transfers_disabled(&registry, &reference).await;
    assert!(!roots.path("imports").exists());
    assert!(!roots.path("exports").exists());
}

#[tokio::test]
async fn file_transfer_environment_opt_in_is_exact_and_captured_at_runtime_construction() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    std::fs::write(roots.path("imports/source.bin"), b"captured opt-in").unwrap();
    let (enabled, store) = registry(&roots);
    std::env::remove_var("KHIVE_FILE_TRANSFERS");
    let imported = enabled
        .dispatch("blob.import", json!({"path": "source.bin"}))
        .await
        .unwrap();
    let reference = ContentRef::from_hex(imported["content_ref"].as_str().unwrap()).unwrap();
    enabled
        .dispatch(
            "blob.export",
            json!({"content_ref": reference, "path": "captured.bin"}),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(roots.path("exports/captured.bin")).unwrap(),
        b"captured opt-in"
    );
    assert_eq!(store.size(&reference).await.unwrap(), Some(15));
    let disabled_runtime = KhiveRuntime::memory().unwrap();
    disabled_runtime.install_blob_store(store).unwrap();
    std::env::set_var("KHIVE_FILE_TRANSFERS", "1");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&["blob".into()], disabled_runtime, &mut builder).unwrap();
    transfers_disabled(&builder.build().unwrap(), &reference).await;
    for value in ["", "0", "true", " 1", "1 "] {
        std::env::set_var("KHIVE_FILE_TRANSFERS", value);
        let (registry, _) = registry(&roots);
        transfers_disabled(&registry, &reference).await;
    }
}

#[tokio::test]
async fn configured_file_transfer_opt_in_reaches_both_runtime_conversion_paths() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    std::env::remove_var("KHIVE_FILE_TRANSFERS");
    std::fs::write(roots.path("imports/source.bin"), b"configured opt-in").unwrap();
    for (index, engines) in [
        "",
        "[[engines]]\nname = \"default\"\nmodel = \"all-minilm-l6-v2\"\ndefault = true\n",
    ]
    .into_iter()
    .enumerate()
    {
        for (opt_in, environment) in [(true, None), (true, Some("0")), (false, Some("1"))] {
            match environment {
                Some(value) => std::env::set_var("KHIVE_FILE_TRANSFERS", value),
                None => std::env::remove_var("KHIVE_FILE_TRANSFERS"),
            }
            let path = roots.path(&format!("config-{index}.toml"));
            std::fs::write(
                &path,
                format!("{engines}[blob]\nfile_transfers = {opt_in}\n"),
            )
            .unwrap();
            let config = khive_runtime::KhiveConfig::load(Some(&path))
                .unwrap()
                .unwrap();
            let mut resolved = khive_runtime::runtime_config_from_khive_config(
                &config,
                khive_runtime::RuntimeConfig::no_embeddings(),
            );
            resolved.db_path = None;
            resolved.disable_embedding_models();
            let runtime = durable_runtime(&roots, resolved);
            let store = Arc::new(FsBlobStore::new(roots.path(&format!("cas-{index}")), 0).unwrap());
            runtime.install_blob_store(store).unwrap();
            let mut builder = VerbRegistryBuilder::new();
            PackRegistry::register_packs(&["blob".into()], runtime, &mut builder).unwrap();
            let registry = builder.build().unwrap();
            let imported = registry
                .dispatch("blob.import", json!({"path": "source.bin"}))
                .await
                .unwrap();
            registry.dispatch("blob.export", json!({"content_ref": imported["content_ref"], "path": format!("configured-{index}.bin")})).await.unwrap();
            assert_eq!(
                std::fs::read(roots.path(&format!("exports/configured-{index}.bin"))).unwrap(),
                b"configured opt-in"
            );
        }
    }
}
