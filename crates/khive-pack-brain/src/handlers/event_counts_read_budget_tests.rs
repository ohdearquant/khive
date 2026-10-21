use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

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

    // Through the registry the pack's state load reads first and meets the expired deadline,
    // so the request is refused before any count runs. That load reports the timeout as an
    // internal error naming it, not as a typed storage timeout.
    let dispatched = khive_storage::scope_request_read_deadline_at(
        expired(),
        registry.dispatch(
            "brain.event_counts",
            json!({"since": "2026-10-01T00:00:00Z"}),
        ),
    )
    .await;
    let error = dispatched.expect_err("an expired request must not return counts");
    assert!(
        error.to_string().contains("timeout"),
        "dispatched error: {error:?}"
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorReadStage {
    Rows,
    Boundary,
}

impl CursorReadStage {
    fn operation(self) -> &'static str {
        match self {
            Self::Rows => "cursor fixture row read",
            Self::Boundary => "cursor fixture boundary count",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum CursorRefusal {
    Timeout,
    Admission,
    Pool,
    DenseTie,
}

struct RefusingCursor {
    stage: CursorReadStage,
    refusal: CursorRefusal,
    rows: Vec<Event>,
    grouped_calls: AtomicUsize,
    query_calls: AtomicUsize,
    boundary_calls: AtomicUsize,
}

impl RefusingCursor {
    fn new(stage: CursorReadStage, refusal: CursorRefusal) -> Self {
        let rows = if stage == CursorReadStage::Boundary {
            (0..TRANSPORT_PAGE_ROWS)
                .map(|index| {
                    let mut event = Event::new(
                        "local",
                        "cursor.fixture",
                        khive_types::EventKind::Audit,
                        khive_types::SubstrateKind::Note,
                        "cursor-reader",
                    );
                    event.id = uuid::Uuid::from_u128(u128::from(index) + 1);
                    event.created_at = 100;
                    event
                })
                .collect()
        } else {
            Vec::new()
        };
        Self {
            stage,
            refusal,
            rows,
            grouped_calls: AtomicUsize::new(0),
            query_calls: AtomicUsize::new(0),
            boundary_calls: AtomicUsize::new(0),
        }
    }

    fn error(&self) -> khive_storage::StorageError {
        let operation = self.stage.operation().into();
        match self.refusal {
            CursorRefusal::Timeout => khive_storage::StorageError::Timeout { operation },
            CursorRefusal::Admission => khive_storage::StorageError::AdmissionTimeout {
                operation,
                timeout_ms: 37,
                pool_identity: Some("cursor-fixture-pool".into()),
            },
            CursorRefusal::Pool => khive_storage::StorageError::Pool {
                operation,
                message: "injected pool failure".into(),
            },
            CursorRefusal::DenseTie => panic!("dense tie returns a successful boundary count"),
        }
    }
}

#[async_trait::async_trait]
impl khive_storage::EventStore for RefusingCursor {
    async fn append_event(&self, _: Event) -> khive_storage::StorageResult<()> {
        panic!("cursor reads must not write")
    }

    async fn append_events(
        &self,
        _: Vec<Event>,
    ) -> khive_storage::StorageResult<khive_storage::BatchWriteSummary> {
        panic!("cursor reads must not write")
    }

    async fn get_event(&self, _: uuid::Uuid) -> khive_storage::StorageResult<Option<Event>> {
        panic!("cursor walk reads pages")
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: khive_storage::PageRequest,
    ) -> khive_storage::StorageResult<khive_storage::Page<Event>> {
        let index = self.query_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(page.offset, 0);
        assert_eq!(page.limit, TRANSPORT_PAGE_ROWS);
        if self.stage == CursorReadStage::Rows {
            return Err(self.error());
        }
        assert!(index < 2, "boundary refusal must stop the walk");
        assert_eq!(filter.before, if index == 0 { None } else { Some(101) });
        assert_eq!(self.rows.len(), TRANSPORT_PAGE_ROWS as usize);
        Ok(khive_storage::Page {
            items: self.rows.clone(),
            total: None,
        })
    }

    async fn count_events(&self, filter: EventFilter) -> khive_storage::StorageResult<u64> {
        self.boundary_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(self.stage, CursorReadStage::Boundary);
        assert_eq!(self.query_calls.load(Ordering::SeqCst), 2);
        assert_eq!(filter.after, Some(99));
        if matches!(self.refusal, CursorRefusal::DenseTie) {
            Ok(self.rows.len() as u64 + 1)
        } else {
            Err(self.error())
        }
    }

    async fn count_events_grouped(
        &self,
        _: EventFilter,
        group_by: khive_storage::event::EventGroupBy,
    ) -> khive_storage::StorageResult<std::collections::BTreeMap<String, u64>> {
        self.grouped_calls.fetch_add(1, Ordering::SeqCst);
        assert!(matches!(group_by, khive_storage::event::EventGroupBy::Kind));
        Ok([("audit".into(), u64::from(TRANSPORT_PAGE_ROWS) + 1)].into())
    }
}

async fn cursor_failure(stage: CursorReadStage, refusal: CursorRefusal, mode: u8) -> RuntimeError {
    let store = RefusingCursor::new(stage, refusal);
    let limit = TRANSPORT_PAGE_ROWS + 1;
    let mut counts = EventCountsAccumulator::new(None, None);
    let result = match mode {
        0 | 1 => fetch_event_counts_window(&store, &EventFilter::default(), mode == 1, limit)
            .await
            .map(|_| ()),
        2 => fold_event_counts_window_exhaustive(
            &store,
            &EventFilter::default(),
            TRANSPORT_PAGE_ROWS,
            u64::from(limit),
            &mut counts,
        )
        .await
        .map(|_| ()),
        _ => panic!("unknown fixture mode"),
    };
    let error = result.expect_err("a refused cursor read must not return a successful prefix");
    assert_eq!(store.grouped_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.query_calls.load(Ordering::SeqCst),
        if stage == CursorReadStage::Rows { 1 } else { 2 }
    );
    assert_eq!(
        store.boundary_calls.load(Ordering::SeqCst),
        usize::from(stage == CursorReadStage::Boundary)
    );
    if mode == 2 && stage == CursorReadStage::Boundary {
        let mut prefix = json!({});
        counts.add_to_result(&mut prefix, false);
        assert_eq!(prefix["counts_by_kind"]["audit"], TRANSPORT_PAGE_ROWS);
    }
    error
}

fn assert_cursor_refusal(error: RuntimeError, stage: CursorReadStage, refusal: CursorRefusal) {
    match (refusal, error) {
        (
            CursorRefusal::Timeout,
            RuntimeError::Storage(khive_storage::StorageError::Timeout { operation }),
        ) => {
            assert_eq!(operation, stage.operation());
        }
        (
            CursorRefusal::Admission,
            RuntimeError::Storage(khive_storage::StorageError::AdmissionTimeout {
                operation,
                timeout_ms,
                pool_identity,
            }),
        ) => {
            assert_eq!(operation, stage.operation());
            assert_eq!(timeout_ms, 37);
            assert_eq!(pool_identity.as_deref(), Some("cursor-fixture-pool"));
        }
        (_, other) => panic!("cursor refusal lost its variant or fields: {other:?}"),
    }
}

#[tokio::test]
async fn cursor_row_reads_preserve_typed_refusals_after_successful_grouped_counts() {
    for mode in 0..3 {
        for refusal in [CursorRefusal::Timeout, CursorRefusal::Admission] {
            let error = cursor_failure(CursorReadStage::Rows, refusal, mode).await;
            assert_cursor_refusal(error, CursorReadStage::Rows, refusal);
        }
    }
}

#[tokio::test]
async fn cursor_boundary_counts_preserve_typed_refusals_after_a_nonempty_prefix() {
    for mode in 0..3 {
        for refusal in [CursorRefusal::Timeout, CursorRefusal::Admission] {
            let error = cursor_failure(CursorReadStage::Boundary, refusal, mode).await;
            assert_cursor_refusal(error, CursorReadStage::Boundary, refusal);
        }
    }
}

#[tokio::test]
async fn cursor_non_timeout_storage_errors_keep_their_invalid_input_text() {
    for mode in 0..3 {
        for stage in [CursorReadStage::Rows, CursorReadStage::Boundary] {
            let expected = RefusingCursor::new(stage, CursorRefusal::Pool)
                .error()
                .to_string();
            let error = cursor_failure(stage, CursorRefusal::Pool, mode).await;
            assert!(matches!(error, RuntimeError::InvalidInput(message) if message == expected));
        }
    }
}

#[tokio::test]
async fn cursor_dense_timestamp_ties_keep_their_existing_invalid_input_text() {
    for mode in 0..3 {
        let error = cursor_failure(CursorReadStage::Boundary, CursorRefusal::DenseTie, mode).await;
        let expected = format!(
            "brain.event_counts cannot page this window: more than {TRANSPORT_PAGE_ROWS} \
             events share one created_at microsecond, which exceeds the event \
             transport's page cap; narrow `since`/`until` or add filters (`actor` \
             or `kind`)"
        );
        assert!(matches!(error, RuntimeError::InvalidInput(message) if message == expected));
    }
}
