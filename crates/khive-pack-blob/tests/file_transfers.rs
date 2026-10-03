use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use khive_db::stores::blob::FsBlobStore;
use khive_pack_blob as _;
use khive_runtime::{KhiveRuntime, PackRegistry, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{BlobStore, ContentRef, StorageError, StorageResult};
use serde_json::json;

static FILE_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Roots {
    directory: tempfile::TempDir,
    old_import: Option<OsString>,
    old_export: Option<OsString>,
    old_transfers: Option<OsString>,
}

impl Roots {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let old_import = std::env::var_os("KHIVE_IMPORT_FROM_ROOT");
        let old_export = std::env::var_os("KHIVE_SAVE_TO_ROOT");
        let old_transfers = std::env::var_os("KHIVE_FILE_TRANSFERS");
        std::env::set_var("KHIVE_FILE_TRANSFERS", "1");
        std::env::set_var("KHIVE_IMPORT_FROM_ROOT", root.join("imports"));
        std::env::set_var("KHIVE_SAVE_TO_ROOT", root.join("exports"));
        std::fs::create_dir_all(root.join("imports")).unwrap();
        std::fs::create_dir_all(root.join("exports")).unwrap();
        Self {
            directory,
            old_import,
            old_export,
            old_transfers,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().canonicalize().unwrap().join(name)
    }
}

impl Drop for Roots {
    fn drop(&mut self) {
        for (name, old) in [
            ("KHIVE_IMPORT_FROM_ROOT", &self.old_import),
            ("KHIVE_SAVE_TO_ROOT", &self.old_export),
            ("KHIVE_FILE_TRANSFERS", &self.old_transfers),
        ] {
            match old {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn registry(roots: &Roots) -> (VerbRegistry, Arc<FsBlobStore>) {
    let store = Arc::new(FsBlobStore::new(roots.path("cas"), 0).unwrap());
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_blob_store(store.clone()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&["blob".into()], runtime, &mut builder).unwrap();
    (builder.build().unwrap(), store)
}

async fn import_refusal(registry: &VerbRegistry, path: &Path, reason: &str) {
    let error = registry
        .dispatch("blob.import", json!({"path": path}))
        .await
        .expect_err("confined import refusal");
    let text = error.to_string();
    assert!(text.contains(reason), "specific import refusal: {text}");
    assert!(
        !text.contains("unknown verb"),
        "an absent verb does not prove path confinement"
    );
}

#[tokio::test]
async fn server_file_import_stat_export_round_trip() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _store) = registry(&roots);
    let bytes: Vec<u8> = (0..200_000).map(|index| (index % 251) as u8).collect();
    std::fs::write(roots.path("imports/original.bin"), &bytes).unwrap();
    let imported = registry
        .dispatch(
            "blob.import",
            json!({"path": "original.bin", "media_type": "application/octet-stream"}),
        )
        .await
        .unwrap();
    assert_eq!(
        imported["content_ref"],
        blake3::hash(&bytes).to_hex().as_str()
    );
    assert_eq!(imported["size"], bytes.len());
    assert_eq!(imported["media_type"], "application/octet-stream");
    assert!(imported.get("bytes").is_none());
    let stat = registry
        .dispatch("blob.stat", json!({"content_ref": imported["content_ref"]}))
        .await
        .unwrap();
    assert_eq!(stat["size"], bytes.len());
    assert_eq!(stat["exists"], true);
    std::fs::write(roots.path("exports/copy.bin"), b"old destination").unwrap();
    let exported = registry
        .dispatch(
            "blob.export",
            json!({"content_ref": imported["content_ref"], "path": "copy.bin"}),
        )
        .await
        .unwrap();
    assert_eq!(exported["size"], bytes.len());
    assert!(exported.get("bytes").is_none());
    assert_eq!(
        std::fs::read(roots.path("exports/copy.bin")).unwrap(),
        bytes
    );
    assert_eq!(
        exported["path"],
        roots.path("exports/copy.bin").to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn import_outside_root_refuses() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _) = registry(&roots);
    let outside = roots.path("outside.bin");
    std::fs::write(&outside, b"outside file").unwrap();
    import_refusal(&registry, &outside, "escapes the allowed import root").await;
}

#[tokio::test]
async fn missing_outside_import_refuses_before_path_inspection() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _) = registry(&roots);
    let existing = roots.path("outside.bin");
    let missing = roots.path("missing-outside.bin");
    std::fs::write(&existing, b"outside file").unwrap();
    assert!(!missing.exists());
    let expected = format!(
        "blob.import: import path escapes the allowed import root ({})",
        roots.path("imports").display()
    );
    for outside in [&existing, &missing] {
        let error = registry
            .dispatch("blob.import", json!({"path": outside}))
            .await
            .expect_err("outside paths must refuse before inspection");
        match error {
            khive_runtime::RuntimeError::InvalidInput(text) => assert_eq!(text, expected),
            other => panic!("expected confined-path InvalidInput, got {other}"),
        }
    }
    assert!(!missing.exists());
    assert_eq!(std::fs::read(&existing).unwrap(), b"outside file");
    assert_eq!(std::fs::read_dir(roots.path("cas")).unwrap().count(), 0);
}

#[tokio::test]
async fn import_traversal_directory_and_size_refuse() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _) = registry(&roots);
    import_refusal(&registry, Path::new("../outside.bin"), "'..' traversal").await;
    import_refusal(&registry, &roots.path("imports"), "regular file").await;
    std::fs::File::create(roots.path("imports/oversize.bin"))
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    import_refusal(&registry, Path::new("oversize.bin"), "maximum").await;
}

#[cfg(unix)]
#[tokio::test]
async fn import_symlinks_inside_and_outside_root_refuse() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, _) = registry(&roots);
    std::fs::write(roots.path("imports/original.bin"), b"inside target").unwrap();
    std::fs::write(roots.path("outside.bin"), b"outside target").unwrap();
    std::os::unix::fs::symlink(
        roots.path("imports/original.bin"),
        roots.path("imports/inside-link"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        roots.path("outside.bin"),
        roots.path("imports/outside-link"),
    )
    .unwrap();
    // An inside-target link isolates symlink validation from canonical containment.
    import_refusal(
        &registry,
        Path::new("inside-link"),
        "must not contain a symlink",
    )
    .await;
    import_refusal(
        &registry,
        Path::new("outside-link"),
        "must not contain a symlink",
    )
    .await;
    std::fs::create_dir_all(roots.path("imports/subdirectory")).unwrap();
    std::fs::write(
        roots.path("imports/subdirectory/nested.bin"),
        b"nested target",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        roots.path("imports/subdirectory"),
        roots.path("imports/directory-link"),
    )
    .unwrap();
    import_refusal(
        &registry,
        Path::new("directory-link/nested.bin"),
        "must not contain a symlink",
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn export_existing_symlink_refuses_without_replacing_target() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, store) = registry(&roots);
    let reference = store.put(b"exported bytes".to_vec()).await.unwrap();
    let target = roots.path("exports/target.bin");
    std::fs::write(&target, b"preserved target").unwrap();
    std::os::unix::fs::symlink(&target, roots.path("exports/link.bin")).unwrap();
    let error = registry
        .dispatch(
            "blob.export",
            json!({"content_ref": reference, "path": "link.bin"}),
        )
        .await
        .expect_err("export symlink refusal");
    assert!(
        error.to_string().contains("must not be a symlink"),
        "{error}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"preserved target");
}

#[tokio::test]
async fn equal_and_nested_file_roots_refuse_both_verbs() {
    let _guard = FILE_ENV.lock().await;
    let roots = Roots::new();
    let (registry, store) = registry(&roots);
    let reference = store.put(b"root overlap".to_vec()).await.unwrap();
    for (imports, exports) in [
        ("shared", "shared"),
        ("shared/inner", "shared"),
        ("shared", "shared/inner"),
    ] {
        std::env::set_var("KHIVE_IMPORT_FROM_ROOT", roots.path(imports));
        std::env::set_var("KHIVE_SAVE_TO_ROOT", roots.path(exports));
        for (verb, params) in [
            ("blob.import", json!({"path": "input.bin"})),
            (
                "blob.export",
                json!({"content_ref": reference, "path": "output.bin"}),
            ),
        ] {
            let error = registry
                .dispatch(verb, params)
                .await
                .expect_err("equal or nested roots must refuse");
            assert!(
                error.to_string().contains("must not be equal or nested"),
                "{error}"
            );
        }
    }
}

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
            resolved.embedding_model = None;
            resolved.additional_embedding_models.clear();
            let runtime = KhiveRuntime::new(resolved).unwrap();
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
