use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use khive_storage::event::{EventAppendDisposition, IdempotentEventBatchResult};
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageError,
    StorageResult,
};
use khive_types::{HandlerDef, Pack, VerbCategory, Visibility};
use serde_json::{json, Value};

use crate::error::DispatchError;
use crate::pack::{
    AuditSuccessRowsMode, PackRuntime, RequestIdentity, VerbRegistry, VerbRegistryBuilder,
};
use crate::runtime::NamespaceToken;
use crate::RuntimeError;

pub const HEARTBEAT: &str = "comm.heartbeat";
pub const CLEANUP: &str = "comm.cleanup_expired_quarantine";
pub const OMITTED: [&str; 4] = ["comm.inbox", "comm.probe", "comm.cursor_get", "whoami"];

const fn handler(name: &'static str) -> HandlerDef {
    HandlerDef {
        name,
        description: "success audit fixture",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Declaration,
        params: &[],
    }
}
pub const COMM: &[HandlerDef] = &[
    handler("comm.inbox"),
    handler("comm.probe"),
    handler("comm.cursor_get"),
    handler(HEARTBEAT),
    handler(CLEANUP),
    handler("comm.cursor_set"),
];
pub const KG: &[HandlerDef] = &[handler("whoami"), handler("stats")];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allocation {
    pub verb: String,
    pub seq: u64,
    pub request: u64,
}
tokio::task_local! { static REQUEST: u64; }
static ALLOCATIONS: Mutex<Vec<Allocation>> = Mutex::new(Vec::new());
pub fn observe(verb: &str, seq: u64) {
    if let Ok(request) = REQUEST.try_with(|request| *request) {
        ALLOCATIONS.lock().unwrap().push(Allocation {
            verb: verb.into(),
            seq,
            request,
        });
    }
}
pub fn allocations() -> Vec<Allocation> {
    ALLOCATIONS.lock().unwrap().clone()
}

pub fn isolate(selector: &str, cases: &[&str]) -> Option<String> {
    const TEST: &str = "KHIVE_TEST_SUCCESS_ROW_SELECTOR";
    const CASE: &str = "KHIVE_TEST_SUCCESS_ROW_CASE";
    if std::env::var(TEST).ok().as_deref() == Some(selector) {
        return Some(std::env::var(CASE).expect("isolated case"));
    }
    let directory = tempfile::tempdir().unwrap();
    for (index, case) in cases.iter().enumerate() {
        let path = directory.path().join(format!("{index}.log"));
        let output = std::fs::File::create(&path).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", selector, "--nocapture", "--test-threads=1"])
            .env_remove("KHIVE_AUDIT_SUCCESS_ROWS")
            .env(TEST, selector)
            .env(CASE, case)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .expect("isolated audit test starts");
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "isolated audit test exceeded its bound: {}",
                    std::fs::read_to_string(&path).unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = std::fs::read_to_string(&path).unwrap();
        assert!(
            status.success() && output.contains("1 passed"),
            "{selector}/{case}: {output}"
        );
    }
    None
}

#[derive(Default)]
pub struct Store {
    pub rows: Mutex<Vec<Event>>,
    pub submitted: AtomicU64,
    pub fail: AtomicBool,
    pub hold: AtomicBool,
    pub started: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}
impl Store {
    pub fn rows(&self) -> Vec<Event> {
        self.rows.lock().unwrap().clone()
    }
}
#[async_trait]
impl EventStore for Store {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.append_events_idempotent(vec![event]).await.map(|_| ())
    }
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        let count = events.len() as u64;
        self.append_events_idempotent(events).await?;
        Ok(BatchWriteSummary {
            attempted: count,
            affected: count,
            ..BatchWriteSummary::default()
        })
    }
    async fn get_event(&self, id: uuid::Uuid) -> StorageResult<Option<Event>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|event| event.id == id)
            .cloned())
    }
    async fn query_events(
        &self,
        _filter: EventFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        unimplemented!("cut fixture reads its complete committed recording directly")
    }
    async fn count_events(&self, _filter: EventFilter) -> StorageResult<u64> {
        Ok(self.rows.lock().unwrap().len() as u64)
    }
    fn preflight_event(&self, _event: &Event) -> StorageResult<()> {
        Ok(())
    }
    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        self.submitted
            .fetch_add(events.len() as u64, Ordering::Relaxed);
        if self.hold.swap(false, Ordering::SeqCst) {
            self.started.notify_one();
            self.release.notified().await;
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(StorageError::Internal(
                "audit fixture permanent append failure".into(),
            ));
        }
        let mut stored = self.rows.lock().unwrap();
        let mut rows = Vec::new();
        for event in events {
            if let Some(existing) = stored.iter().find(|old| old.id == event.id) {
                rows.push(if *existing == event {
                    EventAppendDisposition::AlreadyPresentIdentical
                } else {
                    EventAppendDisposition::IdentityConflict
                });
            } else {
                stored.push(event);
                rows.push(EventAppendDisposition::Inserted);
            }
        }
        Ok(IdempotentEventBatchResult { rows })
    }
    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
}

#[derive(Default)]
pub struct Control {
    pub calls: AtomicU64,
    pub result: RwLock<Option<Value>>,
    pub error: AtomicBool,
}
impl Control {
    pub async fn respond(&self, verb: &str) -> Result<Value, RuntimeError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        crate::usage::count(crate::usage::UsageUnit::DbRoundTrips, 3);
        if self.error.load(Ordering::SeqCst) {
            return Err(RuntimeError::InvalidInput("fixture domain error".into()));
        }
        Ok(self
            .result
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| clean(verb)))
    }
}
pub fn clean(verb: &str) -> Value {
    match verb {
        HEARTBEAT => json!({"ok":true,"outcome":"success"}),
        CLEANUP => json!({"ok":true,"deleted":0,"routed_owners_detached":0}),
        _ => json!({"ok":true}),
    }
}
pub struct PolicyPack {
    pub owner: &'static str,
    pub handlers: &'static [HandlerDef],
    pub control: Arc<Control>,
}
impl Pack for PolicyPack {
    const NAME: &'static str = "success_audit_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}
#[async_trait]
impl PackRuntime for PolicyPack {
    fn name(&self) -> &str {
        self.owner
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        self.handlers
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.control.respond(verb).await
    }
}
pub struct Rig {
    pub registry: VerbRegistry,
    pub store: Arc<Store>,
    pub control: Arc<Control>,
}
pub fn builder(
    mode: AuditSuccessRowsMode,
    trusted: bool,
    control: Arc<Control>,
) -> VerbRegistryBuilder {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_audit_success_rows_mode(mode);
    for (owner, handlers) in [("comm", COMM), ("kg", KG)] {
        let pack = PolicyPack {
            owner,
            handlers,
            control: control.clone(),
        };
        if trusted {
            builder.register_trusted(pack);
        } else {
            builder.register(pack);
        }
    }
    builder
}
pub fn rig(mode: AuditSuccessRowsMode, trusted: bool, with_store: bool) -> Rig {
    let store = Arc::new(Store::default());
    let control = Arc::new(Control::default());
    let mut builder = builder(mode, trusted, control.clone());
    if with_store {
        builder.with_event_store(store.clone());
    }
    Rig {
        registry: builder.build().unwrap(),
        store,
        control,
    }
}
pub async fn call(
    rig: &Rig,
    verb: &str,
    request: u64,
    intercepted: bool,
) -> Result<Value, DispatchError> {
    call_in(rig, verb, request, intercepted, "local").await
}
pub async fn call_in(
    rig: &Rig,
    verb: &str,
    request: u64,
    intercepted: bool,
    namespace: &str,
) -> Result<Value, DispatchError> {
    let identity = RequestIdentity {
        namespace: namespace.into(),
        actor_id: Some("audit-reader".into()),
        request_id: Some(request),
        ..RequestIdentity::default()
    };
    REQUEST
        .scope(request, async {
            if intercepted {
                rig.registry
                    .dispatch_intercepted_with_metadata_and_disposition(
                        verb,
                        &json!({}),
                        Some(&identity),
                        |_| async {
                            rig.control
                                .respond(verb)
                                .await
                                .map(|value| crate::pack::InterceptedDispatchResult::new(value, ()))
                        },
                    )
                    .await
                    .map(|outcome| outcome.result)
            } else {
                rig.registry
                    .dispatch_with_disposition(verb, json!({}), Some(identity))
                    .await
            }
        })
        .await
}
pub type VerbCounts = Vec<(&'static str, u64)>;
pub fn counts(registry: &VerbRegistry) -> (VerbCounts, VerbCounts, VerbCounts) {
    (
        registry.audit_success_rows_omitted_counts().to_vec(),
        registry.audit_success_rows_sampled_out_counts().to_vec(),
        registry.audit_success_rows_forced_counts().to_vec(),
    )
}
pub fn count(values: &[(&str, u64)], verb: &str) -> u64 {
    values.iter().find(|(name, _)| *name == verb).unwrap().1
}
pub fn sampling(row: &Event) -> &Value {
    &row.payload["resource"]["sampling"]
}

pub fn env_rig() -> Rig {
    let store = Arc::new(Store::default());
    let control = Arc::new(Control::default());
    let mut builder = VerbRegistryBuilder::new();
    for (owner, handlers) in [("comm", COMM), ("kg", KG)] {
        builder.register_trusted(PolicyPack {
            owner,
            handlers,
            control: control.clone(),
        });
    }
    builder.with_event_store(store.clone());
    Rig {
        registry: builder.build().unwrap(),
        store,
        control,
    }
}
