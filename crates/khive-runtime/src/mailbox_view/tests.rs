use serde_json::json;

use super::*;
use crate::{
    config::{runtime_config_from_khive_config, BackendId},
    engine_config::KhiveConfig,
    AllowAllGate, CallerEnrollmentGate, Namespace, RuntimeConfig,
};

fn config(gate: GateRef) -> RuntimeConfig {
    RuntimeConfig {
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: chrono_tz::Tz::UTC,
        events_split: None,
        db_path: None,
        blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate,
        packs: vec![],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: Some("lambda:owner".into()),
        exec: Default::default(),
    }
}

fn token(id: &str) -> NamespaceToken {
    NamespaceToken::mint_authorized(Namespace::local(), ActorRef::new("actor", id))
}

fn policy(inner: GateRef) -> GateRef {
    Arc::new(
        MailboxReadGate::new(
            inner,
            ActorRef::new("actor", "lambda:owner"),
            vec![ActorRef::new("actor", "lambda:reader")],
        )
        .unwrap(),
    )
}

#[test]
fn gate_mailbox_direct_api_preserves_actor_and_rechecks_base_policy() {
    let reader = token("lambda:reader");
    let args = json!({"mailbox_actor":"lambda:owner", "namespace":"lambda:owner"});
    let runtime = KhiveRuntime::new(config(Arc::new(AllowAllGate))).unwrap();
    let error = runtime
        .authorize_mailbox_view(&reader, "comm.inbox", Some("lambda:owner"), &args)
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::PermissionDenied { reason, .. } if reason == "mailbox_read_not_granted")
    );

    let runtime = KhiveRuntime::new(config(policy(Arc::new(AllowAllGate)))).unwrap();
    for verb in ["comm.inbox", "comm.thread"] {
        let view = runtime
            .authorize_mailbox_view(&reader, verb, Some("lambda:owner"), &args)
            .unwrap();
        assert_eq!(
            view,
            MailboxView {
                actor_id: "lambda:owner".into(),
                delegated: true
            }
        );
        assert_eq!(reader.actor(), &ActorRef::new("actor", "lambda:reader"));
        let own_args = json!({"mailbox_actor":"lambda:reader"});
        assert!(
            !runtime
                .authorize_mailbox_view(&reader, verb, Some("lambda:reader"), &own_args)
                .unwrap()
                .delegated
        );
    }
    let denied = KhiveRuntime::new(config(policy(Arc::new(CallerEnrollmentGate::new(
        vec![],
        false,
    )))))
    .unwrap();
    let error = denied
        .authorize_mailbox_view(&reader, "comm.inbox", Some("lambda:owner"), &args)
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::PermissionDenied { reason, .. } if reason == "actor is not enrolled")
    );
    // Even an already minted token and an omitted own-view selector cannot
    // bypass a base policy that denies the actual read verb.
    assert!(matches!(
        denied.authorize_mailbox_view(&reader, "comm.inbox", None, &json!({})),
        Err(RuntimeError::PermissionDenied { .. })
    ));
}

#[test]
fn gate_mailbox_direct_api_validates_original_null_and_mismatched_selector() {
    let runtime = KhiveRuntime::new(config(Arc::new(AllowAllGate))).unwrap();
    let reader = token("lambda:reader");
    for (selector, args) in [
        (None, json!({"mailbox_actor":null})),
        (None, json!({"mailbox_actor":42})),
        (Some("local"), json!({"mailbox_actor":"local"})),
        (Some("lambda:owner"), json!({})),
        (None, json!({"mailbox_actor":"lambda:reader"})),
    ] {
        assert!(matches!(
            runtime.authorize_mailbox_view(&reader, "comm.inbox", selector, &args),
            Err(RuntimeError::InvalidInput(_))
        ));
    }
    let anonymous = NamespaceToken::mint_authorized(Namespace::local(), ActorRef::anonymous());
    let view = runtime
        .authorize_mailbox_view(&anonymous, "comm.inbox", None, &json!({}))
        .unwrap();
    assert_eq!(
        view,
        MailboxView {
            actor_id: "local".into(),
            delegated: false
        }
    );
    assert!(matches!(
        runtime.authorize_mailbox_view(&reader, "comm.read", None, &json!({})),
        Err(RuntimeError::InvalidInput(_))
    ));
}

#[derive(Debug)]
struct BrokenMailboxBackend;

impl Gate for BrokenMailboxBackend {
    fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::allow())
    }
    fn check_mailbox_read(
        &self,
        _req: &GateRequest,
        _owner: &ActorRef,
    ) -> Result<GateDecision, GateError> {
        Err(GateError::Internal("sensitive backend error".into()))
    }
}

#[test]
fn gate_mailbox_backend_failure_is_typed_and_safe() {
    let runtime = KhiveRuntime::new(config(Arc::new(BrokenMailboxBackend))).unwrap();
    let error = runtime
        .authorize_mailbox_view(
            &token("lambda:reader"),
            "comm.thread",
            Some("lambda:owner"),
            &json!({"mailbox_actor":"lambda:owner"}),
        )
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::GateUnavailable { reason, .. } if reason == "gate backend unavailable")
    );
}

#[test]
fn gate_mailbox_boot_conversion_requires_explicit_file_owner_and_fails_closed() {
    let reader = token("lambda:reader");
    let req = GateRequest::new(
        reader.actor().clone(),
        Namespace::local(),
        "comm.inbox",
        json!({"mailbox_actor":"lambda:owner"}),
    );
    let mut file = KhiveConfig::default();
    file.actor.mailbox_readers = vec!["lambda:reader".into()];
    // Base identity is deliberately the requested owner. It must not supply
    // the missing explicit serving-file grant owner.
    let invalid = runtime_config_from_khive_config(&file, config(Arc::new(AllowAllGate)));
    assert!(invalid.gate.check(&req).is_err());
    let invalid_fingerprint = invalid.gate.configuration_fingerprint().unwrap().to_owned();
    let runtime = KhiveRuntime::new(invalid).unwrap();
    assert!(runtime.authorize(Namespace::local()).is_err());
    assert!(matches!(
        runtime.authorize_mailbox_view(&reader, "comm.inbox", None, &json!({})),
        Err(RuntimeError::GateUnavailable { .. })
    ));

    file.actor.id = Some("lambda:owner".into());
    let valid = runtime_config_from_khive_config(&file, config(Arc::new(AllowAllGate)));
    assert!(check_with_mailbox_policy(valid.gate.as_ref(), &req)
        .unwrap()
        .is_allow());
    assert_ne!(
        valid.gate.configuration_fingerprint(),
        Some(invalid_fingerprint.as_str())
    );
    file.actor.mailbox_readers = vec![String::new()];
    let invalid = runtime_config_from_khive_config(&file, config(Arc::new(AllowAllGate)));
    assert!(invalid.gate.check(&req).is_err());
    assert_ne!(
        invalid.gate.configuration_fingerprint(),
        Some(invalid_fingerprint.as_str())
    );

    file.actor.mailbox_readers.clear();
    let base: GateRef = Arc::new(CallerEnrollmentGate::new(
        vec!["lambda:reader".into()],
        false,
    ));
    let empty = runtime_config_from_khive_config(&file, config(base.clone()));
    assert!(Arc::ptr_eq(&empty.gate, &base));
    assert!(
        matches!(check_with_mailbox_policy(empty.gate.as_ref(), &req).unwrap(), GateDecision::Deny { reason } if reason == "mailbox_read_not_granted")
    );
}
