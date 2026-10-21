//! Dispatch failures expose closed classes without copying their messages.
use async_trait::async_trait;
use khive_gate::{Gate, GateDecision, GateError, GateRequest};
use khive_runtime::audit_error::AuditErrorClass;
use khive_runtime::pack::{HandlerDef, PackRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_runtime::runtime::NamespaceToken;
use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::{Event, EventFilter, EventStore, PageRequest, StorageError};
use khive_types::pack::{Pack, Visibility};
use khive_types::{EventKind, EventOutcome, Namespace, VerbCategory};
use serde_json::{json, Value};
use std::sync::Arc;

type ErrorFactory = fn() -> RuntimeError;
struct Probe(Option<ErrorFactory>);
impl Pack for Probe {
    const NAME: &'static str = "audit_probe";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "audit_probe.run",
        description: "audit fixture",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}
#[async_trait]
impl PackRuntime for Probe {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        _: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match self.0 {
            Some(error) => Err(error()),
            None => Ok(json!({"ok":true})),
        }
    }
}

fn registry(
    error: Option<ErrorFactory>,
    gate: Option<Arc<dyn Gate>>,
) -> (VerbRegistry, Arc<dyn EventStore>) {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let store = rt.events(&token).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(Probe(error));
    builder.with_event_store(store.clone());
    if let Some(gate) = gate {
        builder.with_gate(gate);
    }
    (builder.build().unwrap(), store)
}

async fn event(store: &Arc<dyn EventStore>, verb: &str) -> Event {
    let mut rows = store
        .query_events(
            EventFilter {
                kinds: vec![EventKind::Audit],
                verbs: vec![verb.into()],
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap()
        .items;
    assert_eq!(rows.len(), 1, "one dispatch row per call");
    rows.pop().unwrap()
}

fn cases() -> Vec<(&'static str, ErrorFactory)> {
    vec![
        ("policy_refusal", || {
            RuntimeError::permission_denied("audit_probe.run", "private-error-text")
        }),
        ("invalid_params", || {
            RuntimeError::InvalidInput("private-error-text".into())
        }),
        ("not_found_or_not_addressed", || {
            RuntimeError::NotFound("private-error-text".into())
        }),
        ("deadline_exceeded", || RuntimeError::DeadlineExceeded {
            operation: "private-error-text".into(),
            budget_ms: 10,
            elapsed_ms: 11,
        }),
        ("admission_exhausted", || {
            RuntimeError::Storage(StorageError::WriteQueueFull { timeout_ms: 10 })
        }),
        ("conflict", || {
            RuntimeError::Storage(StorageError::Conflict {
                capability: khive_storage::StorageCapability::Sql,
                operation: "fixture".into(),
                message: "private-error-text".into(),
            })
        }),
        ("unsupported", || {
            RuntimeError::CanonicalUnsupportedOperation {
                op_index: 0,
                operation: "fixture",
            }
        }),
        ("configuration", || {
            RuntimeError::Unconfigured("private-error-text".into())
        }),
        ("storage", || {
            RuntimeError::Storage(StorageError::Internal("private-error-text".into()))
        }),
        ("unavailable", || RuntimeError::GateUnavailable {
            verb: "audit_probe.run".into(),
            reason: "private-error-text".into(),
        }),
        ("internal", || {
            RuntimeError::Internal("private-error-text".into())
        }),
    ]
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn every_handler_class_is_recorded_on_ordinary_and_intercepted_dispatch() {
    for (expected, error) in cases() {
        for intercepted in [false, true] {
            let (registry, store) = registry(Some(error), None);
            let failure = if intercepted {
                registry
                    .dispatch_intercepted_with_identity(
                        "audit_probe.run",
                        &json!({}),
                        None,
                        |_| async { Err(error()) },
                    )
                    .await
                    .unwrap_err()
            } else {
                registry
                    .dispatch("audit_probe.run", json!({}))
                    .await
                    .unwrap_err()
            };
            assert_eq!(failure.audit_error_class().as_str(), expected);
            let row = event(&store, "audit_probe.run").await;
            assert_eq!(row.outcome, EventOutcome::Error);
            assert_eq!(row.payload["error_class"], expected);
            assert!(!row.payload.to_string().contains("private-error-text"));
            assert!(row.payload["resource"].get("cost_unit").is_none());
        }
    }
}

#[derive(Debug)]
struct Deny;
impl Gate for Deny {
    fn check(&self, _: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::deny("fixture refusal"))
    }
}
#[derive(Debug)]
struct Unavailable;
impl Gate for Unavailable {
    fn check(&self, _: &GateRequest) -> Result<GateDecision, GateError> {
        Err(GateError::Internal("fixture unavailable".into()))
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn registry_denials_are_distinct_from_handler_policy_refusals() {
    for intercepted in [false, true] {
        let (registry, store) = registry(None, Some(Arc::new(Deny)));
        let result = if intercepted {
            registry
                .dispatch_intercepted_with_identity(
                    "audit_probe.run",
                    &json!({}),
                    None,
                    |_| async { panic!("gate denied before callback") },
                )
                .await
        } else {
            registry.dispatch("audit_probe.run", json!({})).await
        };
        assert!(matches!(result, Err(RuntimeError::PermissionDenied { .. })));
        let row = event(&store, "audit_probe.run").await;
        assert_eq!(row.outcome, EventOutcome::Denied);
        assert_eq!(row.payload["error_class"], "gate_refusal");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unavailable_gate_unknown_verb_and_success_keep_distinct_shapes() {
    for intercepted in [false, true] {
        let (registry, store) = registry(None, Some(Arc::new(Unavailable)));
        let result = if intercepted {
            registry
                .dispatch_intercepted_with_identity(
                    "audit_probe.run",
                    &json!({}),
                    None,
                    |_| async { panic!("gate failed before callback") },
                )
                .await
        } else {
            registry.dispatch("audit_probe.run", json!({})).await
        };
        assert!(matches!(result, Err(RuntimeError::GateUnavailable { .. })));
        assert_eq!(
            event(&store, "audit_probe.run").await.payload["error_class"],
            "unavailable"
        );
    }
    let (ordinary, store) = registry(None, None);
    assert!(matches!(
        ordinary.dispatch("absent", json!({})).await,
        Err(RuntimeError::UnknownVerb(_))
    ));
    assert_eq!(
        event(&store, "absent").await.payload["error_class"],
        "invalid_params"
    );
    ordinary
        .dispatch("audit_probe.run", json!({}))
        .await
        .unwrap();
    let row = event(&store, "audit_probe.run").await;
    assert_eq!(row.outcome, EventOutcome::Success);
    assert!(row.payload.get("error_class").is_none());

    let (intercepted, store) = registry(None, None);
    let result = intercepted
        .dispatch_intercepted_with_identity("audit_probe.run", &json!({}), None, |_| async {
            Ok(json!({"ok": true}))
        })
        .await
        .unwrap();
    assert_eq!(result, json!({"ok": true}));
    let row = event(&store, "audit_probe.run").await;
    assert_eq!(row.outcome, EventOutcome::Success);
    assert!(row.payload.get("error_class").is_none());
}

#[test]
fn future_and_invalid_tokens_never_default_to_internal() {
    for token in ["", "future_failure", "INTERNAL", "deadline_exceeded "] {
        assert_eq!(AuditErrorClass::from_token(token), None);
    }
    for (expected, error) in cases() {
        let class = error().audit_error_class();
        assert_eq!(AuditErrorClass::from_token(expected), Some(class));
    }
    assert_eq!(
        AuditErrorClass::from_token("gate_refusal"),
        Some(AuditErrorClass::GateRefusal)
    );
}

#[test]
fn evidence_and_storage_wrappers_keep_the_typed_source_class() {
    let wrapped = RuntimeError::CanonicalAdmission {
        op_index: 3,
        source: Box::new(RuntimeError::Storage(
            StorageError::WriterTaskRequestFailed {
                request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                source: Box::new(StorageError::WriteQueueFull { timeout_ms: 5 }),
            },
        )),
    };
    assert_eq!(
        wrapped.audit_error_class(),
        AuditErrorClass::AdmissionExhausted
    );
    let erased =
        RuntimeError::Internal("deadline_exceeded policy_refusal private-error-text".into());
    assert_eq!(erased.audit_error_class(), AuditErrorClass::Internal);
}
