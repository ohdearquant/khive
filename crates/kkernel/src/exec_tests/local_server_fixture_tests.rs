// ── isolated DB helpers ────────────────────────────────────────────────────

/// Build an isolated in-process runtime using a temp-file SQLite database.
/// Never touches the production `~/.khive/khive.db`.
fn isolated_server(db_path: &str) -> KhiveMcpServer {
    let cfg = RuntimeConfig {
        db_path: Some(PathBuf::from(db_path)),
        embedding_model: None,
        additional_embedding_models: vec![],
        // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
        // from the ambient environment (#1276) — callers of this helper
        // dispatch `kg` and `gtd.assign` verbs, so pin both rather than
        // letting a wider ambient pack set a developer's shell exports.
        packs: vec!["kg".to_string(), "gtd".to_string()],
        ..Default::default()
    };
    let rt = KhiveRuntime::new(cfg).expect("runtime on temp db");
    KhiveMcpServer::new(rt).expect("server on temp db")
}

struct OpsFileConcurrencyProbePack {
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    max_in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    reject_overlap: bool,
}

impl khive_types::Pack for OpsFileConcurrencyProbePack {
    const NAME: &'static str = "ops-file-concurrency-probe";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[khive_runtime::HandlerDef {
        name: "reader_probe",
        description: "records test-only handler concurrency",
        visibility: khive_runtime::Visibility::Verb,
        category: khive_runtime::VerbCategory::Assertive,
        params: &[],
    }];
}

struct ProbeInFlightGuard {
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for ProbeInFlightGuard {
    fn drop(&mut self) {
        self.in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for OpsFileConcurrencyProbePack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        params: serde_json::Value,
        _registry: &khive_runtime::VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> std::result::Result<serde_json::Value, khive_runtime::RuntimeError> {
        let current = self
            .in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let _guard = ProbeInFlightGuard {
            in_flight: self.in_flight.clone(),
        };
        self.max_in_flight
            .fetch_max(current, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        if params["fail"].as_bool().unwrap_or(false) {
            return Err(khive_runtime::RuntimeError::Internal(
                "requested probe failure".to_string(),
            ));
        }
        if self.reject_overlap && current > 1 {
            return Err(khive_runtime::RuntimeError::Internal(
                "sql_bridge.reader_open constrained-reader overlap".to_string(),
            ));
        }
        Ok(serde_json::json!({"sequence": params["sequence"]}))
    }
}

fn concurrency_probe_server(
    reject_overlap: bool,
) -> (
    KhiveMcpServer,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(OpsFileConcurrencyProbePack {
        in_flight: in_flight.clone(),
        max_in_flight: max_in_flight.clone(),
        reject_overlap,
    });
    (
        KhiveMcpServer::from_registry(builder.build().expect("probe registry")),
        in_flight,
        max_in_flight,
    )
}

fn reader_probe_ops(count: usize) -> Vec<OpsFileEntry> {
    (0..count)
        .map(|sequence| OpsFileEntry {
            tool: "reader_probe".to_string(),
            args: serde_json::json!({"sequence": sequence}),
        })
        .collect()
}

// ── isolated_server ignores ambient KHIVE_PACKS (#1276) ───────────────────
//
// `cargo test -p kkernel` failed ~20 exec tests whenever a developer's
// shell exported `KHIVE_PACKS` naming a pack not compiled into this
// workspace (e.g. `kg,gtd`): every `RuntimeConfig` built by this test
// module's shared helpers fell through to `RuntimeConfig::default()`'s
// `packs` field, which reads that env var, so construction panicked with
// `PackRegError { unknown: "gtd", .. }`. A unit test's outcome must not
// depend on ambient shell configuration.
#[test]
fn isolated_server_ignores_ambient_khive_packs_naming_unavailable_pack() {
    if crate::test_process::run_in_child() {
        return;
    }
    std::env::set_var("KHIVE_PACKS", "kg,gtd");

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    // Before the fix, this panicked inside `KhiveMcpServer::new` — the
    // helper inherited the ambient list above instead of pinning its own.
    let _server = isolated_server(&db_path);
}

fn rerun_in_command_scoped_empty_home(child_marker: &str, test_name: &str) -> bool {
    if std::env::var_os(child_marker).is_some() {
        return false;
    }

    let calling_process_home = std::env::var_os("HOME");
    let empty_home = tempfile::tempdir().expect("isolated child HOME");
    let status =
        std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .arg(test_name)
            .arg("--exact")
            .env("HOME", empty_home.path())
            .env_remove("KHIVE_EMBEDDING_MODEL")
            .env_remove("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
            .env_remove("KHIVE_ACTOR")
            .env(child_marker, "1")
            .status()
            .expect("spawn isolated config-discovery test process");
    assert_eq!(
        std::env::var_os("HOME"),
        calling_process_home,
        "command-scoped HOME must not mutate the calling test process"
    );
    assert!(status.success(), "isolated child test failed: {status}");
    true
}
