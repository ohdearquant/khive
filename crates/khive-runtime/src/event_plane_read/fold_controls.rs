use super::grouping::{EventCountGroupBy, EventCountsAccumulator};
use super::test_support::cross_sum;
use khive_storage::{Event, EventFilter};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::counts::{
    collect_events_cursor_walk, fold_event_counts_window_exhaustive, visit_events_cursor_walk,
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
            1 => json!({"signal": "wrong",
                "resource": {"work_class": "nested", "cost_unit": 10}}),
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
    async fn count_events_grouped(
        &self,
        filter: EventFilter,
        group_by: khive_storage::event::EventGroupBy,
    ) -> StorageResult<std::collections::BTreeMap<String, u64>> {
        assert!(matches!(group_by, khive_storage::event::EventGroupBy::Kind));
        let total = self.count_events(filter).await?;
        Ok(std::collections::BTreeMap::from([(
            EventKind::FeedbackExplicit.name().to_string(),
            total,
        )]))
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
