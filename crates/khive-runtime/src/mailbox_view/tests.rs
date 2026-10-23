use serde_json::json;

use super::*;
use crate::{
    config::{runtime_config_from_khive_config, BackendId},
    engine_config::KhiveConfig,
    AllowAllGate, CallerEnrollmentGate, Namespace, RuntimeConfig,
};

fn config(gate: GateRef) -> RuntimeConfig {
    RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        telemetry: Default::default(),
        web: Default::default(),
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
        ..crate::RuntimeConfig::no_embeddings()
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
                delegated: true,
                maintainer_actor: Some("lambda:owner".into()),
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
fn generic_note_reads_keep_the_callers_mailbox_and_base_policy() {
    let reader = token("lambda:reader");
    let allowed = KhiveRuntime::new(config(policy(Arc::new(AllowAllGate)))).unwrap();
    let denied = KhiveRuntime::new(config(policy(Arc::new(CallerEnrollmentGate::new(
        vec![],
        false,
    )))))
    .unwrap();
    for verb in ["search", "get", "context", "neighbors"] {
        let args = json!({"mailbox_actor":"lambda:owner"});
        let view = allowed
            .authorize_mailbox_view(&reader, verb, None, &args)
            .unwrap();
        assert_eq!(view.actor_id, "lambda:reader");
        assert!(!view.delegated);
        assert!(matches!(
            allowed.authorize_mailbox_view(&reader, verb, Some("lambda:owner"), &args),
            Err(RuntimeError::InvalidInput(_))
        ));
        assert!(matches!(
            denied.authorize_mailbox_view(&reader, verb, None, &args),
            Err(RuntimeError::PermissionDenied { .. })
        ));
    }
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
            delegated: false,
            maintainer_actor: Some("lambda:owner".into()),
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

#[test]
fn probe_view_requires_an_exact_grant_and_preserves_caller_identity() {
    let reader = token("lambda:reader");
    let args = json!({"actor":"lambda:owner", "mailbox_actor":"lambda:reader"});
    let ungranted = KhiveRuntime::new(config(Arc::new(AllowAllGate))).unwrap();
    assert!(matches!(
        ungranted.authorize_mailbox_view(&reader, "comm.probe", Some("lambda:owner"), &args),
        Err(RuntimeError::PermissionDenied { reason, .. }) if reason == "mailbox_read_not_granted"
    ));
    let granted = KhiveRuntime::new(config(policy(Arc::new(AllowAllGate)))).unwrap();
    assert_eq!(
        granted
            .authorize_mailbox_view(&reader, "comm.probe", Some("lambda:owner"), &args)
            .unwrap(),
        MailboxView {
            actor_id: "lambda:owner".into(),
            delegated: true,
            maintainer_actor: Some("lambda:owner".into()),
        }
    );
    assert_eq!(reader.actor(), &ActorRef::new("actor", "lambda:reader"));
    for other in [
        token("lambda:reader:child"),
        NamespaceToken::mint_authorized(
            Namespace::local(),
            ActorRef::new("agent", "lambda:reader"),
        ),
    ] {
        assert!(matches!(
            granted.authorize_mailbox_view(&other, "comm.probe", Some("lambda:owner"), &args),
            Err(RuntimeError::PermissionDenied { reason, .. }) if reason == "mailbox_read_not_granted"
        ));
    }
    assert!(matches!(
        granted.authorize_mailbox_view(
            &reader, "comm.probe", Some("lambda:owner:child"),
            &json!({"actor":"lambda:owner:child"})
        ),
        Err(RuntimeError::PermissionDenied { reason, .. }) if reason == "mailbox_read_not_granted"
    ));
}

#[test]
fn probe_view_keeps_own_named_and_anonymous_local_access() {
    let runtime = KhiveRuntime::new(config(Arc::new(AllowAllGate))).unwrap();
    let anonymous = NamespaceToken::mint_authorized(Namespace::local(), ActorRef::anonymous());
    for (caller, selected) in [
        (token("lambda:owner"), "lambda:owner"),
        (token("lambda:reader"), "lambda:reader"),
        (anonymous.clone(), "local"),
    ] {
        assert_eq!(
            runtime
                .authorize_mailbox_view(
                    &caller,
                    "comm.probe",
                    Some(selected),
                    &json!({"actor":selected})
                )
                .unwrap(),
            MailboxView {
                actor_id: selected.into(),
                delegated: false,
                maintainer_actor: Some("lambda:owner".into()),
            }
        );
    }
    assert!(matches!(
        runtime.authorize_mailbox_view(&anonymous, "comm.probe", Some("lambda:owner"), &json!({"actor":"lambda:owner"})),
        Err(RuntimeError::PermissionDenied { reason, .. }) if reason == "mailbox_read_not_granted"
    ));
    assert!(matches!(
        runtime.authorize_mailbox_view(&token("lambda:reader"), "comm.probe", Some("local"), &json!({"actor":"local"})),
        Err(RuntimeError::PermissionDenied { reason, .. }) if reason == "mailbox_read_not_granted"
    ));
}

#[test]
fn probe_view_validates_original_actor_and_preserves_failures() {
    let reader = token("lambda:reader");
    let runtime = KhiveRuntime::new(config(policy(Arc::new(AllowAllGate)))).unwrap();
    for (selector, args) in [
        (None, json!({"actor":null})),
        (None, json!({"actor":7})),
        (Some(" "), json!({"actor":" "})),
        (Some("lambda:owner"), json!({})),
        (None, json!({"actor":"lambda:owner"})),
        (Some("lambda:owner"), json!({"actor":"lambda:reader"})),
    ] {
        assert!(matches!(
            runtime.authorize_mailbox_view(&reader, "comm.probe", selector, &args),
            Err(RuntimeError::InvalidInput(_))
        ));
    }
    let args = json!({"actor":"lambda:owner"});
    let denied = KhiveRuntime::new(config(policy(Arc::new(CallerEnrollmentGate::new(
        vec![],
        false,
    )))))
    .unwrap();
    assert!(matches!(
        denied.authorize_mailbox_view(&reader, "comm.probe", Some("lambda:owner"), &args),
        Err(RuntimeError::PermissionDenied { reason, .. }) if reason == "actor is not enrolled"
    ));
    let broken = KhiveRuntime::new(config(Arc::new(BrokenMailboxBackend))).unwrap();
    assert!(matches!(
        broken.authorize_mailbox_view(&reader, "comm.probe", Some("lambda:owner"), &args),
        Err(RuntimeError::GateUnavailable { reason, .. }) if reason == "gate backend unavailable"
    ));
}

#[test]
fn am6_row_and_scope_override_match_independent_typed_quarantine_formula() {
    let q_values = [
        None,
        Some(Value::Null),
        Some(json!(true)),
        Some(json!("true")),
        Some(json!(1)),
        Some(json!(1.0)),
        Some(json!(false)),
        Some(json!("True")),
        Some(json!([true])),
        Some(json!({"quarantined":true})),
    ];
    let directions = [
        None,
        Some(json!("inbound")),
        Some(json!("outbound")),
        Some(Value::Null),
        Some(json!("unknown")),
        Some(json!(7)),
    ];
    let routes = [
        None,
        Some(json!("lambda:landing")),
        Some(json!("lambda:owner")),
        Some(Value::Null),
        Some(json!(["lambda:landing"])),
        Some(json!(7)),
    ];
    for (actor, delegated, anonymous) in [
        ("lambda:landing", false, false),
        ("lambda:owner", false, false),
        ("lambda:owner", true, false),
        ("lambda:landing", true, false),
        ("local", false, true),
        ("local", true, false),
    ] {
        let caller = if anonymous {
            NamespaceToken::mint_authorized(Namespace::local(), ActorRef::anonymous())
        } else {
            token(if delegated { "lambda:reader" } else { actor })
        };
        let original = MailboxView {
            actor_id: actor.into(),
            delegated,
            maintainer_actor: None,
        };
        for kind in ["message", "observation"] {
            for q in &q_values {
                for direction in &directions {
                    for to in &routes {
                        for from in &routes {
                            let mut properties = json!({});
                            for (key, value) in [
                                ("quarantined", q),
                                ("direction", direction),
                                ("to_actor", to),
                                ("from_actor", from),
                            ] {
                                if let Some(value) = value {
                                    properties[key] = value.clone();
                                }
                            }
                            let note = Note::new("local", kind, "formula fixture")
                                .with_properties(properties);
                            let old_b = original.permits_message_note(&caller, &note);
                            let exact_q = kind == "message"
                                && (q.as_ref() == Some(&json!(true))
                                    || q.as_ref() == Some(&json!("true")));
                            for configured in [false, true] {
                                let view = MailboxView {
                                    actor_id: actor.into(),
                                    delegated,
                                    maintainer_actor: configured.then(|| "lambda:owner".into()),
                                };
                                let expected = if configured && exact_q {
                                    actor == "lambda:owner"
                                } else {
                                    old_b
                                };
                                assert_eq!(view.permits_message_note(&caller, &note), expected,
                                    "row actor={actor} delegated={delegated} Q={q:?} direction={direction:?} to={to:?} from={from:?}");
                                assert_eq!(MailboxView::scope_permits_message_note(&view.note_scope(&caller), &note), expected,
                                    "generic cursor/key scope must match the same independent formula");
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn am6_authorized_view_snapshots_the_configured_maintainer_beside_selected_actor() {
    let reader = token("lambda:reader");
    let runtime = KhiveRuntime::new(config(policy(Arc::new(AllowAllGate)))).unwrap();
    let delegated = runtime
        .authorize_mailbox_view(
            &reader,
            "comm.inbox",
            Some("lambda:owner"),
            &json!({"mailbox_actor":"lambda:owner"}),
        )
        .unwrap();
    assert_eq!(delegated.actor_id, "lambda:owner");
    assert_eq!(delegated.maintainer_actor.as_deref(), Some("lambda:owner"));
    let own = runtime
        .authorize_mailbox_view(&reader, "get", None, &json!({}))
        .unwrap();
    assert_eq!(own.actor_id, "lambda:reader");
    assert_eq!(own.maintainer_actor.as_deref(), Some("lambda:owner"));
    assert_eq!(
        own.note_scope(&reader).maintainer_actor,
        own.maintainer_actor
    );
    for actor_id in [None, Some(String::new())] {
        let mut cfg = config(Arc::new(AllowAllGate));
        cfg.actor_id = actor_id;
        let unconfigured = KhiveRuntime::new(cfg).unwrap();
        assert_eq!(
            unconfigured
                .authorize_mailbox_view(&reader, "get", None, &json!({}))
                .unwrap()
                .maintainer_actor,
            None
        );
    }
}
