use khive_mounts::MountedPack;
use khive_runtime::{
    mount_config::{MountConfig, MountEffect, MountToolConfig},
    KhiveRuntime, PackRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};
use std::{fs, path::Path, sync::Arc};

fn fixture() -> String {
    let current = std::env::current_exe().unwrap();
    fs::read_dir(current.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("fixture-")
                && (path.extension().is_none() || path.extension().is_some_and(|ext| ext == "exe"))
        })
        .max_by_key(|path| path.metadata().unwrap().modified().unwrap())
        .unwrap()
        .to_string_lossy()
        .into_owned()
}
fn write_catalog(path: &Path, names: &[&str], changed: bool) {
    let tools: Vec<_> = names.iter().map(|name| json!({"name": name, "description": "fixture tool", "inputSchema": {"type": "object", "description": if changed { "changed" } else { "original" }}})).collect();
    let tmp = path.with_extension("new");
    fs::write(&tmp, json!({"tools": tools}).to_string()).unwrap();
    fs::rename(tmp, path).unwrap();
}
fn config(path: &Path, names: &[&str]) -> MountConfig {
    MountConfig {
        name: "demo".into(),
        transport: "stdio".into(),
        command: fixture(),
        args: vec![path.to_string_lossy().into_owned()],
        env: vec![],
        credential: None,
        tools: names
            .iter()
            .map(|name| MountToolConfig {
                name: (*name).into(),
                effect: MountEffect::Mutating,
            })
            .collect(),
        timeout_ms: 3000,
    }
}
fn runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .unwrap()
}
async fn registry(runtime: &KhiveRuntime, config: MountConfig, denied: bool) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_runtime_event_store(runtime).unwrap();
    if denied {
        builder.with_gate(Arc::new(khive_gate::CallerEnrollmentGate::new(
            vec![],
            false,
        )));
    }
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder
        .register_mounted(Box::new(
            MountedPack::start(config, runtime.clone()).await.unwrap(),
        ))
        .unwrap();
    builder.build().unwrap()
}
fn count(path: &Path, suffix: &str) -> usize {
    fs::read_to_string(format!("{}{suffix}", path.display()))
        .unwrap_or_default()
        .lines()
        .count()
}

#[tokio::test]
async fn pin_drift_unknown_repin_and_public_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    write_catalog(&path, &["A", "B"], false);
    let runtime = runtime();
    let cfg = config(&path, &["A", "B"]);
    let registry = registry(&runtime, cfg, false).await;
    for name in ["A", "B"] {
        assert_eq!(
            registry
                .dispatch(&format!("demo.{name}"), json!({}))
                .await
                .unwrap()["content"][0]["text"],
            "ok"
        );
    }
    let listed = registry
        .dispatch("verbs", json!({"pack": "demo"}))
        .await
        .unwrap();
    assert_eq!(listed["total"], 2);
    assert_eq!(listed["verbs"][0]["visibility"], "Verb");
    write_catalog(&path, &["A", "B", "C"], true);
    let before = count(&path, ".calls");
    assert!(matches!(
        registry.dispatch("demo.C", json!({})).await,
        Err(khive_runtime::RuntimeError::UnknownVerb(_))
    ));
    assert!(registry
        .dispatch("demo.A", json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("tool_error"));
    assert_eq!(count(&path, ".calls"), before);
    let operator = MountedPack::start(config(&path, &["A", "B", "C"]), runtime.clone())
        .await
        .unwrap();
    let result = operator.repin("operator").await.unwrap();
    assert_eq!(result["generation"], 2);
    assert_eq!(result["added"], json!(["C"]));
    for name in ["A", "C"] {
        registry
            .dispatch(&format!("demo.{name}"), json!({}))
            .await
            .unwrap();
    }
    assert_eq!(
        registry
            .dispatch("verbs", json!({"pack": "demo"}))
            .await
            .unwrap()["total"],
        3
    );
}

#[tokio::test]
async fn foreign_errors_timeouts_and_malformed_are_classified_and_redacted() {
    for (mode, class) in [
        ("error", "tool_error"),
        ("is_error", "tool_error"),
        ("timeout", "tool_timeout"),
        ("malformed", "tool_malformed"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        write_catalog(&path, &["A"], false);
        let runtime = runtime();
        let registry = registry(&runtime, config(&path, &["A"]), false).await;
        let error = registry
            .dispatch("demo.A", json!({"mode": mode}))
            .await
            .expect_err(mode);
        let khive_runtime::RuntimeError::Khive(error) = error else {
            panic!("ordinary KhiveError required")
        };
        let wire = serde_json::to_value(error).unwrap();
        assert_eq!(wire["details"]["class"], class, "{mode}: {wire}");
        assert!(!wire.to_string().contains("RAW_SECRET_SENTINEL"));
    }
}

#[tokio::test]
async fn gate_denial_has_no_foreign_call_or_additional_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    write_catalog(&path, &["A"], false);
    let runtime = runtime();
    let registry = registry(&runtime, config(&path, &["A"]), true).await;
    let starts = count(&path, ".starts");
    let mounted = registry.dispatch("demo.A", json!({})).await.unwrap_err();
    let native = registry.dispatch("stats", json!({})).await.unwrap_err();
    let khive_runtime::RuntimeError::PermissionDenied { verb, reason } = mounted else {
        panic!("mounted gate refusal")
    };
    let khive_runtime::RuntimeError::PermissionDenied {
        reason: native_reason,
        ..
    } = native
    else {
        panic!("native gate refusal")
    };
    assert_eq!(verb, "demo.A");
    assert_eq!(reason, native_reason);
    let events = audit(&runtime).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.verb == "demo.A"
                && event.kind == khive_types::EventKind::Audit
                && event.payload["decision"] == "deny")
            .count(),
        1
    );
    assert_eq!(count(&path, ".starts"), starts);
    assert_eq!(count(&path, ".calls"), 0);
}

#[tokio::test]
async fn missing_configured_tool_is_boot_refusal_and_no_silent_repin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    write_catalog(&path, &["A"], false);
    let runtime = runtime();
    assert!(MountedPack::start(config(&path, &["B"]), runtime.clone())
        .await
        .is_err());
    let original = MountedPack::start(config(&path, &["A"]), runtime.clone())
        .await
        .unwrap();
    let digest = original.mounted_catalog().await.unwrap()[0].digest.clone();
    write_catalog(&path, &["A"], true);
    let restarted = MountedPack::start(config(&path, &["A"]), runtime)
        .await
        .unwrap();
    assert_eq!(restarted.mounted_catalog().await.unwrap()[0].digest, digest);
}

async fn audit(runtime: &KhiveRuntime) -> Vec<khive_storage::Event> {
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    runtime
        .events(&token)
        .unwrap()
        .query_events(
            khive_storage::EventFilter::default(),
            khive_storage::PageRequest {
                limit: 200,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items
}

#[tokio::test]
async fn success_audit_records_default_mutating_and_explicit_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    write_catalog(&path, &["A", "B"], false);
    let runtime = runtime();
    let mut cfg = config(&path, &["A", "B"]);
    cfg.tools[1].effect = MountEffect::Read;
    let registry = registry(&runtime, cfg, false).await;
    registry.dispatch("demo.A", json!({})).await.unwrap();
    registry.dispatch("demo.B", json!({})).await.unwrap();
    let events = audit(&runtime).await;
    for (verb, effect) in [("demo.A", "mutating"), ("demo.B", "read")] {
        let rows: Vec<_> = events
            .iter()
            .filter(|event| event.verb == verb && event.kind == khive_types::EventKind::Audit)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].payload["mounted_tool"]["effect"], effect);
        assert_eq!(rows[0].payload["mounted_tool"]["generation"], 1);
    }
}

#[tokio::test]
async fn old_generation_is_denied_and_repin_is_one_audited_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    write_catalog(&path, &["A"], false);
    let runtime = runtime();
    let pack = MountedPack::start(config(&path, &["A"]), runtime.clone())
        .await
        .unwrap();
    let old = pack.mounted_catalog().await.unwrap().remove(0);
    pack.repin("operator").await.unwrap();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let empty = VerbRegistryBuilder::new().build().unwrap();
    let error = pack
        .dispatch_mounted(&old, "demo.A", json!({}), &empty, &token)
        .await
        .unwrap_err();
    let khive_runtime::RuntimeError::Khive(error) = error else {
        panic!("typed drift")
    };
    assert_eq!(
        serde_json::to_value(error).unwrap()["details"]["reason"],
        "catalog_drift"
    );
    assert_eq!(count(&path, ".calls"), 0);
    let events = audit(&runtime).await;
    let rows: Vec<_> = events
        .iter()
        .filter(|event| event.verb == "mount.repin")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor, "operator");
    assert_eq!(rows[0].payload["generation"], 2);
}

#[tokio::test]
async fn second_process_death_stays_down_and_missing_mount_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    write_catalog(&path, &["A"], false);
    let runtime = runtime();
    let registry = registry(&runtime, config(&path, &["A"]), false).await;
    fs::write(&path, json!({"exit": true}).to_string()).unwrap();
    registry.dispatch("demo.A", json!({})).await.unwrap_err();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while count(&path, ".starts") < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut final_error = Value::Null;
    for _ in 0..20 {
        if let Err(khive_runtime::RuntimeError::Khive(error)) =
            registry.dispatch("demo.A", json!({})).await
        {
            final_error = serde_json::to_value(error).unwrap();
        }
        if final_error["details"]["reason"] == "mount_down" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(final_error["details"]["reason"], "mount_down");
    assert_eq!(count(&path, ".starts"), 2);
    let empty = VerbRegistryBuilder::new().build().unwrap();
    assert!(matches!(
        empty.dispatch("demo.A", json!({})).await,
        Err(khive_runtime::RuntimeError::UnknownVerb(_))
    ));
}

#[tokio::test]
async fn repin_audit_failure_rolls_back_catalog_update() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    write_catalog(&path, &["A"], false);
    let runtime = runtime();
    let pack = MountedPack::start(config(&path, &["A"]), runtime.clone())
        .await
        .unwrap();
    let original = pack.mounted_catalog().await.unwrap().remove(0);
    runtime.sql().writer().await.unwrap().execute_script("CREATE TRIGGER refuse_repin_audit BEFORE INSERT ON events WHEN NEW.verb = 'mount.repin' BEGIN SELECT RAISE(ABORT, 'injected audit failure'); END;".into()).await.unwrap();
    write_catalog(&path, &["A"], true);
    assert!(pack.repin("operator").await.is_err());
    let current = pack.mounted_catalog().await.unwrap().remove(0);
    assert_eq!(current.generation, original.generation);
    assert_eq!(current.digest, original.digest);
    assert!(audit(&runtime)
        .await
        .iter()
        .all(|event| event.verb != "mount.repin"));
}

#[derive(Debug, Default)]
struct SpyGate(std::sync::Mutex<Vec<khive_gate::GateRequest>>);
impl khive_gate::Gate for SpyGate {
    fn check(
        &self,
        request: &khive_gate::GateRequest,
    ) -> Result<khive_gate::GateDecision, khive_gate::GateError> {
        self.0.lock().unwrap().push(request.clone());
        Ok(khive_gate::GateDecision::allow())
    }
}

#[tokio::test]
async fn mounted_call_mints_one_gate_request_and_preserves_declared_namespace_argument() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    fs::write(&path, json!({"tools": [{"name": "A", "inputSchema": {"type": "object", "properties": {"namespace": {"type": "string"}}, "required": ["namespace"]}}]}).to_string()).unwrap();
    let runtime = runtime();
    let spy = Arc::new(SpyGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(spy.clone());
    builder
        .register_mounted(Box::new(
            MountedPack::start(config(&path, &["A"]), runtime)
                .await
                .unwrap(),
        ))
        .unwrap();
    let result = builder
        .build()
        .unwrap()
        .dispatch("demo.A", json!({"namespace": "local"}))
        .await
        .unwrap();
    assert_eq!(result["structuredContent"]["namespace"], "local");
    let requests = spy.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].verb.as_str(), "demo.A");
    assert_eq!(requests[0].namespace.as_str(), "local");
    assert_eq!(requests[0].args["namespace"], "local");
}
