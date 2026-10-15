use super::*;

struct RefusingCount {
    admission: bool,
}

#[async_trait::async_trait]
impl khive_storage::EventStore for RefusingCount {
    async fn append_event(&self, _: Event) -> khive_storage::StorageResult<()> {
        panic!("count refusal must not write")
    }

    async fn append_events(
        &self,
        _: Vec<Event>,
    ) -> khive_storage::StorageResult<khive_storage::BatchWriteSummary> {
        panic!("count refusal must not write")
    }

    async fn get_event(&self, _: uuid::Uuid) -> khive_storage::StorageResult<Option<Event>> {
        panic!("count refusal must precede row reads")
    }

    async fn query_events(
        &self,
        _: EventFilter,
        _: khive_storage::PageRequest,
    ) -> khive_storage::StorageResult<khive_storage::Page<Event>> {
        panic!("count refusal must precede row reads")
    }

    async fn count_events(&self, _: EventFilter) -> khive_storage::StorageResult<u64> {
        panic!("grouped count must not fall back to a scalar")
    }

    async fn count_events_grouped(
        &self,
        _: EventFilter,
        _: khive_storage::event::EventGroupBy,
    ) -> khive_storage::StorageResult<std::collections::BTreeMap<String, u64>> {
        let operation =
            "count fixture (operator read budget 30s; earlier caller deadline retained)".into();
        Err(if self.admission {
            khive_storage::StorageError::AdmissionTimeout {
                operation,
                timeout_ms: 17,
                pool_identity: Some("fixture-pool".into()),
            }
        } else {
            khive_storage::StorageError::Timeout { operation }
        })
    }
}

#[tokio::test]
async fn both_grouped_count_adapters_preserve_typed_read_refusals_without_fallback() {
    for admission in [false, true] {
        for unfiltered in [false, true] {
            let error = fetch_event_counts_window(
                &RefusingCount { admission },
                &EventFilter::default(),
                unfiltered,
                1,
            )
            .await
            .unwrap_err();
            let RuntimeError::Storage(error) = error else {
                panic!("read error must keep its storage variant");
            };
            match (admission, error) {
                (false, khive_storage::StorageError::Timeout { operation }) => {
                    assert_eq!(operation, "count fixture (operator read budget 30s; earlier caller deadline retained)");
                }
                (
                    true,
                    khive_storage::StorageError::AdmissionTimeout {
                        operation,
                        timeout_ms,
                        pool_identity,
                    },
                ) => {
                    assert_eq!(operation, "count fixture (operator read budget 30s; earlier caller deadline retained)");
                    assert_eq!(timeout_ms, 17);
                    assert_eq!(pool_identity.as_deref(), Some("fixture-pool"));
                }
                (_, other) => panic!("read error fields changed: {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn actual_event_counts_handler_refuses_an_expired_original_request() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize_with_visibility(Namespace::local(), runtime.visible_namespaces().to_vec())
        .unwrap();
    let pack = BrainPack::new(runtime.clone());
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime));
    let registry = builder.build().unwrap();
    let expired = || khive_storage::RequestReadDeadline::after(std::time::Duration::ZERO);

    // The handler checks the original deadline before any read and names the operator budget.
    let direct = khive_storage::scope_request_read_deadline_at(
        expired(),
        pack.handle_event_counts(&token, json!({"since": "2026-10-01T00:00:00Z"})),
    )
    .await;
    assert!(
        matches!(&direct, Err(RuntimeError::Storage(khive_storage::StorageError::Timeout { operation })) if operation.contains("operator read budget") && operation.contains("brain.event_counts")),
        "direct handler result: {direct:?}"
    );

    // Through the registry the pack's state load reads first, so the refusal is a typed
    // timeout from whichever read meets the expired deadline; no count is returned.
    let dispatched = khive_storage::scope_request_read_deadline_at(
        expired(),
        registry.dispatch(
            "brain.event_counts",
            json!({"since": "2026-10-01T00:00:00Z"}),
        ),
    )
    .await;
    assert!(
        matches!(
            &dispatched,
            Err(RuntimeError::Storage(
                khive_storage::StorageError::Timeout { .. }
            ))
        ),
        "dispatched result: {dispatched:?}"
    );
    let live = registry
        .dispatch(
            "brain.event_counts",
            json!({"since": "2026-10-01T00:00:00Z"}),
        )
        .await
        .unwrap();
    assert!(live["window_event_total"].as_u64().is_some());
}
