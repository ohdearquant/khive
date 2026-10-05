use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Event, EventFilter};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};

use crate::event_counts_grouping::EventCountGroupBy;
use crate::handlers::{fetch_event_counts_window, fetch_event_counts_window_exhaustive};
use crate::BrainPack;

const CALLER: &str = "lambda:reader";
const SINCE: i64 = 1_000_000;
const UNTIL: i64 = 2_000_000;

fn fixture(fleet_reader: bool) -> (KhiveRuntime, VerbRegistry) {
    let mut config = RuntimeConfig {
        db_path: None,
        brain_profile: None,
        actor_id: Some(CALLER.into()),
        visible_namespaces: vec![Namespace::parse("lambda:visible").unwrap()],
        ..RuntimeConfig::no_embeddings()
    };
    if fleet_reader {
        config.brain.fleet_readers = vec![CALLER.into()];
    }
    let runtime = KhiveRuntime::new(config).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(CALLER.into()));
    builder.with_visible_namespaces(runtime.config().visible_namespaces.clone());
    builder.register(KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime.clone()));
    (runtime, builder.build().unwrap())
}

fn window() -> Value {
    json!({"since": khive_runtime::micros_to_iso(SINCE), "until": khive_runtime::micros_to_iso(UNTIL)})
}

async fn seed(
    runtime: &KhiveRuntime,
    namespace: &str,
    verb: &str,
    actor: &str,
    kind: EventKind,
    time: i64,
) {
    let mut event = Event::new(namespace, verb, kind, SubstrateKind::Note, actor);
    event.created_at = time;
    if kind == EventKind::SearchExecuted {
        event.payload = json!({"result_kind": "note"});
    }
    // Trusted fixture insertion preserves historical actor labels exactly.
    runtime
        .backend()
        .events_for_namespace(namespace)
        .unwrap()
        .append_event(event)
        .await
        .unwrap();
}

fn cross_sum(value: &Value) -> u64 {
    value
        .as_object()
        .unwrap()
        .values()
        .flat_map(|actors| actors.as_object().unwrap().values())
        .map(|count| count.as_u64().unwrap())
        .sum()
}

#[tokio::test]
async fn requested_cross_keeps_joint_identity_when_marginals_are_equal() {
    let (runtime, registry) = fixture(true);
    for (verb, actor, count) in [
        ("gtd.tasks", "actor:lambda:a", 3),
        ("gtd.tasks", "agent:worker:z", 1),
        ("memory.recall", "actor:lambda:a", 1),
        ("memory.recall", "agent:worker:z", 3),
    ] {
        for offset in 0..count {
            seed(
                &runtime,
                "local",
                verb,
                actor,
                EventKind::Audit,
                SINCE + offset,
            )
            .await;
        }
    }
    seed(
        &runtime,
        "local",
        "outside.before",
        "actor:lambda:a",
        EventKind::Audit,
        SINCE - 1,
    )
    .await;
    seed(
        &runtime,
        "local",
        "outside.until",
        "actor:lambda:a",
        EventKind::Audit,
        UNTIL,
    )
    .await;
    seed(
        &runtime,
        "local",
        "other.kind",
        "actor:lambda:a",
        EventKind::SearchExecuted,
        SINCE,
    )
    .await;
    seed(
        &runtime,
        "elsewhere",
        "other.namespace",
        "actor:lambda:a",
        EventKind::Audit,
        SINCE,
    )
    .await;
    let mut args = window();
    args["all_actors"] = json!(true);
    args["kind"] = json!("audit");
    let ungrouped = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert!(ungrouped.get("counts_by_verb_and_actor").is_none());
    assert!(ungrouped
        .get("counts_by_verb_and_actor_page_scoped")
        .is_none());
    args["group_by"] = json!(["verb", "actor"]);
    let mut grouped = registry.dispatch("brain.event_counts", args).await.unwrap();
    assert_eq!(
        grouped["counts_by_verb"],
        json!({"gtd.tasks": 4, "memory.recall": 4})
    );
    assert_eq!(
        grouped["counts_by_actor"],
        json!({"actor:lambda:a": 4, "agent:worker:z": 4})
    );
    assert_eq!(
        grouped["counts_by_verb_and_actor"],
        json!({
            "gtd.tasks": {"actor:lambda:a": 3, "agent:worker:z": 1},
            "memory.recall": {"actor:lambda:a": 1, "agent:worker:z": 3},
        })
    );
    assert_eq!(cross_sum(&grouped["counts_by_verb_and_actor"]), 8);
    assert_eq!(grouped["window_event_total"], 8);
    grouped
        .as_object_mut()
        .unwrap()
        .remove("counts_by_verb_and_actor");
    assert_eq!(
        grouped, ungrouped,
        "grouping must not change any existing response field"
    );
}

#[tokio::test]
async fn cross_inherits_default_alias_coalescing_and_explicit_actor_scope() {
    let (runtime, registry) = fixture(false);
    for actor in [
        CALLER,
        "actor:lambda:reader",
        "actor:lambda:visible",
        "actor:lambda:hidden",
    ] {
        seed(
            &runtime,
            "local",
            "pack.verb",
            actor,
            EventKind::Audit,
            SINCE,
        )
        .await;
    }
    let mut args = window();
    args["group_by"] = json!(["verb", "actor"]);
    let own = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert_eq!(
        own["counts_by_verb_and_actor"],
        json!({"pack.verb": {CALLER: 2}})
    );
    assert_eq!(own["counts_by_actor"], json!({CALLER: 2}));
    args["actor"] = json!(CALLER);
    let explicit = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert_eq!(
        explicit["counts_by_verb_and_actor"],
        json!({"pack.verb": {CALLER: 1, "actor:lambda:reader": 1}})
    );
    args["actor"] = json!("lambda:visible");
    let visible = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert_eq!(
        visible["counts_by_verb_and_actor"],
        json!({"pack.verb": {"actor:lambda:visible": 1}})
    );
    args["actor"] = json!("lambda:hidden");
    let hidden = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap_err();
    assert!(matches!(hidden, RuntimeError::InvalidInput(_)));
    args.as_object_mut().unwrap().remove("actor");
    args["all_actors"] = json!(true);
    let all = registry
        .dispatch("brain.event_counts", args)
        .await
        .unwrap_err();
    assert!(all.to_string().contains("not a configured fleet reader"));
}

#[tokio::test]
async fn group_by_is_optional_and_accepts_only_the_ordered_pair() {
    let (_, registry) = fixture(true);
    let base = registry
        .dispatch("brain.event_counts", window())
        .await
        .unwrap();
    let mut args = window();
    args["group_by"] = Value::Null;
    assert_eq!(
        registry
            .dispatch("brain.event_counts", args.clone())
            .await
            .unwrap(),
        base
    );
    args["group_by"] = json!(["verb", "actor"]);
    let empty = registry
        .dispatch("brain.event_counts", args.clone())
        .await
        .unwrap();
    assert_eq!(empty["counts_by_verb_and_actor"], json!({}));
    for invalid in [
        json!([]),
        json!(["verb"]),
        json!(["verb", "actor", "kind"]),
        json!(["actor", "verb"]),
        json!(["verb", "verb"]),
        json!(["actor", "actor"]),
        json!(["verb", "kind"]),
        json!(["actor", "kind"]),
        json!(["Verb", "actor"]),
        json!("verb,actor"),
        json!({"verb": "actor"}),
        json!(["verb", null]),
        json!([1, "actor"]),
    ] {
        args["group_by"] = invalid.clone();
        let error = registry
            .dispatch("brain.event_counts", args.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "{invalid}: {error:?}"
        );
    }
    args["group_by"] = json!(["verb", "actor"]);
    args["actor"] = json!(CALLER);
    args["all_actors"] = json!(true);
    assert!(registry
        .dispatch("brain.event_counts", args)
        .await
        .unwrap_err()
        .to_string()
        .contains("cannot be combined"));
}

#[tokio::test]
async fn cross_uses_the_sampled_and_exhaustive_event_windows_including_audit_segregation() {
    let (runtime, _) = fixture(true);
    for offset in 0..5 {
        seed(
            &runtime,
            "local",
            "audit.verb",
            &format!("actor:sample:{offset}"),
            EventKind::Audit,
            SINCE + 10 + offset,
        )
        .await;
    }
    for offset in 0..2 {
        seed(
            &runtime,
            "local",
            "quiet.search",
            "actor:quiet",
            EventKind::SearchExecuted,
            SINCE + offset,
        )
        .await;
    }
    let store = runtime.backend().events_for_namespace("local").unwrap();
    let filter = EventFilter {
        after: Some(SINCE - 1),
        before: Some(UNTIL),
        ..Default::default()
    };
    let (sample, total, truncated) = fetch_event_counts_window(store.as_ref(), &filter, true, 2)
        .await
        .unwrap();
    assert_eq!(total, 7);
    assert!(truncated);
    let mut result = json!({});
    EventCountGroupBy::VerbActor.add_to_result(&mut result, &sample, None, truncated);
    assert!(result.get("counts_by_verb_and_actor").is_none());
    assert_eq!(
        result["counts_by_verb_and_actor_page_scoped"],
        json!({
            "audit.verb": {"actor:sample:4": 1, "actor:sample:3": 1},
            "quiet.search": {"actor:quiet": 2},
        })
    );
    assert_eq!(
        cross_sum(&result["counts_by_verb_and_actor_page_scoped"]),
        sample.len() as u64
    );
    let cells: usize = result["counts_by_verb_and_actor_page_scoped"]
        .as_object()
        .unwrap()
        .values()
        .map(|actors| actors.as_object().unwrap().len())
        .sum();
    assert!(
        cells <= sample.len(),
        "a cross cannot invent unobserved cells"
    );

    let audit_filter = EventFilter {
        kinds: vec![EventKind::Audit],
        ..filter.clone()
    };
    let (audit, total, truncated) =
        fetch_event_counts_window(store.as_ref(), &audit_filter, false, 2)
            .await
            .unwrap();
    assert_eq!(total, 5);
    assert!(truncated);
    let mut audit_result = json!({});
    EventCountGroupBy::VerbActor.add_to_result(&mut audit_result, &audit, None, truncated);
    assert!(audit_result["counts_by_verb_and_actor_page_scoped"]
        .get("quiet.search")
        .is_none());
    assert_eq!(
        cross_sum(&audit_result["counts_by_verb_and_actor_page_scoped"]),
        2
    );

    let (all, total, truncated) =
        fetch_event_counts_window_exhaustive(store.as_ref(), &filter, 2, 20)
            .await
            .unwrap();
    assert_eq!(total, 7);
    assert!(!truncated);
    let mut complete = json!({});
    EventCountGroupBy::VerbActor.add_to_result(&mut complete, &all, None, truncated);
    assert!(complete
        .get("counts_by_verb_and_actor_page_scoped")
        .is_none());
    assert_eq!(cross_sum(&complete["counts_by_verb_and_actor"]), 7);
    assert_eq!(
        complete["counts_by_verb_and_actor"]["quiet.search"]["actor:quiet"],
        2
    );
}

#[tokio::test]
async fn more_cross_cells_than_marginal_keys_do_not_create_another_cap() {
    let (runtime, registry) = fixture(true);
    for verb in ["v.one", "v.two"] {
        for actor in ["actor:a", "actor:b"] {
            seed(&runtime, "local", verb, actor, EventKind::Audit, SINCE).await;
        }
    }
    let store = runtime.backend().events_for_namespace("local").unwrap();
    let (items, total, truncated) = fetch_event_counts_window(
        store.as_ref(),
        &EventFilter {
            kinds: vec![EventKind::Audit],
            ..Default::default()
        },
        false,
        4,
    )
    .await
    .unwrap();
    assert_eq!(total, 4);
    assert_eq!(items.len(), 4);
    assert!(!truncated);
    let mut args = window();
    args["all_actors"] = json!(true);
    args["kind"] = json!("audit");
    args["group_by"] = json!(["verb", "actor"]);
    let result = registry.dispatch("brain.event_counts", args).await.unwrap();
    assert_eq!(result["counts_by_verb"].as_object().unwrap().len(), 2);
    assert_eq!(result["counts_by_actor"].as_object().unwrap().len(), 2);
    assert_eq!(result["truncated"], false);
    assert_eq!(
        result["counts_by_verb_and_actor"],
        json!({"v.one": {"actor:a": 1, "actor:b": 1}, "v.two": {"actor:a": 1, "actor:b": 1}})
    );
}

// The opt-in allocator lives only in this unit-test binary. The mock owns
// scalar row descriptors, so payload liveness cannot be hidden in the fixture.
pub(crate) mod fold_controls {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use crate::handlers::{
        collect_events_cursor_walk, fold_event_counts_window_exhaustive, visit_events_cursor_walk,
        EventCountsAccumulator,
    };
    use khive_storage::event::EventStore;
    use khive_storage::{BatchWriteSummary, Page, PageRequest, StorageResult};

    const PAYLOAD_BYTES: usize = 262_147;

    #[derive(Clone, Copy, Default)]
    struct Probe {
        enabled: bool,
        live: usize,
        peak: usize,
        allocated: usize,
        underflow: bool,
    }

    thread_local! {
        static PROBE: Cell<Probe> = const { Cell::new(Probe {
            enabled: false, live: 0, peak: 0, allocated: 0, underflow: false,
        }) };
    }

    fn allocation_changed(old_size: usize, new_size: usize) {
        // Const TLS and POD Cell updates do not allocate. Never panic from an
        // allocator hook, including when TLS is unavailable during teardown.
        let _ = PROBE.try_with(|cell| {
            let mut probe = cell.get();
            if probe.enabled {
                if old_size == PAYLOAD_BYTES {
                    if probe.live == 0 {
                        probe.underflow = true;
                    } else {
                        probe.live -= 1;
                    }
                }
                if new_size == PAYLOAD_BYTES {
                    probe.live = probe.live.saturating_add(1);
                    probe.allocated = probe.allocated.saturating_add(1);
                    probe.peak = probe.peak.max(probe.live);
                }
                cell.set(probe);
            }
        });
    }

    struct PayloadAllocator;

    // SAFETY: System receives each original pointer/layout unchanged. The
    // observer never dereferences caller memory, allocates, or unwinds.
    unsafe impl GlobalAlloc for PayloadAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller supplies GlobalAlloc's layout contract.
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                allocation_changed(0, layout.size());
            }
            pointer
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller supplies GlobalAlloc's layout contract.
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                allocation_changed(0, layout.size());
            }
            pointer
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            // SAFETY: preserve the caller's pointer/layout/new-size contract.
            let resized = unsafe { System.realloc(pointer, layout, size) };
            if !resized.is_null() {
                allocation_changed(layout.size(), size);
            }
            resized
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: preserve the caller's pointer/layout contract.
            unsafe { System.dealloc(pointer, layout) };
            allocation_changed(layout.size(), 0);
        }
    }

    #[global_allocator]
    static ALLOCATOR: PayloadAllocator = PayloadAllocator;

    struct ProbeScope;

    impl ProbeScope {
        fn start() -> Self {
            PROBE.with(|cell| {
                assert!(!cell.get().enabled, "overlapping payload probes");
                cell.set(Probe {
                    enabled: true,
                    ..Default::default()
                });
            });
            Self
        }
    }

    impl Drop for ProbeScope {
        fn drop(&mut self) {
            let _ = PROBE.try_with(|cell| {
                let mut probe = cell.get();
                probe.enabled = false;
                cell.set(probe);
            });
        }
    }

    fn probe() -> Probe {
        PROBE.with(Cell::get)
    }

    fn padding() -> String {
        // Move the Vec's exact allocation through String and Value; do not
        // clone it while constructing the fixture payload.
        let bytes = vec![b'x'; PAYLOAD_BYTES];
        assert_eq!(bytes.capacity(), PAYLOAD_BYTES);
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.capacity(), PAYLOAD_BYTES);
        text
    }

    pub(crate) struct GeneratedEvents {
        times: Vec<i64>,
        reported_total: u64,
        padded: bool,
        queries: Mutex<Vec<(u32, usize)>>,
        pub(crate) counts: AtomicUsize,
    }

    impl GeneratedEvents {
        pub(crate) fn new(times: &[i64], reported_total: u64, padded: bool) -> Self {
            assert!(times.windows(2).all(|pair| pair[0] >= pair[1]));
            Self {
                times: times.to_vec(),
                reported_total,
                padded,
                queries: Mutex::new(Vec::new()),
                counts: AtomicUsize::new(0),
            }
        }

        pub(crate) fn queries(&self) -> Vec<(u32, usize)> {
            self.queries.lock().unwrap().clone()
        }

        fn matches(time: i64, filter: &EventFilter) -> bool {
            filter.after.is_none_or(|after| time > after)
                && filter.before.is_none_or(|before| time < before)
        }

        fn event(&self, index: usize) -> Event {
            let actor = if index.is_multiple_of(2) {
                "actor:legacy"
            } else {
                "lambda:caller"
            };
            let mut event = Event::new(
                "local",
                "memory.recall",
                EventKind::FeedbackExplicit,
                SubstrateKind::Note,
                actor,
            );
            event.id = uuid::Uuid::from_u128(index as u128 + 1);
            event.created_at = self.times[index];
            event.payload = match index {
                0 => json!({"originating_verb": "search", "served_by_profile_id": "p",
                    "signal": "useful", "work_class": "top",
                    "resource": {"work_class": "ignored", "cost_unit": i64::MAX}}),
                1 => {
                    json!({"signal": "wrong", "resource": {"work_class": "nested", "cost_unit": 10}})
                }
                2 => json!({"resource": {"cost_unit": -20}}),
                _ => json!({"resource": {"cost_unit": 999}}),
            };
            if self.padded {
                event
                    .payload
                    .as_object_mut()
                    .unwrap()
                    .insert("padding".into(), Value::String(padding()));
            }
            event
        }
    }

    #[async_trait::async_trait]
    impl EventStore for GeneratedEvents {
        async fn append_event(&self, _: Event) -> StorageResult<()> {
            panic!("read-only fixture")
        }
        async fn append_events(&self, _: Vec<Event>) -> StorageResult<BatchWriteSummary> {
            panic!("read-only fixture")
        }
        async fn get_event(&self, _: uuid::Uuid) -> StorageResult<Option<Event>> {
            panic!("cursor must query pages")
        }
        async fn query_events(
            &self,
            filter: EventFilter,
            page: PageRequest,
        ) -> StorageResult<Page<Event>> {
            assert_eq!(page.offset, 0);
            let live = probe().live;
            self.queries.lock().unwrap().push((page.limit, live));
            let items = self
                .times
                .iter()
                .enumerate()
                .filter(|(_, time)| Self::matches(**time, &filter))
                .take(page.limit as usize)
                .map(|(index, _)| self.event(index))
                .collect();
            Ok(Page { items, total: None })
        }
        async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
            if self.counts.fetch_add(1, Ordering::Relaxed) == 0 {
                Ok(self.reported_total)
            } else {
                Ok(self
                    .times
                    .iter()
                    .filter(|time| Self::matches(**time, &filter))
                    .count() as u64)
            }
        }
    }

    fn released_before_next_page(queries: &[(u32, usize)]) -> bool {
        queries.len() > 1 && queries.iter().all(|(_, live)| *live == 0)
    }

    #[test]
    fn payload_probe_counts_real_clone_and_drop() {
        let _scope = ProbeScope::start();
        let original = padding();
        assert_eq!(probe().live, 1);
        let cloned = std::hint::black_box(original.clone());
        assert_eq!(probe().live, 2);
        drop(cloned);
        assert_eq!(probe().live, 1);
        drop(original);
        assert_eq!(probe().live, 0);
        assert!(!probe().underflow);
        assert_eq!(probe().allocated, 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhaustive_fold_releases_payloads_before_each_page_including_widened_ties() {
        for (times, total, budget, admitted) in [
            (&[9, 8, 7, 6, 5, 4][..], 6, 20, 6),
            (&[9, 8, 8, 8, 5, 4][..], 6, 20, 6),
            (&[9, 9, 9, 8][..], 2, 3, 3),
        ] {
            let store = GeneratedEvents::new(times, total, true);
            let mut counts = EventCountsAccumulator::new(None, Some(EventCountGroupBy::VerbActor));
            let _scope = ProbeScope::start();
            let result = fold_event_counts_window_exhaustive(
                &store,
                &EventFilter::default(),
                2,
                budget,
                &mut counts,
            )
            .await
            .unwrap();
            assert_eq!(result, (admitted, total, false));
            let queries = store.queries();
            assert!(
                released_before_next_page(&queries),
                "retained payloads: {queries:?}"
            );
            let peak_limit = queries
                .iter()
                .map(|(limit, _)| *limit as usize)
                .max()
                .unwrap();
            assert!(
                probe().allocated >= admitted as usize,
                "probe must observe real payload allocations"
            );
            assert!(
                probe().peak <= peak_limit,
                "only one effective page may be live"
            );
            assert_eq!(probe().live, 0);
            assert!(!probe().underflow);
            if times[1] == times[2] {
                assert!(peak_limit > 2, "fixture must actually widen the tie page");
            }
            let mut result = json!({});
            counts.add_to_result(&mut result, false);
            assert_eq!(result["counts_by_verb"], json!({"memory.recall": admitted}));
            assert_eq!(cross_sum(&result["counts_by_verb_and_actor"]), admitted);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn same_admission_probe_rejects_collecting_and_clone_retention() {
        for clone_retention in [false, true] {
            let store = GeneratedEvents::new(&[9, 8, 7, 6, 5, 4], 6, true);
            let mut counts = EventCountsAccumulator::new(None, None);
            let _scope = ProbeScope::start();
            let retained = if clone_retention {
                let mut retained = Vec::new();
                let admitted =
                    visit_events_cursor_walk(&store, &EventFilter::default(), 2, 20, |event| {
                        counts.observe(&event);
                        retained.push(event.clone());
                    })
                    .await
                    .unwrap();
                assert_eq!(admitted, 6);
                retained
            } else {
                collect_events_cursor_walk(&store, &EventFilter::default(), 2, 20)
                    .await
                    .unwrap()
            };
            std::hint::black_box(&retained);
            assert_eq!(retained.len(), 6);
            let queries = store.queries();
            assert!(queries.len() > 1);
            assert!(
                !released_before_next_page(&queries),
                "retention control must fail the same oracle"
            );
            assert!(queries.iter().skip(1).any(|(_, live)| *live > 0));
            assert!(probe().peak > 2);
            assert_eq!(probe().live, 6);
            if clone_retention {
                let mut result = json!({});
                counts.add_to_result(&mut result, false);
                assert_eq!(
                    result["counts_by_verb"],
                    json!({"memory.recall": 6}),
                    "correct callbacks alone do not prove payload release"
                );
            }
            drop(retained);
            assert_eq!(probe().live, 0);
            assert!(!probe().underflow);
        }
    }
}
