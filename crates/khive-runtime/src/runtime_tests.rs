use super::*;
use khive_gate::GateRef;
use serial_test::serial;

#[cfg(target_os = "macos")]
#[test]
fn in_process_runtime_tests_have_4096_open_file_slots() {
    let _runtime = KhiveRuntime::memory().expect("test runtime");
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limits` is a writable local value.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) },
        0
    );
    assert!(
        limits.rlim_cur >= IN_PROCESS_TEST_NOFILE_LIMIT,
        "a parallel runtime suite needs at least 4096 open-file slots"
    );
}

fn test_blob_hydrator() -> (tempfile::TempDir, Arc<crate::BlobHydrator>) {
    let root = tempfile::tempdir().expect("blob root");
    let store = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(root.path().to_path_buf(), 0)
            .expect("fs blob store"),
    );
    let hydrator = Arc::new(
        crate::BlobHydrator::new(store, crate::DEFAULT_BLOB_HYDRATION_BYTES)
            .expect("blob hydrator"),
    );
    (root, hydrator)
}

#[test]
fn memory_runtime_creates_successfully() {
    let rt = KhiveRuntime::memory().expect("memory runtime should create");
    assert!(rt.config().db_path.is_none());
}

include!("runtime_blob_hydrator_tests.rs");
include!("runtime_volume_lock_dir_tests.rs");

#[test]
fn fresh_tail_policy_is_instance_scoped_and_clone_stable() {
    let enabled = KhiveRuntime::memory()
        .expect("enabled memory runtime")
        .with_ann_fresh_tail_enabled(true);
    let disabled = KhiveRuntime::memory()
        .expect("disabled memory runtime")
        .with_ann_fresh_tail_enabled(false);

    assert!(enabled.ann_fresh_tail_enabled());
    assert!(enabled.clone().ann_fresh_tail_enabled());
    assert!(!disabled.ann_fresh_tail_enabled());
    assert!(!disabled.clone().ann_fresh_tail_enabled());
}

include!("runtime_db_diagnostics_counter_tests.rs");

#[test]
fn diagnostics_tracks_late_events_sidecar_without_retaining_its_pool() {
    let dir = tempfile::tempdir().expect("diagnostics database directory");
    let guard = crate::events_split::TestRegistryGuard::new(dir.path());
    let sidecar_path = dir.path().join("main.db.events.db");
    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = Some(dir.path().join("main.db"));
    config.events_split = Some(crate::events_split::EventsSplitConfig {
        db_path: sidecar_path.clone(),
        socket_path: None,
    });
    let runtime = KhiveRuntime::new_for_test(config).expect("main runtime");
    let clone = runtime.clone();

    assert!(!sidecar_path.exists());
    assert_eq!(
        runtime
            .diagnostic_backends()
            .iter()
            .filter(|backend| backend.canonical_path.as_deref() == Some(sidecar_path.as_path()))
            .count(),
        0,
        "an unopened sidecar must not appear in diagnostics"
    );
    assert!(runtime
        .events_sidecar_sql_read_only()
        .expect("missing sidecar lookup")
        .is_none());
    assert!(
        !sidecar_path.exists(),
        "inspection must not create the sidecar"
    );

    std::fs::File::create(&sidecar_path).expect("preexisting events sidecar");
    let sql = runtime
        .events_sidecar_sql_read_only()
        .expect("sidecar SQL lookup")
        .expect("preexisting sidecar opens");
    let canonical_sidecar = sidecar_path.canonicalize().expect("canonical sidecar path");
    let snapshot = clone.diagnostic_backends();
    let entries: Vec<_> = snapshot
        .iter()
        .filter(|backend| backend.canonical_path.as_deref() == Some(canonical_sidecar.as_path()))
        .collect();
    assert_eq!(entries.len(), 1, "sidecar opens after runtime composition");
    assert_eq!(entries[0].backend_names, vec!["events".to_string()]);
    let weak_sidecar_pool = Arc::downgrade(&entries[0].pool);

    let event_store = runtime
        .raw_events_for_namespace("local")
        .expect("direct events store");
    let repeated = runtime.diagnostic_backends();
    assert_eq!(
            repeated
                .iter()
                .filter(|backend| backend.canonical_path.as_deref()
                    == Some(canonical_sidecar.as_path()))
                .count(),
            1,
            "the SQL and event-store paths must report one physical file"
        );

    let boot_alias =
        StorageBackend::sqlite_for_test(dir.path().join(".").join("main.db.events.db"))
            .expect("second pool for canonical-file alias");
    let boot_alias_pool = boot_alias.pool_arc();
    let main_pool = runtime.backend().pool_arc();
    let with_boot_alias = runtime.clone().with_diagnostic_backends(
        vec![
            OpenedDiagnosticBackend {
                backend_names: vec!["main".into()],
                canonical_path: main_pool.canonical_path().map(PathBuf::from),
                pool: main_pool,
            },
            OpenedDiagnosticBackend {
                backend_names: vec!["boot_alias".into()],
                canonical_path: boot_alias_pool.canonical_path().map(PathBuf::from),
                pool: Arc::clone(&boot_alias_pool),
            },
        ]
        .into(),
    );
    let with_boot_alias_snapshot = with_boot_alias.diagnostic_backends();
    let merged: Vec<_> = with_boot_alias_snapshot
        .iter()
        .filter(|backend| backend.canonical_path.as_deref() == Some(canonical_sidecar.as_path()))
        .collect();
    assert_eq!(merged.len(), 1, "two pools over one canonical file");
    assert_eq!(
        merged[0].backend_names,
        vec!["boot_alias".to_string(), "events".to_string()]
    );
    assert!(Arc::ptr_eq(&merged[0].pool, &boot_alias_pool));

    drop(with_boot_alias_snapshot);
    drop(with_boot_alias);
    drop(boot_alias_pool);
    drop(boot_alias);
    drop(repeated);
    drop(snapshot);
    drop(guard);
    assert!(weak_sidecar_pool.upgrade().is_some());
    assert!(runtime
        .diagnostic_backends()
        .iter()
        .any(|backend| { backend.canonical_path.as_deref() == Some(canonical_sidecar.as_path()) }));

    drop(event_store);
    drop(sql);
    assert!(weak_sidecar_pool.upgrade().is_none());
    assert_eq!(
            runtime
                .diagnostic_backends()
                .iter()
                .filter(|backend| backend.canonical_path.as_deref()
                    == Some(canonical_sidecar.as_path()))
                .count(),
            0,
            "diagnostics must not retain a pool after its owner releases it"
        );
}

#[test]
fn diagnostics_tracks_direct_events_open_after_runtime_composition() {
    let dir = tempfile::tempdir().expect("diagnostics database directory");
    let guard = crate::events_split::TestRegistryGuard::new(dir.path());
    let sidecar_path = dir.path().join("main.db.events.db");
    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = Some(dir.path().join("main.db"));
    config.events_split = Some(crate::events_split::EventsSplitConfig {
        db_path: sidecar_path.clone(),
        socket_path: None,
    });
    let runtime = KhiveRuntime::new_for_test(config.clone()).expect("main runtime");

    assert!(!sidecar_path.exists());
    let events = runtime
        .raw_events_for_namespace("local")
        .expect("direct event store opens sidecar");
    let canonical_sidecar = sidecar_path.canonicalize().expect("canonical sidecar path");
    let opened = runtime.clone().diagnostic_backends();
    let matching: Vec<_> = opened
        .iter()
        .filter(|backend| backend.canonical_path.as_deref() == Some(canonical_sidecar.as_path()))
        .collect();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].backend_names, vec!["events".to_string()]);
    let weak_sidecar_pool = Arc::downgrade(&matching[0].pool);

    let pack_runtime = KhiveRuntime::memory()
        .expect("pack runtime")
        .with_diagnostic_observer_from(&runtime);
    assert!(pack_runtime
        .diagnostic_backends()
        .iter()
        .any(|backend| { backend.canonical_path.as_deref() == Some(canonical_sidecar.as_path()) }));

    let unrelated = KhiveRuntime::new_for_test(config).expect("independent runtime");
    assert_eq!(
        unrelated.diagnostic_backends().len(),
        1,
        "an independent runtime must not inherit another runtime's events pool"
    );

    drop(opened);
    drop(guard);
    assert!(weak_sidecar_pool.upgrade().is_some());
    assert!(runtime
        .diagnostic_backends()
        .iter()
        .any(|backend| { backend.canonical_path.as_deref() == Some(canonical_sidecar.as_path()) }));

    drop(events);
    assert!(weak_sidecar_pool.upgrade().is_none());
    assert!(runtime
        .diagnostic_backends()
        .iter()
        .all(|backend| backend.canonical_path.as_deref() != Some(canonical_sidecar.as_path())));
}

#[test]
fn backend_data_dir_returns_none_for_memory_backend() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    assert!(rt.backend_data_dir().is_none());
}

#[test]
fn backend_data_dir_returns_parent_dir_for_file_backend() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let config = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: Some(path),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new_for_test(config).expect("file runtime");
    let data_dir = rt
        .backend_data_dir()
        .expect("file backend must return Some");
    assert_eq!(data_dir, dir.path());
}

/// A sidecar-only event must resolve through the public hex-prefix path:
/// the main-store scan cannot see lane rows, so `resolve_prefix_inner`
/// carries a sidecar arm. The pre-insert assert is the control — the
/// prefix misses until the lane row exists, so a pass cannot come from
/// the legacy scan.
#[tokio::test]
async fn resolve_prefix_finds_sidecar_only_event() {
    let dir = tempfile::tempdir().unwrap();
    let _registry_guard = crate::events_split::TestRegistryGuard::new(dir.path());
    let sidecar_path = dir.path().join("main.db.events.db");
    let config = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: Some(crate::events_split::EventsSplitConfig {
            db_path: sidecar_path.clone(),
            socket_path: None,
        }),
        db_path: Some(dir.path().join("main.db")),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new_for_test(config).expect("file runtime");

    let event = khive_storage::Event::new(
        "local",
        "memory.recall",
        khive_types::EventKind::RecallExecuted,
        khive_types::SubstrateKind::Note,
        "agent:test",
    );
    let event_id = event.id;
    let prefix = event_id.to_string()[..8].to_string();

    assert_eq!(
        rt.resolve_prefix_unfiltered(&prefix)
            .await
            .expect("pre-insert resolve"),
        None,
        "control: prefix must miss before the lane row exists"
    );

    // One process opens the lane with one set of policies: the test
    // runtime's pool carries the test lock directory, so the lane uses it.
    let lane = crate::events_split::direct_backend_with_policies(
        &sidecar_path,
        false,
        None,
        rt.events_wal_ceiling_policy(),
        Some(rt.events_disk_guard_policy().expect("events disk policy")),
        rt.events_volume_lock_dir(),
    )
    .expect("lane backend")
    .events_for_namespace("local")
    .expect("lane store");
    lane.append_event(event).await.expect("lane append");

    assert_eq!(
        rt.resolve_prefix_unfiltered(&prefix)
            .await
            .expect("post-insert resolve"),
        Some(event_id),
        "a sidecar-only event id must resolve by hex prefix"
    );
}

#[test]
fn backend_data_dir_returns_none_for_from_backend_with_memory() {
    let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
    let config = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::from_backend(backend, config);
    assert!(rt.backend_data_dir().is_none());
}

#[test]
fn direct_memory_runtime_rejects_nonzero_wal_ceiling() {
    let config = RuntimeConfig {
        db_path: None,
        wal_ceiling_bytes: 4152,
        wal_ceiling_configured_bytes: 4152,
        ..RuntimeConfig::no_embeddings()
    };
    let error = match KhiveRuntime::new(config) {
        Ok(_) => panic!("memory has no WAL extent"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        RuntimeError::Sqlite(khive_db::SqliteError::InvalidConfig(_))
    ));
}

#[test]
fn file_runtime_creates_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let config = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: Some(path.clone()),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new_for_test(config).expect("file runtime should create");
    assert!(path.exists());
    assert_eq!(rt.config().default_namespace.as_str(), "test");
}

#[cfg(unix)]
#[tokio::test]
async fn normal_boot_detects_read_only_snapshot_and_skips_model_registration() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read_only_runtime.db");
    let base = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: Some(path.clone()),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    {
        let writable = KhiveRuntime::new_for_test(base.clone()).expect("create migrated snapshot");
        assert!(writable
            .list_embedding_models(None)
            .await
            .expect("registry query")
            .is_empty());
    }

    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&path, permissions).unwrap();
    // A lingering writable `-shm` from the writable fixture's asynchronous
    // connection close is rejected by read-only admission as potentially
    // live; freeze any sidecars into the documented frozen-snapshot form.
    khive_storage::test_support::freeze_snapshot_sidecars(&path);

    let read_only_config = RuntimeConfig {
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        ..base
    };
    let runtime = KhiveRuntime::new_for_test(read_only_config)
        .expect("read-only boot must validate instead of migrating/registering");
    assert!(runtime.is_read_only());
    assert_eq!(
        runtime.backend().pool().writer_acquisition_snapshot(),
        khive_db::pool::WriterAcquisitionSnapshot::default(),
        "the construction-inclusive acquisition baseline must stay at zero"
    );
    assert!(
        runtime
            .list_embedding_models(None)
            .await
            .expect("read-only registry query")
            .is_empty(),
        "configured models must remain in-memory only during read-only boot"
    );
}

#[test]
fn explicit_readonly_constructor_uses_read_only_pool_even_on_writable_file_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("explicit_read_only_runtime.db");
    let config = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: Some(path.clone()),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    KhiveRuntime::new_for_test(config.clone()).expect("create migrated database");
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);

    let runtime = KhiveRuntime::new_readonly_for_test(config).expect("explicit read-only boot");
    assert!(runtime.is_read_only());
    assert_eq!(
        runtime.backend().pool().writer_acquisition_snapshot(),
        khive_db::pool::WriterAcquisitionSnapshot::default(),
        "explicit read-only construction must validate through a reader without ever \
             acquiring the writer"
    );
}

/// Grants Write on the primary namespace and Read, but not Write, on the
/// extra namespace. The pseudo-verb split is what lets a right-aware gate
/// express ADR-129's asymmetric authority contract.
#[derive(Debug)]
struct ReadOnlyExtraGate {
    primary: &'static str,
    extra: &'static str,
}

impl khive_gate::Gate for ReadOnlyExtraGate {
    fn check(
        &self,
        req: &khive_gate::GateRequest,
    ) -> Result<khive_gate::GateDecision, khive_gate::GateError> {
        let allowed = match req.verb.as_str() {
            "authorize" => req.namespace.as_str() == self.primary,
            "authorize.visible" => req.namespace.as_str() == self.extra,
            _ => false,
        };
        if allowed {
            Ok(khive_gate::GateDecision::allow())
        } else {
            Ok(khive_gate::GateDecision::Deny {
                reason: format!(
                    "{} denied for namespace {:?}",
                    req.verb,
                    req.namespace.as_str()
                ),
            })
        }
    }
}

#[test]
fn authorize_with_visibility_allows_read_only_extra_namespace() {
    let primary = Namespace::parse("lambda:caller").expect("primary");
    let extra = Namespace::parse("lambda:read-only").expect("extra");
    let config = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: None,
        gate: Arc::new(ReadOnlyExtraGate {
            primary: "lambda:caller",
            extra: "lambda:read-only",
        }),
        ..RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new(config).expect("memory runtime");

    let token = rt
        .authorize_with_visibility(primary, vec![extra.clone()])
        .expect("Write on primary and Read on extra must mint");
    assert!(token.visible_namespaces().contains(&extra));
}

/// Denies exactly one namespace; every other request is allowed. Lets the
/// test below prove a refusal comes from the per-extra visibility check
/// rather than from the primary authorization.
#[derive(Debug)]
struct DenyNamespaceGate {
    deny: &'static str,
}

impl khive_gate::Gate for DenyNamespaceGate {
    fn check(
        &self,
        req: &khive_gate::GateRequest,
    ) -> Result<khive_gate::GateDecision, khive_gate::GateError> {
        if req.namespace.as_str() == self.deny {
            Ok(khive_gate::GateDecision::Deny {
                reason: "namespace denied by policy".to_string(),
            })
        } else {
            Ok(khive_gate::GateDecision::allow())
        }
    }
}

#[test]
fn authorize_with_visibility_denies_missing_extra_read() {
    let config = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: None,
        gate: Arc::new(DenyNamespaceGate {
            deny: "lambda:secret",
        }),
        ..RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new(config).expect("memory runtime");
    let primary = Namespace::parse("lambda:caller").expect("primary");
    let denied = Namespace::parse("lambda:secret").expect("denied");
    let allowed = Namespace::parse("lambda:open").expect("allowed");

    // Control: the same mint without the denied namespace succeeds, so
    // the refusal below can only come from the per-extra check.
    rt.authorize_with_visibility(primary.clone(), vec![allowed.clone()])
        .expect("mint with only allowed extras");

    let err = rt
        .authorize_with_visibility(primary, vec![allowed, denied])
        .expect_err("a denied extra namespace must refuse the whole mint");
    let msg = err.to_string();
    assert!(
        msg.contains("lambda:secret"),
        "refusal must name the offending namespace: {msg}"
    );
}

/// Build a migrated database and reopen it read-only, returning the
/// tempdir that keeps it alive alongside the runtime.
fn make_read_only_runtime() -> (tempfile::TempDir, KhiveRuntime) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read_only_blob_seam.db");
    let config = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        db_path: Some(path.clone()),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        events_split: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    KhiveRuntime::new_for_test(config.clone()).expect("create migrated database");
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);
    let runtime = KhiveRuntime::new_readonly_for_test(config).expect("read-only boot");
    assert!(runtime.is_read_only());
    (dir, runtime)
}

#[tokio::test]
async fn install_blob_store_on_read_only_runtime_refuses_mutators() {
    let (_dir, runtime) = make_read_only_runtime();

    // Control: the raw store IS writable — seed an object through it —
    // so the refusal below can only come from the install-seam wrap.
    use khive_storage::BlobStore as _;
    let blob_root = tempfile::tempdir().unwrap();
    let writable = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0)
            .expect("fs blob store"),
    );
    let seeded = writable
        .put(b"seed".to_vec())
        .await
        .expect("seed put through the raw store");
    runtime
        .install_blob_store(writable.clone())
        .expect("read-only install wraps rather than refusing");

    let installed = runtime.blob_store().expect("installed store");
    assert!(
        installed.exists(&seeded).await.expect("exists"),
        "bounded read surface must stay available"
    );
    let err = installed
        .put(b"post-boot".to_vec())
        .await
        .expect_err("put must refuse on a read-only runtime");
    assert!(
        err.to_string().contains("read-only"),
        "refusal must name the mode: {err}"
    );

    // Reinstalling the SAME raw store stays idempotent even though the
    // installed hydrator holds a wrapper around it: identity is checked
    // against the raw handle the hydrator remembers.
    runtime
        .install_blob_store(writable.clone())
        .expect("reinstalling the same raw store must be idempotent");

    // The hydrator seam holds the mode too: pairing a writable store
    // with `BlobHydrator::new` and installing it directly must refuse,
    // or it is a public bypass of everything above.
    let bypass_root = tempfile::tempdir().unwrap();
    let bypass_store = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(bypass_root.path().to_path_buf(), 0)
            .expect("fs blob store"),
    );
    let writable_hydrator = Arc::new(
        crate::BlobHydrator::new(bypass_store, crate::DEFAULT_BLOB_HYDRATION_BYTES)
            .expect("construct writable hydrator"),
    );
    let err = runtime
        .install_blob_hydrator(writable_hydrator)
        .expect_err("a writable hydrator must be refused on a read-only runtime");
    assert!(
        err.to_string().contains("read-only"),
        "refusal must name the mode: {err}"
    );
}

#[tokio::test]
async fn mode_aware_hydrator_constructor_satisfies_read_only_install() {
    // The boot path constructs its hydrator with `for_mode` from the
    // blob runtime's configured mode; a read-only construction must pass
    // the read-only install gate, serve bounded reads, refuse mutation,
    // and treat a second hydrator allocation over the same raw store as
    // the same pairing (the concurrent-first-install loser shape).
    let (_dir, runtime) = make_read_only_runtime();
    let blob_root = tempfile::tempdir().unwrap();
    let raw = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0)
            .expect("fs blob store"),
    ) as Arc<dyn khive_storage::BlobStore>;
    let seeded = raw.put(b"seed".to_vec()).await.expect("seed put");
    let hydrator = Arc::new(
        crate::BlobHydrator::for_mode(Arc::clone(&raw), crate::DEFAULT_BLOB_HYDRATION_BYTES, true)
            .expect("mode-aware read-only construction"),
    );
    runtime
        .install_blob_hydrator(hydrator)
        .expect("a for_mode(read_only) hydrator must pass the read-only gate");
    let installed = runtime.blob_store().expect("installed store");
    assert!(installed.exists(&seeded).await.expect("exists"));
    installed
        .put(b"post".to_vec())
        .await
        .expect_err("mutation must refuse through the wrapped store");

    // A DISTINCT hydrator allocation over the same raw store, budget,
    // and mode is the same pairing: installing it must be idempotent,
    // not a conflicting-install error.
    let twin = Arc::new(
        crate::BlobHydrator::for_mode(Arc::clone(&raw), crate::DEFAULT_BLOB_HYDRATION_BYTES, true)
            .expect("twin construction"),
    );
    runtime
        .install_blob_hydrator(twin)
        .expect("an equivalent pairing must read as idempotent");
}

#[tokio::test]
async fn shared_install_permits_governed_writable_hydrator_on_read_only_handle() {
    // The documented multi-backend matrix includes a writable blob
    // secondary beside a read-only main: boot installs ONE writable
    // hydrator on every handle, including read-only ones. The plain
    // seam must still refuse that pairing, and the shared seam accepts
    // it ONLY when the hydrator's mode was derived from a governing
    // backend — a hand-paired writable hydrator is refused too, so a
    // safe downstream caller cannot use the shared seam to defeat a
    // read-only handle's guarantee.
    let (_dir, runtime) = make_read_only_runtime();
    let blob_root = tempfile::tempdir().unwrap();
    let raw = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0)
            .expect("fs blob store"),
    ) as Arc<dyn khive_storage::BlobStore>;
    let hand_paired = Arc::new(
        crate::BlobHydrator::for_mode(raw, crate::DEFAULT_BLOB_HYDRATION_BYTES, false)
            .expect("writable construction"),
    );
    runtime
        .install_blob_hydrator(Arc::clone(&hand_paired))
        .expect_err("the plain seam must refuse a writable hydrator on a read-only handle");
    runtime
        .install_shared_blob_hydrator(hand_paired)
        .expect_err("the shared seam must refuse a hand-paired (ungoverned) hydrator");

    // The sanctioned path: a WRITABLE governing backend (the blob pack's
    // backend in the mixed-mode topology) derives a governed writable
    // hydrator, and the shared seam installs it on the read-only handle.
    let governing = khive_db::StorageBackend::memory().expect("memory backend");
    let cfg = crate::KhiveConfig {
        storage: crate::engine_config::StorageSectionConfig {
            blob: Some(crate::engine_config::BlobConfig::Fs {
                root: Some(blob_root.path().to_string_lossy().into_owned()),
                floor_bytes: Some(0),
            }),
        },
        ..crate::KhiveConfig::default()
    };
    let governed = Arc::new(
        crate::BlobHydrator::resolve_for_governing_backend(
            &cfg,
            &governing,
            &governing,
            crate::DEFAULT_BLOB_HYDRATION_BYTES,
        )
        .expect("governed construction"),
    );
    runtime
        .install_shared_blob_hydrator(governed)
        .expect("the shared seam accepts a governed hydrator");
    let installed = runtime.blob_store().expect("installed store");
    let put = installed
        .put(b"shared-write".to_vec())
        .await
        .expect("the governed writable capability must actually mutate");
    assert!(installed.exists(&put).await.expect("exists"));
}

/// A `~/`-prefixed `--db`/`KHIVE_DB` override must resolve, boot, and
/// fingerprint identically to the equivalent absolute path. Before this
/// fix, `resolve_db_anchor` left a leading `~` unexpanded in
/// `RuntimeConfig.db_path`, so single-backend boot (`KhiveRuntime::new`)
/// opened a literal `./~/...` file under the process cwd while
/// `compute_config_id` (which canonicalizes/expands separately) still
/// fingerprinted the real `$HOME` path — two processes pointed at the
/// same logical database could open different files yet share a
/// `config_id`, letting daemon dispatch route requests to the wrong one.
#[test]
#[serial]
fn tilde_prefixed_db_override_resolves_and_boots_like_the_absolute_equivalent() {
    if crate::test_process::run_in_child() {
        return;
    }

    let original_home = std::env::var_os("HOME");
    let original_cwd = std::env::current_dir().expect("read cwd");
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let work_dir = tempfile::tempdir().expect("work tempdir");
    std::env::set_var("HOME", home_dir.path());
    std::env::set_current_dir(work_dir.path()).expect("chdir into isolated work dir");

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let tilde_anchor = crate::config::resolve_db_anchor(Some("~/data.db"))
            .expect("an explicit path always anchors");
        let expected = home_dir.path().join("data.db");
        assert_eq!(
            tilde_anchor, expected,
            "resolve_db_anchor must expand a leading ~ to $HOME before it ever \
                 reaches RuntimeConfig.db_path"
        );

        let absolute_anchor =
            crate::config::resolve_db_anchor(Some(expected.to_str().expect("utf8 tempdir path")))
                .expect("an explicit path always anchors");
        assert_eq!(
            tilde_anchor, absolute_anchor,
            "a ~-prefixed override and its equivalent absolute path must resolve to \
                 the identical anchor"
        );

        let make_config = |db_path: std::path::PathBuf| RuntimeConfig {
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: Some(db_path),
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: khive_db::WalCeilingSource::Default,
            wal_ceiling_env_raw: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..crate::RuntimeConfig::no_embeddings()
        };

        let tilde_cfg = make_config(tilde_anchor.clone());

        let rt = KhiveRuntime::new_for_test(tilde_cfg).expect("boot must open the expanded path");
        assert_eq!(
            rt.backend_data_dir().expect("file backend"),
            home_dir.path(),
            "single-backend boot must open the file under the expanded $HOME \
                 directory, not a literal ~ path relative to cwd"
        );
        assert!(
            expected.exists(),
            "the database file must be created at the expanded $HOME path"
        );
        assert!(
            !work_dir.path().join("~").exists(),
            "boot must never create a literal '~' directory under the process cwd"
        );
    }));

    match &original_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
    let _ = std::env::set_current_dir(&original_cwd);
    outcome.expect("test body panicked");
}

#[test]
fn from_backend_uses_provided_backend() {
    let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
    let config = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::parse("lore").expect("valid backend id"),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::from_backend(backend, config);
    assert_eq!(rt.backend_id().as_str(), "lore");
    assert!(rt.config().db_path.is_none());
}

#[test]
fn backend_id_defaults_to_main() {
    let rt = KhiveRuntime::memory().unwrap();
    assert_eq!(rt.backend_id().as_str(), BackendId::MAIN);
}

#[cfg(unix)]
#[test]
fn storage_identity_accepts_hard_links_but_rejects_distinct_files() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let first_path = dir.path().join("first.db");
    let alias_path = dir.path().join("alias.db");
    let distinct_path = dir.path().join("distinct.db");
    let open = |path: &std::path::Path| {
        Arc::new(
            StorageBackend::sqlite_for_test_with_journal_mode(
                path,
                false,
                std::time::Duration::from_secs(1),
            )
            .expect("test backend"),
        )
    };
    let first = open(&first_path);
    std::fs::hard_link(&first_path, &alias_path).expect("hard-link alias");
    let alias = open(&alias_path);
    let distinct = open(&distinct_path);
    let first = KhiveRuntime::from_backend(first, RuntimeConfig::no_embeddings());
    let alias = KhiveRuntime::from_backend(alias, RuntimeConfig::no_embeddings());
    let distinct = KhiveRuntime::from_backend(distinct, RuntimeConfig::no_embeddings());

    assert!(first.shares_backend_storage_with(&alias));
    assert!(!first.shares_backend_storage_with(&distinct));
    assert!(first.shares_backend_storage_with(&first.clone()));
}

#[test]
fn store_accessors_return_ok() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();
    assert!(rt.entities(&tok).is_ok());
    assert!(rt.graph(&tok).is_ok());
    assert!(rt.notes(&tok).is_ok());
    assert!(rt.events(&tok).is_ok());
}

fn attributed_event_runtime() -> (KhiveRuntime, NamespaceToken) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("lambda:enrolled".to_string()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("configured actor is allowed by the default gate");
    (runtime, token)
}

fn forged_event(verb: &str) -> Event {
    Event::new(
        "caller-selected-namespace",
        verb,
        EventKind::Audit,
        SubstrateKind::Event,
        "caller-selected-actor",
    )
}

fn assert_token_attribution(event: &Event) {
    assert_eq!(event.namespace, "local");
    assert_eq!(event.actor, "actor:lambda:enrolled");
}

#[tokio::test]
async fn token_scoped_event_store_stamps_resolved_attribution_on_single_append() {
    let (runtime, token) = attributed_event_runtime();
    let store = runtime.events(&token).expect("event store");
    let event = forged_event("single");
    let id = event.id;

    store.append_event(event).await.expect("append");

    let stored = store
        .get_event(id)
        .await
        .expect("read")
        .expect("the token-stamped event remains visible to the token");
    assert_token_attribution(&stored);
    assert_eq!(stored.verb, "single", "non-attribution fields survive");
}

#[tokio::test]
async fn token_scoped_event_store_stamps_resolved_attribution_on_batch_paths() {
    let (runtime, token) = attributed_event_runtime();
    let store = runtime.events(&token).expect("event store");
    let ordinary = forged_event("batch");
    let ordinary_id = ordinary.id;

    store
        .append_events(vec![ordinary])
        .await
        .expect("ordinary batch append");
    let stored = store
        .get_event(ordinary_id)
        .await
        .expect("read")
        .expect("ordinary batch event remains visible to the token");
    assert_token_attribution(&stored);

    let idempotent = forged_event("idempotent_batch");
    let idempotent_id = idempotent.id;
    let outcome = store
        .append_events_idempotent(vec![idempotent])
        .await
        .expect("idempotent batch append");
    assert_eq!(
        outcome.rows,
        vec![khive_storage::event::EventAppendDisposition::Inserted]
    );
    let stored = store
        .get_event(idempotent_id)
        .await
        .expect("read")
        .expect("idempotent batch event remains visible to the token");
    assert_token_attribution(&stored);
}

#[test]
fn vectors_returns_unconfigured_without_model() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();
    match rt.vectors(&tok) {
        Err(crate::RuntimeError::Unconfigured(s)) => assert_eq!(s, "embedding_model"),
        Err(other) => panic!("expected Unconfigured, got {:?}", other),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

#[test]
fn vec_model_key_sanitizes_dots_and_dashes() {
    assert_eq!(
        vec_model_key(EmbeddingModel::BgeSmallEnV15),
        "bge_small_en_v1_5"
    );
    assert_eq!(
        vec_model_key(EmbeddingModel::BgeBaseEnV15),
        "bge_base_en_v1_5"
    );
    assert_eq!(
        vec_model_key(EmbeddingModel::AllMiniLmL6V2),
        "all_minilm_l6_v2"
    );
}

#[test]
fn default_config_uses_allow_all_gate() {
    let cfg = RuntimeConfig::default();
    assert_eq!(cfg.default_namespace.as_str(), "local");
    let _: GateRef = cfg.gate.clone();
}

#[test]
fn parse_pack_list_handles_comma_and_whitespace() {
    assert_eq!(parse_pack_list("kg"), vec!["kg".to_string()]);
    assert_eq!(
        parse_pack_list("kg,gtd"),
        vec!["kg".to_string(), "gtd".to_string()]
    );
    assert_eq!(
        parse_pack_list("  kg ,  gtd  "),
        vec!["kg".to_string(), "gtd".to_string()]
    );
    assert_eq!(
        parse_pack_list("kg gtd"),
        vec!["kg".to_string(), "gtd".to_string()]
    );
    assert_eq!(parse_pack_list(",,"), Vec::<String>::new());
    assert_eq!(parse_pack_list(""), Vec::<String>::new());
}

#[test]
fn default_config_packs_loads_production_set() {
    let prior = std::env::var("KHIVE_PACKS").ok();
    // SAFETY: test function runs single-threaded; no other threads read or write KHIVE_PACKS.
    unsafe {
        std::env::remove_var("KHIVE_PACKS");
    }
    // The default distribution loads all production packs.
    let cfg = RuntimeConfig::default();
    assert_eq!(cfg.packs, RuntimeConfig::built_in_packs());
    assert!(cfg.packs.contains(&"kg".to_string()));
    assert!(cfg.packs.contains(&"gtd".to_string()));
    assert!(cfg.packs.contains(&"memory".to_string()));
    assert!(cfg.packs.contains(&"brain".to_string()));
    assert!(cfg.packs.contains(&"comm".to_string()));
    assert!(cfg.packs.contains(&"schedule".to_string()));
    assert!(cfg.packs.contains(&"knowledge".to_string()));
    // session loads by default so its background mirror warm-hook runs in
    // production; its handlers are all operator-only subhandlers (0 wire verbs).
    assert!(cfg.packs.contains(&"session".to_string()));
    assert!(cfg.packs.contains(&"git".to_string()));
    assert!(cfg.packs.contains(&"code".to_string()));
    assert!(cfg.packs.contains(&"workspace".to_string()));
    // blob loads by default; a normal file-backed boot installs a
    // default FsBlobStore beside the database file with no config
    // needed, so its verbs are live in default deployments too (only an
    // in-memory backend leaves them unconfigured).
    assert!(cfg.packs.contains(&"blob".to_string()));
    // tool loads by default: the registry, discovery and use-policy verbs
    // (ADR-180) are live in default deployments.
    assert!(cfg.packs.contains(&"tool".to_string()));
    assert!(cfg.packs.contains(&"exec".to_string()));
    assert_eq!(cfg.packs.len(), 14);
    if let Some(v) = prior {
        // SAFETY: single-threaded test cleanup; restores KHIVE_PACKS to its prior value.
        unsafe {
            std::env::set_var("KHIVE_PACKS", v);
        }
    }
}

#[test]
fn default_config_uses_minilm_when_env_unset() {
    let prior = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
    // SAFETY: tests are serial by default for env mutation here; if other tests
    // mutate this var, mark them with the same scope.
    unsafe {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    }
    let cfg = RuntimeConfig::default();
    assert_eq!(cfg.embedding_model, Some(EmbeddingModel::AllMiniLmL6V2));
    if let Some(v) = prior {
        // SAFETY: single-threaded test cleanup; restores KHIVE_EMBEDDING_MODEL to its prior value.
        unsafe {
            std::env::set_var("KHIVE_EMBEDDING_MODEL", v);
        }
    }
}

// ---- Actor config tests ----

use crate::engine_config::{ActorConfig, KhiveConfig};

fn khive_cfg_with_actor(id: &str) -> KhiveConfig {
    KhiveConfig {
        engines: vec![],
        actor: ActorConfig {
            id: Some(id.to_string()),
            display_name: None,
            ..Default::default()
        },
        ..KhiveConfig::default()
    }
}

#[test]
fn runtime_config_from_khive_config_actor_id_does_not_override_default_namespace() {
    // `[actor] id` must not set `default_namespace`: writes stay pinned to
    // `local`. A non-`'local'` actor.id is folded into the default read
    // visible-set, but that does not change default_namespace. This test
    // asserts the write-routing invariant only.
    let base = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let cfg = khive_cfg_with_actor("lambda:khive");
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.default_namespace.as_str(),
        "local",
        "actor.id must not become default_namespace (ADR-007 Rev 4 Rule 0); writes pin to local"
    );
}

#[test]
fn runtime_config_from_khive_config_empty_actor_id_keeps_base_namespace() {
    let base = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::parse("lambda:base").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let cfg = KhiveConfig {
        engines: vec![],
        actor: ActorConfig {
            id: Some(String::new()),
            display_name: None,
            ..Default::default()
        },
        ..KhiveConfig::default()
    };
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.default_namespace.as_str(),
        "lambda:base",
        "empty actor.id must not override base namespace"
    );
}

#[test]
fn runtime_config_from_khive_config_absent_actor_id_keeps_base_namespace() {
    let base = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::parse("lambda:base").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let cfg = KhiveConfig::default(); // no actor.id
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.default_namespace.as_str(),
        "lambda:base",
        "absent actor.id must not override base namespace"
    );
}

#[test]
fn runtime_config_from_khive_config_actor_id_with_engines() {
    let base = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let cfg = KhiveConfig {
        engines: vec![crate::engine_config::EngineConfig {
            name: "all-minilm-l6-v2".to_string(),
            weight: 1.0,
            dims: None,
        }],
        actor: ActorConfig {
            id: Some("lambda:test".to_string()),
            display_name: None,
            ..Default::default()
        },
        ..KhiveConfig::default()
    };
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.default_namespace.as_str(),
        "local",
        "actor.id must not override default_namespace (ADR-007 Rev 4 Rule 0); \
             writes pin to local; engine config is still applied"
    );
    assert!(result.embedding_model.is_some());
}

// ---- [display] timezone (ADR-169) wiring tests ----

#[test]
fn runtime_config_from_khive_config_display_timezone_overrides_base() {
    let base = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let cfg = KhiveConfig {
        display: crate::engine_config::DisplaySectionConfig {
            timezone: Some("America/New_York".to_string()),
        },
        ..KhiveConfig::default()
    };
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.display_timezone,
        "America/New_York".parse::<chrono_tz::Tz>().unwrap(),
        "[display] timezone in khive.toml must override base.display_timezone"
    );
}

#[test]
fn runtime_config_from_khive_config_absent_display_timezone_keeps_base() {
    let base = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: "Asia/Tokyo".parse().unwrap(),
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let cfg = KhiveConfig::default(); // no [display] section
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.display_timezone,
        "Asia/Tokyo".parse::<chrono_tz::Tz>().unwrap(),
        "absent [display] timezone must preserve base.display_timezone unchanged"
    );
}

// ---- base.actor_id (env-resolved actor) preservation tests ----
//
// Regression coverage: a project config found without an `[actor] id` used
// to silently drop `base.actor_id` (e.g. the value `RuntimeConfig::default()`
// read from `KHIVE_ACTOR`) because both return arms spread an unconditional
// `actor_id: None` over `..base`. The fix falls back to `base.actor_id`
// when the TOML supplies no `[actor] id`, in both arms.

#[test]
#[serial]
fn runtime_config_from_khive_config_engines_present_preserves_env_actor_when_toml_has_none() {
    let prior = std::env::var("KHIVE_ACTOR").ok();
    // SAFETY: test is #[serial]; no other test in this crate reads/writes KHIVE_ACTOR.
    unsafe {
        std::env::set_var("KHIVE_ACTOR", "lambda:test-env-actor");
    }
    let base = RuntimeConfig::default();
    assert_eq!(base.actor_id.as_deref(), Some("lambda:test-env-actor"));

    let cfg = KhiveConfig {
        engines: vec![crate::engine_config::EngineConfig {
            name: "all-minilm-l6-v2".to_string(),
            weight: 1.0,
            dims: None,
        }],
        actor: ActorConfig::default(), // no [actor] id
        ..KhiveConfig::default()
    };
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.actor_id.as_deref(),
        Some("lambda:test-env-actor"),
        "engines-present arm must preserve base.actor_id (env actor) when TOML has no [actor] id"
    );

    // SAFETY: restores prior KHIVE_ACTOR value (test cleanup).
    unsafe {
        match prior {
            Some(v) => std::env::set_var("KHIVE_ACTOR", v),
            None => std::env::remove_var("KHIVE_ACTOR"),
        }
    }
}

#[test]
#[serial]
fn runtime_config_from_khive_config_engines_empty_preserves_env_actor_when_toml_has_none() {
    let prior = std::env::var("KHIVE_ACTOR").ok();
    // SAFETY: test is #[serial]; no other test in this crate reads/writes KHIVE_ACTOR.
    unsafe {
        std::env::set_var("KHIVE_ACTOR", "lambda:test-env-actor");
    }
    let base = RuntimeConfig::default();
    assert_eq!(base.actor_id.as_deref(), Some("lambda:test-env-actor"));

    let cfg = KhiveConfig {
        engines: vec![],
        actor: ActorConfig::default(), // no [actor] id
        ..KhiveConfig::default()
    };
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
            result.actor_id.as_deref(),
            Some("lambda:test-env-actor"),
            "engines-empty early-return arm must preserve base.actor_id (env actor) when TOML has no [actor] id"
        );

    // SAFETY: restores prior KHIVE_ACTOR value (test cleanup).
    unsafe {
        match prior {
            Some(v) => std::env::set_var("KHIVE_ACTOR", v),
            None => std::env::remove_var("KHIVE_ACTOR"),
        }
    }
}

#[test]
#[serial]
fn runtime_config_from_khive_config_toml_actor_wins_over_env_actor() {
    let prior = std::env::var("KHIVE_ACTOR").ok();
    // SAFETY: test is #[serial]; no other test in this crate reads/writes KHIVE_ACTOR.
    unsafe {
        std::env::set_var("KHIVE_ACTOR", "lambda:test-env-actor");
    }
    let base = RuntimeConfig::default();
    assert_eq!(base.actor_id.as_deref(), Some("lambda:test-env-actor"));

    let cfg = khive_cfg_with_actor("lambda:toml-actor");
    let result = runtime_config_from_khive_config(&cfg, base);
    assert_eq!(
        result.actor_id.as_deref(),
        Some("lambda:toml-actor"),
        "TOML [actor] id must win over the env-resolved base.actor_id"
    );

    // SAFETY: restores prior KHIVE_ACTOR value (test cleanup).
    unsafe {
        match prior {
            Some(v) => std::env::set_var("KHIVE_ACTOR", v),
            None => std::env::remove_var("KHIVE_ACTOR"),
        }
    }
}

// ---- list_embedding_models tests ----

// ---- core_backend accessor tests ----

/// Create a migrated in-memory backend (for tests that need raw Arc<StorageBackend>).
fn migrated_memory_backend() -> Arc<StorageBackend> {
    let backend = StorageBackend::memory().expect("memory backend");
    {
        let mut writer = backend.pool().try_writer().expect("writer");
        khive_db::run_migrations(writer.conn_mut()).expect("migrations");
    }
    Arc::new(backend)
}

fn secondary_config() -> RuntimeConfig {
    RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        exec: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::parse("lore").expect("valid backend id"),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        ..crate::RuntimeConfig::no_embeddings()
    }
}

#[test]
fn core_on_main_runtime_returns_same_backend_id() {
    // For a main-bound runtime, core() must return a clone with backend_id == "main".
    let rt = KhiveRuntime::memory().unwrap();
    assert_eq!(rt.backend_id().as_str(), BackendId::MAIN);
    let core_rt = rt.core();
    assert_eq!(core_rt.backend_id().as_str(), BackendId::MAIN);
}

#[tokio::test]
async fn core_on_main_runtime_round_trips_note() {
    // core() on a main-bound runtime (core_backend = None) returns self.clone(),
    // so a note written through core() is readable through the original runtime.
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();

    let note = rt
        .core()
        .create_note(
            &tok,
            "observation",
            None,
            "adr073-main-round-trip",
            None,
            None,
            vec![],
        )
        .await
        .expect("create_note via core()");

    let found = rt
        .notes(&tok)
        .expect("notes store")
        .get_note(note.id)
        .await
        .expect("get_note");

    assert!(
        found.is_some(),
        "note written via core() must be visible through original rt"
    );
}

/// Proves note→main and aux→secondary writes are each isolated.
///
/// Backend A = main; backend B = secondary.
/// rt_secondary is bound to B with core_backend = Some(A).
///
/// Direction 1 (note → main):
///   rt_secondary.core().create_note(...) must land in A (visible from rt_main)
///   and NOT in B (not visible from rt_secondary).
///
/// Direction 2 (aux → secondary):
///   A raw SQL write via rt_secondary.sql() lands in B only; A is untouched.
#[tokio::test]
async fn cross_backend_split_note_to_main_aux_to_secondary() {
    use khive_storage::{SqlStatement, SqlValue};

    let main_arc = migrated_memory_backend();
    let secondary_arc = migrated_memory_backend();

    let main_config = RuntimeConfig {
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..crate::RuntimeConfig::no_embeddings()
    };

    let rt_main = KhiveRuntime::from_backend(main_arc.clone(), main_config);
    let rt_secondary = KhiveRuntime::from_backend(secondary_arc, secondary_config())
        .with_core_backend(main_arc.clone());

    let tok = NamespaceToken::local();

    // ── Direction 1: note must land in A (main), not in B (secondary) ──

    let note = rt_secondary
        .core()
        .create_note(
            &tok,
            "observation",
            None,
            "adr073-split-test",
            None,
            None,
            vec![],
        )
        .await
        .expect("create_note via core()");
    let note_id = note.id;

    // Visible from main (A).
    let in_main = rt_main
        .notes(&tok)
        .expect("main notes store")
        .get_note(note_id)
        .await
        .expect("get_note from main");
    assert!(
        in_main.is_some(),
        "note written via core() must appear in main backend A"
    );

    // Not visible from secondary (B).
    let in_secondary = rt_secondary
        .notes(&tok)
        .expect("secondary notes store")
        .get_note(note_id)
        .await
        .expect("get_note from secondary");
    assert!(
        in_secondary.is_none(),
        "note written to main via core() must NOT appear in secondary backend B"
    );

    // ── Direction 2: aux write via rt_secondary.sql() lands in B, not A ──

    {
        let mut writer = rt_secondary.sql().writer().await.expect("secondary writer");
        writer
            .execute(SqlStatement {
                sql: "CREATE TABLE IF NOT EXISTS _test_adr073_aux \
                          (marker TEXT PRIMARY KEY)"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("create aux table in B");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO _test_adr073_aux VALUES (?1)".into(),
                params: vec![SqlValue::Text("b-side-sentinel".into())],
                label: None,
            })
            .await
            .expect("insert into aux table in B");
    }

    // Row is present in B.
    let mut reader_b = rt_secondary.sql().reader().await.expect("secondary reader");
    let rows_b = reader_b
        .query_all(SqlStatement {
            sql: "SELECT marker FROM _test_adr073_aux".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("select from B");
    assert_eq!(rows_b.len(), 1, "aux row must exist in B");
    match rows_b[0].get("marker") {
        Some(SqlValue::Text(s)) => {
            assert_eq!(s, "b-side-sentinel", "sentinel value must match")
        }
        other => panic!("expected Text('b-side-sentinel'), got {other:?}"),
    }

    // Row is absent from A (table does not exist there).
    let mut reader_a = rt_main.sql().reader().await.expect("main reader");
    let result_a = reader_a
        .query_all(SqlStatement {
            sql: "SELECT marker FROM _test_adr073_aux".into(),
            params: vec![],
            label: None,
        })
        .await;
    // A does not have this table → must error or return no rows.
    match result_a {
        Err(e) => assert!(
            e.to_string().contains("no such table"),
            "expected 'no such table' error from A, got: {e}"
        ),
        Ok(rows) => assert!(
            rows.is_empty(),
            "aux table must not have rows in A, got {} rows",
            rows.len()
        ),
    }
}

#[test]
fn constructors_leave_core_backend_none_by_behavior() {
    // core() on any standard constructor returns a clone with same backend_id —
    // proof that core_backend = None (returns self.clone(), not a different backend).
    let rt_mem = KhiveRuntime::memory().unwrap();
    assert_eq!(rt_mem.core().backend_id().as_str(), BackendId::MAIN);

    let backend = migrated_memory_backend();
    let rt_from = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: khive_db::WalCeilingSource::Default,
            wal_ceiling_env_raw: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::parse("lore").expect("valid backend id"),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..crate::RuntimeConfig::no_embeddings()
        },
    );
    // from_backend with backend_id="lore" and no core_backend: core() returns
    // self.clone() which has backend_id="lore" (not "main").
    assert_eq!(rt_from.core().backend_id().as_str(), "lore");
}

#[test]
fn with_core_backend_sets_core_then_core_returns_main_id() {
    // After wiring, core() must return a runtime with backend_id == "main".
    let main_arc = migrated_memory_backend();
    let secondary_arc = migrated_memory_backend();

    let rt_secondary =
        KhiveRuntime::from_backend(secondary_arc, secondary_config()).with_core_backend(main_arc);

    assert_eq!(rt_secondary.backend_id().as_str(), "lore");
    assert_eq!(
        rt_secondary.core().backend_id().as_str(),
        BackendId::MAIN,
        "core() on a secondary runtime must return a main-bound handle"
    );
}

#[test]
fn attachment_store_rejects_secondary_handle_and_accepts_its_core_projection() {
    let main_arc = migrated_memory_backend();
    let secondary_arc = migrated_memory_backend();
    let rt_secondary =
        KhiveRuntime::from_backend(secondary_arc, secondary_config()).with_core_backend(main_arc);

    let error = match rt_secondary.attachments() {
        Ok(_) => panic!("a secondary runtime must not expose attachment mutation"),
        Err(error) => error,
    };
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    assert!(
        error.to_string().contains("canonical main backend"),
        "secondary refusal must explain the liveness authority: {error}"
    );
    rt_secondary
        .core()
        .attachments()
        .expect("core projection must expose the main attachment store");
}

#[tokio::test]
async fn record_plus_attachment_publication_rejects_a_secondary_runtime() {
    use khive_storage::{BlobStore as _, NewAttachment};

    let main_arc = migrated_memory_backend();
    let secondary_arc = migrated_memory_backend();
    let rt_secondary = KhiveRuntime::from_backend(Arc::clone(&secondary_arc), secondary_config())
        .with_core_backend(Arc::clone(&main_arc));
    let blob_root = tempfile::tempdir().expect("blob root");
    let blob_store = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0)
            .expect("blob store"),
    );
    let content_ref = blob_store.put(b"secondary-ref".to_vec()).await.unwrap();
    rt_secondary
        .install_blob_store(blob_store.clone())
        .expect("shared blob store");
    let token = rt_secondary.authorize(Namespace::local()).unwrap();

    let error = rt_secondary
        .create_entity_with_attachments(
            &token,
            "artifact",
            Some("visual_asset"),
            "must route through core",
            None,
            None,
            vec![],
            vec![NewAttachment {
                role: "content".to_string(),
                content_ref: content_ref.clone(),
                media_type: None,
                size_bytes: Some(13),
            }],
        )
        .await
        .expect_err("secondary attachment publication must fail closed");
    assert!(error.to_string().contains("canonical main backend"));
    assert!(rt_secondary
        .list_entities(&token, None, None, 10, 0)
        .await
        .unwrap()
        .is_empty());
    assert!(rt_secondary
        .core()
        .list_entities(&token, None, None, 10, 0)
        .await
        .unwrap()
        .is_empty());
    assert!(
        blob_store.exists(&content_ref).await.unwrap(),
        "refusal must not mutate the already-published object"
    );
}

#[tokio::test]
async fn list_embedding_models_returns_empty_when_table_absent() {
    // A brand-new in-memory runtime has migrations applied, so _embedding_models
    // IS created. But with no rows inserted, the result must be empty.
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let records = rt
        .list_embedding_models(None)
        .await
        .expect("list ok on empty table");
    assert!(records.is_empty());
}

#[tokio::test]
async fn list_embedding_models_returns_row_after_insert() {
    use khive_storage::{SqlStatement, SqlValue};

    let rt = KhiveRuntime::memory().expect("memory runtime");
    let sql = rt.sql();

    let now = 1_000_000i64;
    let id = uuid::Uuid::new_v4();
    let canonical_key = b"test_engine:test-model-v1:v1:384".to_vec();

    let mut writer = sql.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO _embedding_models \
                      (id, engine_name, model_id, key_version, dim, output_dim, status, \
                       activated_at, superseded_at, superseded_by, canonical_key, created_at) \
                      VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, NULL, NULL, ?8, ?9)"
                .into(),
            params: vec![
                SqlValue::Blob(id.as_bytes().to_vec()),
                SqlValue::Text("test_engine".into()),
                SqlValue::Text("test-model-v1".into()),
                SqlValue::Text("v1".into()),
                SqlValue::Integer(384),
                SqlValue::Text("active".into()),
                SqlValue::Integer(now),
                SqlValue::Blob(canonical_key),
                SqlValue::Integer(now),
            ],
            label: None,
        })
        .await
        .expect("insert row");
    drop(writer);

    let records = rt.list_embedding_models(None).await.expect("list ok");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].engine_name, "test_engine");
    assert_eq!(records[0].model_id, "test-model-v1");
    assert_eq!(records[0].key_version, "v1");
    assert_eq!(records[0].dimensions, 384);
    assert_eq!(records[0].status, "active");

    // engine filter — match
    let filtered = rt
        .list_embedding_models(Some("test_engine"))
        .await
        .expect("filter ok");
    assert_eq!(filtered.len(), 1);

    // engine filter — no match
    let no_match = rt
        .list_embedding_models(Some("other_engine"))
        .await
        .expect("no-match ok");
    assert!(no_match.is_empty());
}

#[test]
fn named_vector_identity_rejects_ambiguous_or_unsafe_values() {
    assert!(NamedVectorIdentity::new("", "model", 4).is_err());
    assert!(NamedVectorIdentity::new("bad-key", "model", 4).is_err());
    assert!(NamedVectorIdentity::new("valid_key", " model", 4).is_err());
    assert!(NamedVectorIdentity::new("valid_key", "model", 0).is_err());
    assert!(NamedVectorIdentity::new("valid_key", "model", 8193).is_err());
    assert!(NamedVectorIdentity::new("k".repeat(128), "m".repeat(512), 4).is_ok());
    assert!(NamedVectorIdentity::new("k".repeat(129), "model", 4).is_err());
    assert!(NamedVectorIdentity::new("valid_key", "m".repeat(513), 4).is_err());
    assert_eq!(
        NamedVectorIdentity::new("valid_key", "model", 4)
            .expect("valid identity")
            .dimensions(),
        4
    );
}

#[tokio::test]
async fn named_vector_store_rejects_dimension_or_model_key_rebinding() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let original = NamedVectorIdentity::new("visual_contract", "model-a", 4).unwrap();
    rt.vectors_for_named_identity(&token, &original)
        .await
        .expect("create named vector store");
    let registered = rt
        .list_embedding_models(Some("visual_contract"))
        .await
        .expect("list model registry");
    assert!(registered.iter().any(|record| {
        record.model_id == "model-a"
            && record.key_version == "visual_contract"
            && record.dimensions == 4
    }));
    let wrong_dimensions = NamedVectorIdentity::new("visual_contract", "model-a", 5).unwrap();
    let Err(dimension_error) = rt
        .vectors_for_named_identity(&token, &wrong_dimensions)
        .await
    else {
        panic!("same key cannot change dimensions");
    };
    assert!(dimension_error.to_string().contains("dimensions"));

    let wrong_model = NamedVectorIdentity::new("visual_contract", "model-b", 4).unwrap();
    let Err(model_error) = rt.vectors_for_named_identity(&token, &wrong_model).await else {
        panic!("same key cannot change model identity");
    };
    assert!(model_error.to_string().contains("already bound"));
}

#[tokio::test]
async fn repeated_named_vector_lookup_avoids_writer_acquisition() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    let identity = NamedVectorIdentity::new("visual_cached", "model-a", 4).unwrap();
    runtime
        .vectors_for_named_identity(&token, &identity)
        .await
        .expect("first lookup validates and registers");
    let writer_before = runtime.backend().pool().writer_acquisition_snapshot();

    runtime
        .clone()
        .vectors_for_named_identity(&token, &identity)
        .await
        .expect("clone reuses verified store");
    assert_eq!(
        runtime.backend().pool().writer_acquisition_snapshot(),
        writer_before,
        "repeated reads must not reach vector-table setup or model registration"
    );
}

#[tokio::test]
async fn core_projection_reuses_main_named_vector_cache() {
    let main_backend = migrated_memory_backend();
    let main =
        KhiveRuntime::from_backend(Arc::clone(&main_backend), RuntimeConfig::no_embeddings());
    let secondary = KhiveRuntime::from_backend(migrated_memory_backend(), secondary_config())
        .with_core_embedders_from(&main)
        .with_core_backend(Arc::clone(&main_backend));
    let core = secondary.core();
    let token = core.authorize(Namespace::local()).expect("authorize");
    let identity = NamedVectorIdentity::new("core_visual_cached", "model-a", 4).unwrap();
    core.vectors_for_named_identity(&token, &identity)
        .await
        .expect("first lookup validates on main");
    let writer_before = main_backend.pool().writer_acquisition_snapshot();

    secondary
        .core()
        .vectors_for_named_identity(&token, &identity)
        .await
        .expect("new core projection reuses main store");
    assert_eq!(
        main_backend.pool().writer_acquisition_snapshot(),
        writer_before
    );
}

#[tokio::test]
async fn rebound_core_named_vector_lookup_uses_new_backend() {
    let first_backend = migrated_memory_backend();
    let second_backend = migrated_memory_backend();
    let secondary = KhiveRuntime::from_backend(migrated_memory_backend(), secondary_config())
        .with_core_backend(Arc::clone(&first_backend));
    let identity = NamedVectorIdentity::new("rebound_visual", "model-a", 4).unwrap();
    let first_core = secondary.core();
    let first_token = first_core.authorize(Namespace::local()).expect("authorize");
    let first_store = first_core
        .vectors_for_named_identity(&first_token, &identity)
        .await
        .expect("first backend store");

    let rebound = secondary.with_core_backend(Arc::clone(&second_backend));
    let second_core = rebound.core();
    let second_token = second_core
        .authorize(Namespace::local())
        .expect("authorize");
    let second_store = second_core
        .vectors_for_named_identity(&second_token, &identity)
        .await
        .expect("second backend store");
    assert!(
        !Arc::ptr_eq(&first_store, &second_store),
        "the second backend needs its own vector store"
    );
    let registered = second_core
        .list_embedding_models(Some("rebound_visual"))
        .await
        .expect("second backend model registry");
    assert!(registered.iter().any(|record| {
        record.model_id == "model-a"
            && record.key_version == "rebound_visual"
            && record.dimensions == 4
    }));
}

#[test]
fn rebound_core_discards_previous_main_embedder_wiring() {
    let first_backend = migrated_memory_backend();
    let second_backend = migrated_memory_backend();
    let first_main =
        KhiveRuntime::from_backend(Arc::clone(&first_backend), RuntimeConfig::no_embeddings());
    let second_main =
        KhiveRuntime::from_backend(Arc::clone(&second_backend), RuntimeConfig::no_embeddings());
    let secondary = KhiveRuntime::from_backend(migrated_memory_backend(), secondary_config())
        .with_core_embedders_from(&first_main)
        .with_core_backend(Arc::clone(&first_backend));
    assert!(secondary.core_embedders.is_some());

    let rebound = secondary.with_core_backend(Arc::clone(&second_backend));
    assert!(
        rebound.core_embedders.is_none(),
        "the second backend cannot use the first main runtime's embedders"
    );
    assert!(Arc::ptr_eq(
        &rebound.core().embedder_registry,
        &rebound.embedder_registry
    ));

    let rewired = rebound.with_core_embedders_from(&second_main);
    assert!(Arc::ptr_eq(
        &rewired.core().embedder_registry,
        &second_main.embedder_registry
    ));
}

#[tokio::test]
async fn concurrent_named_vector_first_bind_has_one_immutable_winner() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let first = NamedVectorIdentity::new("visual_race", "model-a", 4).unwrap();
    let second = NamedVectorIdentity::new("visual_race", "model-b", 4).unwrap();

    let (first_result, second_result) = tokio::join!(
        rt.vectors_for_named_identity(&token, &first),
        rt.vectors_for_named_identity(&token, &second),
    );
    assert_ne!(
        first_result.is_ok(),
        second_result.is_ok(),
        "the active engine_name uniqueness rule must select exactly one first binding"
    );

    let (winner, loser) = if first_result.is_ok() {
        (&first, &second)
    } else {
        (&second, &first)
    };
    rt.vectors_for_named_identity(&token, winner)
        .await
        .expect("winning identity remains idempotent");
    let error = match rt.vectors_for_named_identity(&token, loser).await {
        Ok(_) => panic!("losing identity cannot rebind the empty table"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("already bound"));

    let registered = rt
        .list_embedding_models(Some("visual_race"))
        .await
        .expect("list race registry");
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].model_id, winner.model_name());
}

#[tokio::test]
async fn named_vector_registry_keeps_immutable_revisions_active_together() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let first = NamedVectorIdentity::new("visual_revision_a", "visual-model", 4).unwrap();
    let second = NamedVectorIdentity::new("visual_revision_b", "visual-model", 4).unwrap();

    rt.vectors_for_named_identity(&token, &first)
        .await
        .expect("open first immutable space");
    rt.vectors_for_named_identity(&token, &second)
        .await
        .expect("open second immutable space");

    let registered = rt.list_embedding_models(None).await.expect("list registry");
    assert!(registered.iter().any(|record| {
        record.engine_name == "visual_revision_a"
            && record.model_id == "visual-model"
            && record.key_version == "visual_revision_a"
            && record.status == "active"
    }));
    assert!(registered.iter().any(|record| {
        record.engine_name == "visual_revision_b"
            && record.model_id == "visual-model"
            && record.key_version == "visual_revision_b"
            && record.status == "active"
    }));
}
