//! Response accounting through real request execution, joined work and SQL appends.
use super::KhiveMcpServer;
use crate::tools::request::RequestParams;
use async_trait::async_trait;
use khive_runtime::pack::{
    AuditSuccessRowsMode, DispatchHook, HandlerDef, PackRuntime, VerbRegistry, VerbRegistryBuilder,
};
use khive_runtime::usage::{self, UsageContext, UsageUnit};
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::{Event, EventFilter, EventView, PageRequest};
use khive_types::{EventKind, Pack, SubstrateKind, VerbCategory, Visibility};
use serde_json::{json, Value};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const OMITTED: [&str; 4] = ["comm.inbox", "comm.probe", "comm.cursor_get", "whoami"];
const SAMPLED: [&str; 2] = ["comm.heartbeat", "comm.cleanup_expired_quarantine"];
const fn handler(name: &'static str) -> HandlerDef {
    HandlerDef {
        name,
        description: "measured success audit fixture",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Declaration,
        params: &[],
    }
}
const COMM: &[HandlerDef] = &[
    handler("comm.inbox"),
    handler("comm.probe"),
    handler("comm.cursor_get"),
    handler("comm.heartbeat"),
    handler("comm.cleanup_expired_quarantine"),
];
const KG: &[HandlerDef] = &[handler("whoami")];
#[derive(Default)]
struct Work {
    context: Mutex<Option<UsageContext>>,
    unmeasured: AtomicBool,
}
struct WorkPack {
    owner: &'static str,
    handlers: &'static [HandlerDef],
    runtime: KhiveRuntime,
    work: Arc<Work>,
}
impl Pack for WorkPack {
    const NAME: &'static str = "measured_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}
#[async_trait]
impl PackRuntime for WorkPack {
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
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let context = usage::current().expect("real request arms usage");
        if self.work.unmeasured.load(Ordering::SeqCst) {
            context.mark_unmeasured();
        }
        *self.work.context.lock().unwrap() = Some(context.clone());
        usage::count(UsageUnit::FtsPasses, 2);
        let store = self.runtime.events(token)?;
        let event = Event::new(
            token.namespace().as_str(),
            "usage.child",
            EventKind::SearchExecuted,
            SubstrateKind::Event,
            "usage-fixture",
        );
        tokio::spawn(usage::scope(context, async move {
            usage::count(UsageUnit::VectorPasses, 3);
            store.append_event(event).await
        }))
        .await
        .map_err(|error| RuntimeError::Internal(error.to_string()))??;
        Ok(match verb {
            "comm.heartbeat" => json!({"outcome":"success"}),
            "comm.cleanup_expired_quarantine" => json!({"deleted":0,"routed_owners_detached":0}),
            _ => json!({"ok":true}),
        })
    }
}
struct LateWork;
#[async_trait]
impl DispatchHook for LateWork {
    async fn on_dispatch(&self, _view: &EventView) {
        usage::count(UsageUnit::GraphHops, 777);
    }
}
fn fixture(mode: AuditSuccessRowsMode) -> (KhiveMcpServer, KhiveRuntime, Arc<Work>) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let work = Arc::new(Work::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_audit_success_rows_mode(mode);
    for (owner, handlers) in [("comm", COMM), ("kg", KG)] {
        builder.register_trusted(WorkPack {
            owner,
            handlers,
            runtime: runtime.clone(),
            work: work.clone(),
        });
    }
    builder.with_event_store(runtime.events(&token).unwrap());
    builder.with_dispatch_hook(Arc::new(LateWork));
    (
        KhiveMcpServer::from_registry(builder.build().unwrap()),
        runtime,
        work,
    )
}
async fn request(server: &KhiveMcpServer, verb: &str, id: u64) -> Value {
    let text = server
        .dispatch_request_local(RequestParams {
            ops: json!([{"tool":verb,"args":{}}]).to_string(),
            request_id: Some(id),
            presentation: Some("verbose".into()),
            format: Some("json".into()),
            ..RequestParams::default()
        })
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&text).unwrap();
    let entry = body["results"][0].clone();
    assert_eq!(entry["ok"], true, "{body}");
    entry
}
async fn rows(runtime: &KhiveRuntime) -> Vec<Event> {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .events(&token)
        .unwrap()
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items
}

#[test]
#[serial_test::serial(config_ledger)]
fn audit_success_rows_response_usage_freezes_without_a_row() {
    const NAME:&str="server::audit_success_rows_usage_tests::audit_success_rows_response_usage_freezes_without_a_row";
    const CHILD: &str = "KHIVE_TEST_SUCCESS_USAGE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env_remove("KHIVE_AUDIT_SUCCESS_ROWS")
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (red, red_runtime, red_work) = fixture(AuditSuccessRowsMode::Reduced);
            let (all, all_runtime, all_work) = fixture(AuditSuccessRowsMode::All);
            for (index, verb) in OMITTED.into_iter().chain(SAMPLED).enumerate() {
                if SAMPLED.contains(&verb) {
                    request(&red, verb, 1000 + index as u64).await;
                }
                let reduced = request(&red, verb, index as u64).await;
                let ordinary = request(&all, verb, index as u64).await;
                assert_eq!(
                    reduced["usage"], ordinary["usage"],
                    "late hook work must not leak only into skipped responses"
                );
                assert_eq!(reduced["usage"]["fts_passes"], 2);
                assert_eq!(reduced["usage"]["vector_passes"], 3);
                assert_eq!(reduced["usage"]["event_rows"], 1);
                assert!(reduced["usage"].get("graph_hops").is_none());
                assert_eq!(
                    red_work
                        .context
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .snapshot()["graph_hops"],
                    777,
                    "late hook actually ran"
                );
                assert_eq!(
                    all_work
                        .context
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .snapshot()["graph_hops"],
                    777
                );
                let all_rows = rows(&all_runtime).await;
                let row = all_rows
                    .iter()
                    .find(|row| {
                        row.kind == EventKind::Audit
                            && row.payload["resource"]["request_id"] == index as u64
                    })
                    .unwrap();
                assert_eq!(row.payload["resource"]["units"], reduced["usage"]);
                assert!(!rows(&red_runtime)
                    .await
                    .iter()
                    .any(|row| row.kind == EventKind::Audit
                        && row.payload["resource"]["request_id"] == index as u64));
            }
            for (server, work) in [(&red, &red_work), (&all, &all_work)] {
                work.unmeasured.store(true, Ordering::SeqCst);
                let entry = request(server, "whoami", 99).await;
                assert!(entry.get("usage").is_none());
            }
        });
}
