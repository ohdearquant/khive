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

fn durable_runtime(roots: &Roots, config: khive_runtime::RuntimeConfig) -> KhiveRuntime {
    let backend =
        Arc::new(khive_db::StorageBackend::sqlite_for_test(roots.path("main.db")).unwrap());
    backend.prepare_core_schema().unwrap();
    KhiveRuntime::from_backend(backend, config)
}

fn registry(roots: &Roots) -> (VerbRegistry, Arc<FsBlobStore>) {
    let store = Arc::new(FsBlobStore::new(roots.path("cas"), 0).unwrap());
    let runtime = durable_runtime(roots, khive_runtime::RuntimeConfig::no_embeddings());
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

include!("file_transfers_tests/transfer_and_root_policy_tests.rs");

include!("file_transfers_tests/transfer_admission_tests.rs");
