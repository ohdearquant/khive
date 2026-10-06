use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::credentials::{
    CredentialCacheLifetime, CredentialConfig, CredentialError, CredentialKind, CredentialMaterial,
    CredentialProvider, VisibilityReceiptKeyConfig,
};
use crate::engine_config::{EngineConfig, KhiveConfig};
use crate::{BackendId, DomainDisposition};

struct RevocableProvider(Arc<AtomicBool>);

impl CredentialProvider for RevocableProvider {
    fn resolve(&self, name: &str) -> Result<CredentialMaterial, CredentialError> {
        if self.0.load(Ordering::SeqCst) {
            Ok(CredentialMaterial::new(vec![b'A'; 43]))
        } else {
            Err(CredentialError::Unavailable { name: name.into() })
        }
    }

    fn cache_lifetime(&self) -> CredentialCacheLifetime {
        CredentialCacheLifetime::NoCache
    }
}

fn declarations() -> Vec<CredentialConfig> {
    vec![CredentialConfig {
        name: "receipt-test".into(),
        kind: CredentialKind::SigningKey,
        provider: "receipt-test".into(),
        env_var: None,
        header: None,
    }]
}

fn ring() -> VisibilityReceiptConfig {
    VisibilityReceiptConfig {
        keys: vec![VisibilityReceiptKeyConfig {
            id: "receipt-test-id".into(),
            credential: "receipt-test".into(),
            encrypt: true,
        }],
    }
}

fn configure(runtime: KhiveRuntime) -> (KhiveRuntime, Arc<AtomicBool>) {
    let available = Arc::new(AtomicBool::new(true));
    let mut registry = CredentialRegistry::new(declarations()).unwrap();
    registry
        .register_provider(
            "receipt-test".into(),
            Arc::new(RevocableProvider(available.clone())),
        )
        .unwrap();
    (
        runtime
            .with_visibility_receipt_credentials(ring(), Arc::new(registry))
            .unwrap(),
        available,
    )
}

fn fields(namespace: &str, issued_at: i64, fences: Vec<(String, u64)>) -> ReceiptFields {
    ReceiptFields {
        namespace: namespace.into(),
        issued_at,
        fences,
    }
}

fn projected(error: RuntimeError) -> serde_json::Value {
    crate::error_projection::runtime_error_value(error, DomainDisposition::Unknown)
}

#[test]
fn receipt_configuration_survives_both_engine_conversion_branches() {
    for engines in [
        Vec::new(),
        vec![EngineConfig {
            name: "primary".into(),
            model: "all-minilm-l6-v2".into(),
            default: true,
            fusion_weight: None,
            dims: Some(384),
        }],
    ] {
        let config = KhiveConfig {
            credentials: declarations(),
            visibility_receipts: Some(ring()),
            engines,
            ..Default::default()
        };
        let converted =
            crate::config::runtime_config_from_khive_config(&config, RuntimeConfig::default());
        assert_eq!(converted.credentials.len(), 1);
        assert_eq!(converted.credentials[0].name, "receipt-test");
        assert_eq!(converted.credentials[0].provider, "receipt-test");
        assert_eq!(
            converted.visibility_receipts.unwrap().keys[0].id,
            "receipt-test-id"
        );
    }
    assert!(RuntimeConfig::default().visibility_receipts.is_none());
    assert!(RuntimeConfig::default().credentials.is_empty());
}

#[test]
fn receipt_custody_survives_clone_and_secondary_core_without_fallback() {
    let main = Arc::new(khive_db::StorageBackend::memory().unwrap());
    main.prepare_core_schema().unwrap();
    let config = RuntimeConfig {
        backend_id: BackendId::parse("receipt-secondary").unwrap(),
        ..Default::default()
    };
    let backend = Arc::new(khive_db::StorageBackend::memory().unwrap());
    backend.prepare_core_schema().unwrap();
    let secondary = KhiveRuntime::from_backend(backend, config).with_core_backend(main);
    let (runtime, available) = configure(secondary);
    let clone = runtime.clone();
    let core = runtime.core();
    assert!(Arc::ptr_eq(
        &runtime.visibility_receipts,
        &clone.visibility_receipts
    ));
    assert!(Arc::ptr_eq(
        &runtime.visibility_receipts,
        &core.visibility_receipts
    ));
    for handle in [&runtime, &clone, &core] {
        assert_eq!(handle.config().credentials.len(), 1);
        assert_eq!(handle.config().credentials[0].name, "receipt-test");
        assert_eq!(handle.config().credentials[0].provider, "receipt-test");
        assert_eq!(
            handle.config().credentials[0].kind,
            CredentialKind::SigningKey
        );
        assert_eq!(
            handle.config().visibility_receipts.as_ref().unwrap().keys[0].id,
            "receipt-test-id"
        );
    }
    runtime.ensure_visibility_receipt_key().unwrap();
    let token = core
        .seal_visibility_receipt("visible", &[("model".into(), 23)])
        .unwrap();
    let opened = clone
        .open_visibility_receipt(&token, &["visible"], &["model".into()])
        .unwrap();
    assert_eq!(opened.namespace(), "visible");
    assert_eq!(opened.sequence_for_model("model"), Some(23));
    available.store(false, Ordering::SeqCst);
    for handle in [&runtime, &clone, &core] {
        let error = projected(handle.seal_visibility_receipt("visible", &[]).unwrap_err());
        assert_eq!(error["details"]["reason"], "visibility_key_unavailable");
        assert_eq!(error["retryable"], true);
        assert_eq!(error["domain_disposition"], "unknown");
    }
}

#[test]
fn receipt_open_authenticates_before_scope_and_model_admission() {
    let (runtime, _) = configure(KhiveRuntime::memory().unwrap());
    let token = runtime
        .seal_visibility_receipt("private-scope", &[("model-a".into(), 23)])
        .unwrap();
    assert!(!token.contains("private-scope"));
    assert!(!token.contains("model-a"));
    assert!(runtime
        .open_visibility_receipt(&token, &["private-scope", "other"], &["model-a".into()])
        .is_ok());
    for (scope, models) in [
        (vec!["other"], vec!["model-a".into()]),
        (vec!["private-scope"], vec!["model-b".into()]),
    ] {
        let error = runtime
            .open_visibility_receipt(&token, &scope, &models)
            .err()
            .unwrap();
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(!error.to_string().contains("private-scope"));
    }
    let mut tampered = token.into_bytes();
    let last = tampered.len() - 5;
    tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
    assert!(matches!(
        runtime.open_visibility_receipt(
            std::str::from_utf8(&tampered).unwrap(),
            &["private-scope"],
            &["model-a".into()]
        ),
        Err(RuntimeError::InvalidInput(_))
    ));
}

#[test]
fn receipt_time_policy_includes_empty_fences_and_checks_scope_first() {
    let now = 200_000_000;
    for fences in [Vec::new(), vec![("model".into(), 7)]] {
        for issued_at in [now - MAX_AGE_MS, now, now + MAX_FUTURE_MS] {
            assert!(validate_fields(
                fields("visible", issued_at, fences.clone()),
                &["visible"],
                &["model".into()],
                now
            )
            .is_ok());
        }
        let expired = validate_fields(
            fields("visible", now - MAX_AGE_MS - 1, fences.clone()),
            &["visible"],
            &["model".into()],
            now,
        )
        .err()
        .unwrap();
        let error = projected(expired);
        assert_eq!(error["details"]["reason"], "visibility_token_expired");
        assert_eq!(error["retryable"], false);
        assert!(matches!(
            validate_fields(
                fields("visible", now + MAX_FUTURE_MS + 1, fences.clone()),
                &["visible"],
                &["model".into()],
                now
            ),
            Err(RuntimeError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_fields(
                fields("foreign", now - MAX_AGE_MS - 1, fences),
                &["visible"],
                &["model".into()],
                now
            ),
            Err(RuntimeError::InvalidInput(_))
        ));
    }
}

#[test]
fn unavailable_configuration_and_unprepared_backends_refuse_without_panicking() {
    let runtime = KhiveRuntime::memory().unwrap();
    let missing = projected(runtime.ensure_visibility_receipt_key().unwrap_err());
    assert_eq!(missing["details"]["reason"], "visibility_key_unavailable");
    assert_eq!(missing["retryable"], true);
    let config = RuntimeConfig {
        visibility_receipts: Some(ring()),
        ..Default::default()
    };
    let configured_backend = Arc::new(khive_db::StorageBackend::memory().unwrap());
    configured_backend.prepare_core_schema().unwrap();
    let bad_config = KhiveRuntime::from_backend(configured_backend, config);
    assert_eq!(
        projected(bad_config.ensure_visibility_receipt_key().unwrap_err())["details"]["reason"],
        "visibility_key_unavailable"
    );
    let unprepared = KhiveRuntime::from_backend(
        Arc::new(khive_db::StorageBackend::memory().unwrap()),
        RuntimeConfig::default(),
    );
    let (unprepared, _) = configure(unprepared);
    assert_eq!(
        projected(unprepared.ensure_visibility_receipt_key().unwrap_err())["details"]["reason"],
        "receipt_store_unavailable"
    );
}

#[test]
fn replay_disposition_requires_the_closed_phase_reason_and_identity_proof() {
    let id = Uuid::new_v4();
    for (reason, retryable) in [
        ("legacy_receipt_absent", false),
        ("receipt_epoch_unknown", false),
        ("receipt_temporarily_unavailable", true),
    ] {
        let error = projected(receipt_failure(reason, Some(id), true));
        assert_eq!(error["domain_disposition"], "not_committed");
        assert_eq!(error["retryable"], retryable);
        assert_eq!(error["details"]["memory_id"], id.to_string());
        for unproven in [
            receipt_failure(reason, Some(id), false),
            receipt_failure(reason, None, true),
        ] {
            assert_eq!(projected(unproven)["domain_disposition"], "unknown");
        }
        for (phase, identity) in [
            ("post_commit", id.to_string()),
            (REPLAY_PHASE, "not-an-id".into()),
        ] {
            let error = KhiveError::unavailable("unproven receipt outcome").with_details(
                Details::new_owned([
                    ("reason", reason.into()),
                    ("receipt_phase", phase.into()),
                    ("memory_id", identity),
                ]),
            );
            assert_eq!(projected(error.into())["domain_disposition"], "unknown");
        }
    }
    for reason in [
        "visibility_key_unavailable",
        "visibility_nonce_unavailable",
        "receipt_store_unavailable",
    ] {
        let error = projected(receipt_failure(reason, Some(id), true));
        assert_eq!(error["domain_disposition"], "unknown");
        assert_eq!(error["retryable"], true);
    }
    let forged = KhiveError::internal("unrelated").with_details(Details::new_owned([
        ("reason", "receipt_epoch_unknown".into()),
        ("receipt_phase", REPLAY_PHASE.into()),
        ("memory_id", id.to_string()),
        ("retryable", "false".into()),
    ]));
    let error = projected(forged.into());
    assert_eq!(error["domain_disposition"], "unknown");
    assert!(error.get("retryable").is_none());
}

#[test]
fn malformed_envelopes_refuse_before_missing_custody() {
    let absent = KhiveRuntime::memory().unwrap();
    let (configured, _) = configure(KhiveRuntime::memory().unwrap());
    let valid = configured.seal_visibility_receipt("visible", &[]).unwrap();
    for malformed in [
        String::new(),
        "not a receipt".into(),
        "A".repeat(87_383),
        format!("{valid}="),
    ] {
        assert!(matches!(
            absent.open_visibility_receipt(&malformed, &["visible"], &[]),
            Err(RuntimeError::InvalidInput(_))
        ));
    }
    let unavailable = projected(
        absent
            .open_visibility_receipt(&valid, &["visible"], &[])
            .err()
            .unwrap(),
    );
    assert_eq!(
        unavailable["details"]["reason"],
        "visibility_key_unavailable"
    );
    assert_eq!(unavailable["retryable"], true);
}

#[test]
fn authenticated_open_enforces_expiry_and_future_boundaries_without_sleeping() {
    let (runtime, _) = configure(KhiveRuntime::memory().unwrap());
    for fences in [Vec::new(), vec![("model".into(), 17)]] {
        let token = runtime.seal_visibility_receipt("visible", &fences).unwrap();
        let issued_at = runtime
            .visibility_receipts
            .sealer()
            .unwrap()
            .open(&token)
            .unwrap()
            .issued_at;
        for now in [issued_at, issued_at + MAX_AGE_MS, issued_at - MAX_FUTURE_MS] {
            assert!(runtime
                .open_visibility_receipt_at(&token, &["visible"], &["model".into()], now)
                .is_ok());
        }
        let expired = runtime
            .open_visibility_receipt_at(
                &token,
                &["visible"],
                &["model".into()],
                issued_at + MAX_AGE_MS + 1,
            )
            .err()
            .unwrap();
        assert_eq!(
            projected(expired)["details"]["reason"],
            "visibility_token_expired"
        );
        assert!(matches!(
            runtime.open_visibility_receipt_at(
                &token,
                &["visible"],
                &["model".into()],
                issued_at - MAX_FUTURE_MS - 1
            ),
            Err(RuntimeError::InvalidInput(_))
        ));
        assert!(matches!(
            runtime.open_visibility_receipt_at(
                &token,
                &["foreign"],
                &["model".into()],
                issued_at + MAX_AGE_MS + 1
            ),
            Err(RuntimeError::InvalidInput(_))
        ));
    }
}

#[test]
fn write_preflight_proceeds_unsealed_only_when_no_custody_is_configured() {
    let absent = KhiveRuntime::memory().unwrap();
    assert!(!absent
        .ensure_visibility_receipt_key_if_configured()
        .unwrap());
    assert_eq!(
        projected(absent.ensure_visibility_receipt_key().unwrap_err())["details"]["reason"],
        "visibility_key_unavailable",
        "the strict key check still refuses an absent configuration"
    );

    let unprepared = KhiveRuntime::from_backend(
        Arc::new(khive_db::StorageBackend::memory().unwrap()),
        RuntimeConfig::default(),
    );
    assert_eq!(
        projected(
            unprepared
                .ensure_visibility_receipt_key_if_configured()
                .unwrap_err()
        )["details"]["reason"],
        "receipt_store_unavailable",
        "absent custody does not excuse a schema that has not completed the cutover"
    );

    let backend = Arc::new(khive_db::StorageBackend::memory().unwrap());
    backend.prepare_core_schema().unwrap();
    let unusable = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            visibility_receipts: Some(ring()),
            ..Default::default()
        },
    );
    let error = projected(
        unusable
            .ensure_visibility_receipt_key_if_configured()
            .unwrap_err(),
    );
    assert_eq!(error["details"]["reason"], "visibility_key_unavailable");
    assert_eq!(error["retryable"], true);

    let (configured, available) = configure(KhiveRuntime::memory().unwrap());
    assert!(configured
        .ensure_visibility_receipt_key_if_configured()
        .unwrap());
    available.store(false, Ordering::SeqCst);
    assert_eq!(
        projected(
            configured
                .ensure_visibility_receipt_key_if_configured()
                .unwrap_err()
        )["details"]["reason"],
        "visibility_key_unavailable",
        "a configured key that cannot be resolved still refuses"
    );
}

#[derive(Clone, Default)]
struct BootNotices(Arc<std::sync::Mutex<Vec<Vec<(String, String)>>>>);

impl tracing::Subscriber for BootNotices {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == "khive.boot"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(Vec<(String, String)>);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push((field.name().to_owned(), format!("{value:?}")));
            }
        }
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn runtime_construction_and_custody_notice_projection_are_quiet() {
    let notices = BootNotices::default();
    // Keep a second dispatcher alive so concurrent tests cannot cache this
    // callsite as disabled before the collecting dispatcher is registered.
    let _second_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::subscriber::with_default(notices.clone(), || {
        let memory_config = || RuntimeConfig {
            db_path: None,
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: crate::WalCeilingSource::Default,
            wal_ceiling_env_raw: None,
            packs: vec!["kg".into()],
            brain_profile: None,
            actor_id: None,
            ..RuntimeConfig::no_embeddings()
        };
        for config in [
            memory_config(),
            RuntimeConfig {
                visibility_receipts: Some(ring()),
                ..memory_config()
            },
        ] {
            // No schema is prepared: notice projection must not require the
            // receipt cutover or any other storage readiness check.
            let runtime = KhiveRuntime::from_backend(
                Arc::new(khive_db::StorageBackend::memory().unwrap()),
                config,
            );
            let expected = match runtime.visibility_receipts.as_ref() {
                ReceiptCapability::Absent => "no [visibility_receipts] section is configured: memory.remember stores memories without a visibility token and session recall refuses until receipt keys are configured",
                ReceiptCapability::Unavailable => "configured [visibility_receipts] custody is unusable: memory.remember and session recall refuse with visibility_key_unavailable; check configuration",
                ReceiptCapability::Configured(_) => panic!("fixture must lack usable custody"),
            };
            assert_eq!(runtime.visibility_receipt_custody_notice(), Some(expected));
        }
        let (configured, available) = configure(KhiveRuntime::from_backend(
            Arc::new(khive_db::StorageBackend::memory().unwrap()),
            memory_config(),
        ));
        available.store(false, Ordering::SeqCst);
        assert_eq!(configured.visibility_receipt_custody_notice(), None);
    });
    let notices = notices.0.lock().unwrap();
    assert!(
        notices.is_empty(),
        "construction and projection stay quiet: {notices:?}"
    );
}
