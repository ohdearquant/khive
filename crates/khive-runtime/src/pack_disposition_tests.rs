use super::*;
use crate::DomainDisposition;

#[tokio::test]
#[serial(config_ledger)]
async fn disposition_intercepted_success_preserves_typed_metadata() {
    #[derive(Debug, PartialEq)]
    struct Metadata {
        visited: Vec<&'static str>,
    }

    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(store.clone());
    let registry = builder.build().unwrap();
    let result = registry
        .dispatch_intercepted_with_metadata_and_disposition(
            "probe",
            &serde_json::json!({}),
            None,
            |_| async {
                Ok(InterceptedDispatchResult::new(
                    serde_json::json!({"id": "canonical-id"}),
                    Metadata {
                        visited: vec!["one", "two"],
                    },
                ))
            },
        )
        .await
        .unwrap();
    assert_eq!(result.result, serde_json::json!({"id": "canonical-id"}));
    assert_eq!(
        result.metadata,
        Metadata {
            visited: vec!["one", "two"]
        }
    );
    assert_eq!(store.events.lock().unwrap().len(), 1);
}

#[tokio::test]
#[serial(config_ledger)]
async fn disposition_is_assigned_at_validation_and_handler_raise_sites() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(FailingProbePack);
    let registry = builder.build().unwrap();
    let before = registry
        .dispatch_with_disposition("probe", serde_json::json!({"namespace": 3}), None)
        .await
        .unwrap_err();
    let handler = registry
        .dispatch_with_disposition("probe", serde_json::json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(before.disposition(), DomainDisposition::NotCommitted);
    assert_eq!(handler.disposition(), DomainDisposition::Unknown);
    assert!(matches!(before.source(), RuntimeError::InvalidInput(_)));
    assert!(matches!(handler.source(), RuntimeError::InvalidInput(message) if message == "boom"));
    assert!(matches!(
        registry.dispatch("probe", serde_json::json!({})).await,
        Err(RuntimeError::InvalidInput(message)) if message == "boom"
    ));
    let unknown = registry
        .dispatch_with_disposition("absent", serde_json::json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(unknown.disposition(), DomainDisposition::NotCommitted);
    assert!(matches!(unknown.source(), RuntimeError::UnknownVerb(_)));
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
#[serial(audit_obligation_append_failures)]
async fn disposition_obligation_retains_committed_row_and_original_handler_error() {
    let runtime = crate::KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let notes = runtime.notes(&token).unwrap();
    let audit = Arc::new(MemoryEventStore {
        fail_appends: true,
        ..Default::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(audit);
    let registry = builder.build().unwrap();

    for handler_fails in [false, true] {
        let note = khive_storage::Note::new("local", "observation", "committed before audit");
        let id = note.id;
        let error = registry
            .dispatch_intercepted_with_metadata_and_disposition(
                "write_probe",
                &serde_json::json!({}),
                None,
                |_| async {
                    notes.upsert_note(note).await?;
                    if handler_fails {
                        return Err(RuntimeError::InvalidInput(
                            "handler failed after writing".into(),
                        ));
                    }
                    Ok(InterceptedDispatchResult::new(
                        serde_json::json!({"id": id}),
                        (),
                    ))
                },
            )
            .await
            .unwrap_err();
        assert!(notes.get_note(id).await.unwrap().is_some());
        if handler_fails {
            assert_eq!(error.disposition(), DomainDisposition::Unknown);
            assert!(
                matches!(error.source(), RuntimeError::InvalidInput(message) if message == "handler failed after writing")
            );
        } else {
            assert_eq!(error.disposition(), DomainDisposition::Committed);
            let RuntimeError::AuditObligation {
                failure,
                domain_result,
            } = error.into_source()
            else {
                panic!("successful write must keep its result in the obligation error");
            };
            assert_eq!(failure.wire_code(), "store_failure");
            assert_eq!(domain_result, serde_json::json!({"id": id}));
        }
    }
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
#[serial(audit_obligation_append_failures)]
async fn disposition_nested_failure_never_claims_the_outer_operation() {
    let runtime = crate::KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let notes = runtime.notes(&token).unwrap();
    let outer = VerbRegistryBuilder::new().build().unwrap();
    let mut child_builder = VerbRegistryBuilder::new();
    child_builder.register(AlphaPack);
    child_builder.with_event_store(Arc::new(MemoryEventStore {
        fail_appends: true,
        ..Default::default()
    }));
    let child = child_builder.build().unwrap();

    for child_verb in ["missing", "create"] {
        let note = khive_storage::Note::new("local", "observation", "outer domain effect");
        let id = note.id;
        let error = outer
            .dispatch_intercepted_with_metadata_and_disposition(
                "outer",
                &serde_json::json!({}),
                None,
                |_| async {
                    notes.upsert_note(note).await?;
                    child.dispatch(child_verb, serde_json::json!({})).await?;
                    Ok(InterceptedDispatchResult::new(
                        serde_json::json!({"outer_id": id}),
                        (),
                    ))
                },
            )
            .await
            .unwrap_err();
        assert!(notes.get_note(id).await.unwrap().is_some());
        assert_eq!(error.disposition(), DomainDisposition::Unknown);
        match child_verb {
            "missing" => assert!(matches!(error.source(), RuntimeError::UnknownVerb(_))),
            "create" => assert!(
                matches!(error.source(), RuntimeError::AuditObligation { domain_result, .. } if domain_result.get("outer_id").is_none())
            ),
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
#[serial(audit_obligation_append_failures)]
async fn disposition_receipt_setup_and_submission_keep_the_held_report() {
    for (configured, fail_appends, report, code, branch) in [
        (
            false,
            false,
            serde_json::json!({"project_id": uuid::Uuid::new_v4(), "count": 7}),
            "git_digest_receipt_failure",
            "event store",
        ),
        (
            true,
            false,
            serde_json::json!(["invalid", "report"]),
            "git_digest_receipt_failure",
            "not an object",
        ),
        (
            true,
            false,
            serde_json::json!({"project_id": "bad", "count": 8}),
            "git_digest_receipt_failure",
            "valid project_id",
        ),
        (
            true,
            true,
            serde_json::json!({"project_id": uuid::Uuid::new_v4(), "count": 9}),
            "store_failure",
            "audit submission",
        ),
    ] {
        let mut builder = VerbRegistryBuilder::new();
        if configured {
            builder.with_event_store(Arc::new(MemoryEventStore {
                fail_appends,
                ..Default::default()
            }));
        }
        let registry = builder.build().unwrap();
        let expected = report.clone();
        let error = registry
            .dispatch_intercepted_with_metadata_and_disposition(
                "git.digest",
                &serde_json::json!({}),
                None,
                |_| async { Ok(InterceptedDispatchResult::new(report, ())) },
            )
            .await
            .unwrap_err();
        assert_eq!(error.disposition(), DomainDisposition::Committed);
        let RuntimeError::AuditObligation {
            failure,
            mut domain_result,
        } = error.into_source()
        else {
            panic!("receipt failure must retain the report");
        };
        assert_eq!(failure.wire_code(), code);
        assert!(failure.message.contains(branch), "{}", failure.message);
        if fail_appends {
            assert!(domain_result["receipt_id"].as_str().is_some());
            domain_result.as_object_mut().unwrap().remove("receipt_id");
        }
        assert_eq!(domain_result, expected);
    }
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
#[serial(audit_obligation_append_failures)]
async fn disposition_audit_deadline_keeps_one_write_and_one_late_audit_row() {
    for verb in ["write_probe", "git.digest"] {
        let runtime = crate::KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let notes = runtime.notes(&token).unwrap();
        let append_started = Arc::new(tokio::sync::Notify::new());
        let append_release = Arc::new(tokio::sync::Notify::new());
        let store = Arc::new(MemoryEventStore {
            append_started: Some(append_started.clone()),
            append_release: Some(append_release.clone()),
            ..Default::default()
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.with_event_store(store.clone());
        builder.with_audit_batch_config(crate::audit_batch::AuditBatchConfig {
            admission_deadline: std::time::Duration::from_millis(30),
            ..Default::default()
        });
        let registry = Arc::new(builder.build().unwrap());
        let handler_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let project_id = uuid::Uuid::new_v4();
        let unresolved_before = crate::pack::audit_admission_unresolved_obligation_count();
        let mut dispatch = tokio::spawn({
            let registry = registry.clone();
            let notes = notes.clone();
            let handler_calls = handler_calls.clone();
            async move {
                registry
                    .dispatch_intercepted_with_metadata_and_disposition(
                        verb,
                        &serde_json::json!({}),
                        None,
                        |_| async move {
                            handler_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            // A rerun creates a new row, so the persisted count detects
                            // replay rather than hiding it behind an idempotent upsert.
                            let note = khive_storage::Note::new(
                                "local",
                                "observation",
                                "one domain write",
                            );
                            let id = note.id;
                            notes.upsert_note(note).await?;
                            Ok(InterceptedDispatchResult::new(
                                serde_json::json!({"id": id, "project_id": project_id, "count": 1}),
                                (),
                            ))
                        },
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), append_started.notified())
            .await
            .expect("the submitted audit row reaches the held store");
        assert_eq!(notes.count_notes("local", None).await.unwrap(), 1);

        // The response must resolve while the store is still held. Release
        // before asserting the timeout result so a regression can drain cleanly.
        let response = tokio::time::timeout(std::time::Duration::from_secs(1), &mut dispatch).await;
        let audit_was_uncommitted = store.events.lock().unwrap().is_empty();
        append_release.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            registry.shutdown_audit_batch(),
        )
        .await
        .expect("released audit generation drains")
        .expect("late audit commit succeeds");
        let response = response
            .expect("audit deadline returns without waiting for the store release")
            .expect("dispatch task joins");
        assert!(audit_was_uncommitted);
        // The domain write committed and its audit row is enqueued; the
        // generation commits that row on its own, so the dispatch reports the
        // committed result. Only the git.digest receipt stays strict: there the
        // audit row is the receipt the caller is promised.
        let domain_result = if verb == "git.digest" {
            let error = response.unwrap_err();
            assert_eq!(error.disposition(), DomainDisposition::Committed);
            assert!(error.source().retryable_failure_context().is_none());
            let RuntimeError::AuditObligation {
                failure,
                domain_result,
            } = error.into_source()
            else {
                panic!("expired receipt must retain the canonical domain result");
            };
            assert_eq!(failure.wire_code(), "admission_deadline_expired");
            assert_eq!(
                failure.reason,
                crate::AuditObligationReason::Terminal(
                    crate::audit_batch::AuditTerminalReason::AdmissionDeadlineExpired
                )
            );
            domain_result
        } else {
            response
                .expect("a committed write whose audit row is enqueued reports success")
                .result
        };
        assert_eq!(domain_result["project_id"], serde_json::json!(project_id));
        assert_eq!(domain_result["count"], 1);
        if verb != "git.digest" {
            assert_eq!(
                crate::pack::audit_admission_unresolved_obligation_count(),
                unresolved_before + 1,
                "a degraded write counts on the unresolved-obligation counter"
            );
        }
        let id = domain_result["id"].as_str().unwrap().parse().unwrap();
        assert!(notes.get_note(id).await.unwrap().is_some());
        assert_eq!(notes.count_notes("local", None).await.unwrap(), 1);
        assert_eq!(handler_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let events = store.events.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "the accepted audit row drains exactly once"
        );
        assert_eq!(events[0].verb, verb);
        assert_eq!(events[0].outcome, EventOutcome::Success);
        if verb == "git.digest" {
            assert_eq!(domain_result["receipt_id"], serde_json::json!(events[0].id));
        }
    }
}

#[test]
fn disposition_obligation_does_not_inherit_audit_retry_permission() {
    let source = khive_storage::StorageError::WriteQueueFull { timeout_ms: 12 };
    let result = fold_audit_obligation(
        Ok(serde_json::json!({"id": "already-created"})),
        Err(AuditObligationFailure::from_store("write_probe", source)),
        std::convert::identity,
    );
    let error = result.unwrap_err();
    assert!(error.retryable_failure_context().is_none());
    let failure = std::error::Error::source(&error).expect("typed obligation source");
    let storage = failure.source().expect("original storage source");
    assert!(matches!(
        storage.downcast_ref::<khive_storage::StorageError>(),
        Some(khive_storage::StorageError::WriteQueueFull { timeout_ms: 12 })
    ));
}
