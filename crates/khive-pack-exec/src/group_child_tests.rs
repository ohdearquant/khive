use super::*;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Wake, Waker};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Generation {
    Original,
    Foreign,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Observe,
    Signal(Generation, Option<i32>),
    LeaderKill(Generation, Option<i32>),
    Reap(Generation),
    BackendDrop(Generation),
}

#[derive(Clone, Copy)]
enum Observation {
    Exited,
    Pending,
    Error(i32),
}

#[derive(Clone, Copy)]
enum Reap {
    Exited,
    PendingAndReuse,
    ErrorAndReuse(i32),
}

struct Model {
    generation: Generation,
    events: Vec<Event>,
    observations: VecDeque<Observation>,
    default_observation: Observation,
    signals: VecDeque<Option<i32>>,
    leader_kills: VecDeque<Option<i32>>,
    reap: Reap,
}

struct FakeBackend(Rc<RefCell<Model>>);

impl GroupBackend for FakeBackend {
    fn poll_observe_unreaped(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut model = self.0.borrow_mut();
        model.events.push(Event::Observe);
        let observation = model
            .observations
            .pop_front()
            .unwrap_or(model.default_observation);
        match observation {
            Observation::Exited => Poll::Ready(Ok(())),
            Observation::Pending => Poll::Pending,
            Observation::Error(code) => {
                if code == libc::ECHILD {
                    model.generation = Generation::Foreign;
                }
                Poll::Ready(Err(io::Error::from_raw_os_error(code)))
            }
        }
    }

    fn signal_group(&mut self) -> io::Result<()> {
        let mut model = self.0.borrow_mut();
        let error = model.signals.pop_front().unwrap_or(None);
        let generation = model.generation;
        model.events.push(Event::Signal(generation, error));
        error.map_or(Ok(()), |code| Err(io::Error::from_raw_os_error(code)))
    }

    fn start_kill_leader(&mut self) -> io::Result<()> {
        let mut model = self.0.borrow_mut();
        let error = model.leader_kills.pop_front().unwrap_or(None);
        let generation = model.generation;
        model.events.push(Event::LeaderKill(generation, error));
        error.map_or(Ok(()), |code| Err(io::Error::from_raw_os_error(code)))
    }

    fn poll_reap(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<ExitStatus>> {
        let mut model = self.0.borrow_mut();
        let generation = model.generation;
        model.events.push(Event::Reap(generation));
        // Consuming the anchor immediately makes the same numeric ID foreign.
        model.generation = Generation::Foreign;
        match model.reap {
            Reap::Exited => Poll::Ready(Ok(ExitStatus::from_raw(23 << 8))),
            Reap::PendingAndReuse => Poll::Pending,
            Reap::ErrorAndReuse(code) => Poll::Ready(Err(io::Error::from_raw_os_error(code))),
        }
    }
}

impl Drop for FakeBackend {
    fn drop(&mut self) {
        let mut model = self.0.borrow_mut();
        let generation = model.generation;
        model.events.push(Event::BackendDrop(generation));
        // Tokio's Child drop may reap. Test the same ownership boundary.
        model.generation = Generation::Foreign;
    }
}

fn model_group() -> (GroupLifecycle<FakeBackend>, Rc<RefCell<Model>>) {
    let model = Rc::new(RefCell::new(Model {
        generation: Generation::Original,
        events: Vec::new(),
        observations: VecDeque::new(),
        default_observation: Observation::Exited,
        signals: VecDeque::new(),
        leader_kills: VecDeque::new(),
        reap: Reap::Exited,
    }));
    (GroupLifecycle::new(FakeBackend(model.clone())), model)
}

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn ready<T>(future: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    let waker = Waker::from(Arc::new(WakeCount::default()));
    let mut cx = Context::from_waker(&waker);
    match std::pin::pin!(future).poll(&mut cx) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("model operation unexpectedly pending"),
    }
}

fn signal_events(model: &Rc<RefCell<Model>>) -> Vec<Event> {
    model
        .borrow()
        .events
        .iter()
        .filter(|event| matches!(event, Event::Signal(..)))
        .cloned()
        .collect()
}

#[test]
fn group_lifecycle_never_signals_reused_identity_after_normal_or_timeout_reap() {
    for normal_exit in [true, false] {
        let (mut group, model) = model_group();
        if normal_exit {
            ready(group.wait_unreaped()).unwrap();
        } else {
            model.borrow_mut().default_observation = Observation::Pending;
            let waker = Waker::from(Arc::new(WakeCount::default()));
            let mut cx = Context::from_waker(&waker);
            let mut observing = Box::pin(group.wait_unreaped());
            assert!(observing.as_mut().poll(&mut cx).is_pending());
            drop(observing);
        }
        assert_eq!(ready(group.finish()).unwrap().code(), Some(23));
        assert_eq!(model.borrow().generation, Generation::Foreign);
        drop(group);
        assert_eq!(
            signal_events(&model),
            vec![Event::Signal(Generation::Original, None)],
            "a final signal reached a foreign/reused identity: {:?}",
            model.borrow().events
        );
        assert_eq!(
            model.borrow().events,
            vec![
                Event::Observe,
                Event::Signal(Generation::Original, None),
                Event::Reap(Generation::Original),
                Event::BackendDrop(Generation::Foreign),
            ]
        );
    }
}

#[test]
fn group_lifecycle_disarms_before_reap_is_first_polled() {
    let (mut group, model) = model_group();
    model.borrow_mut().reap = Reap::PendingAndReuse;
    let waker = Waker::from(Arc::new(WakeCount::default()));
    let mut cx = Context::from_waker(&waker);
    let mut finishing = Box::pin(group.finish());
    assert!(finishing.as_mut().poll(&mut cx).is_pending());
    assert_eq!(model.borrow().generation, Generation::Foreign);
    drop(finishing);
    drop(group);
    assert_eq!(
        model.borrow().events,
        vec![
            Event::Signal(Generation::Original, None),
            Event::Reap(Generation::Original),
            Event::BackendDrop(Generation::Foreign),
        ],
        "cancellation after the first consuming poll must never send again"
    );
}

#[test]
fn group_lifecycle_drop_cleans_before_backend_drop() {
    for during_observation in [false, true] {
        let (mut group, model) = model_group();
        if during_observation {
            model.borrow_mut().default_observation = Observation::Pending;
            let waker = Waker::from(Arc::new(WakeCount::default()));
            let mut cx = Context::from_waker(&waker);
            let mut waiting = Box::pin(group.wait_unreaped());
            assert!(waiting.as_mut().poll(&mut cx).is_pending());
            drop(waiting);
            model.borrow_mut().events.clear();
        }
        drop(group);
        assert_eq!(
            model.borrow().events,
            vec![
                Event::Signal(Generation::Original, None),
                Event::BackendDrop(Generation::Original),
            ],
            "held drop must signal before backend drop"
        );
        assert_eq!(model.borrow().generation, Generation::Foreign);
    }
}

#[test]
fn group_lifecycle_lost_identity_never_signals_or_reaps_replacement() {
    for previously_observed in [false, true] {
        let (mut group, model) = model_group();
        let mut expected = Vec::new();
        if previously_observed {
            ready(group.wait_unreaped()).unwrap();
            expected.push(Event::Observe);
        }
        model.borrow_mut().default_observation = Observation::Error(libc::ECHILD);
        assert_eq!(
            ready(group.wait_unreaped()).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(model.borrow().generation, Generation::Foreign);
        assert_eq!(
            ready(group.finish())
                .expect_err("lost identity must refuse cleanup")
                .raw_os_error(),
            Some(libc::ECHILD)
        );
        drop(group);
        expected.extend([Event::Observe, Event::BackendDrop(Generation::Foreign)]);
        assert_eq!(
            model.borrow().events,
            expected,
            "known identity loss must not authorize any numeric signal/reap"
        );
    }
}

#[test]
fn group_lifecycle_owned_observation_error_still_cleans_before_reap() {
    let (mut group, model) = model_group();
    model.borrow_mut().default_observation = Observation::Error(libc::EIO);
    assert_eq!(
        ready(group.wait_unreaped()).unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(
        ready(group.finish())
            .expect("owned observation error must retain cleanup authority")
            .code(),
        Some(23)
    );
    drop(group);
    assert_eq!(
        model.borrow().events,
        vec![
            Event::Observe,
            Event::Signal(Generation::Original, None),
            Event::Reap(Generation::Original),
            Event::BackendDrop(Generation::Foreign),
        ]
    );
}

#[test]
fn group_lifecycle_signal_failure_retains_anchor_until_safe_retry() {
    for code in [libc::EPERM, libc::EIO] {
        for retry_finish in [false, true] {
            let (mut group, model) = model_group();
            model.borrow_mut().signals.push_back(Some(code));
            model.borrow_mut().leader_kills.push_back(Some(code));
            assert_eq!(
                ready(group.finish())
                    .expect_err("failed group signal must not report successful finish")
                    .kind(),
                io::Error::from_raw_os_error(code).kind()
            );
            assert_eq!(
                group.cleanup_error.as_ref().unwrap().raw_os_error(),
                Some(code)
            );
            assert_eq!(model.borrow().generation, Generation::Original);
            assert_eq!(
                model.borrow().events,
                vec![
                    Event::Signal(Generation::Original, Some(code)),
                    Event::LeaderKill(Generation::Original, Some(code)),
                ],
                "signal failure must return before consuming the anchor"
            );
            let mut expected = model.borrow().events.clone();
            if retry_finish {
                assert_eq!(
                    ready(group.finish()).unwrap_err().raw_os_error(),
                    Some(code),
                    "successful retry must not erase the original cleanup failure"
                );
                assert_eq!(group.exit_status.and_then(|status| status.code()), Some(23));
                expected.extend([
                    Event::Signal(Generation::Original, None),
                    Event::Reap(Generation::Original),
                    Event::BackendDrop(Generation::Foreign),
                ]);
            } else {
                expected.extend([
                    Event::Signal(Generation::Original, None),
                    Event::BackendDrop(Generation::Original),
                ]);
            }
            drop(group);
            assert_eq!(model.borrow().events, expected);
        }
    }
}

#[test]
fn group_lifecycle_observed_eperm_reaps_and_keeps_status() {
    let (mut group, model) = model_group();
    ready(group.wait_unreaped()).unwrap();
    model.borrow_mut().signals.push_back(Some(libc::EPERM));
    assert_eq!(
        ready(group.finish())
            .expect("observed-exit EPERM must not turn normal exit into cleanup failure")
            .code(),
        Some(23)
    );
    assert_eq!(group.exit_status.and_then(|status| status.code()), Some(23));
    assert!(group.cleanup_error.is_none());
    let before = model.borrow().events.clone();
    assert_eq!(ready(group.finish()).unwrap().code(), Some(23));
    assert_eq!(
        model.borrow().events,
        before,
        "completed finish must not poll again"
    );
    drop(group);
    assert_eq!(
        model.borrow().events,
        vec![
            Event::Observe,
            Event::Signal(Generation::Original, Some(libc::EPERM)),
            Event::Reap(Generation::Original),
            Event::BackendDrop(Generation::Foreign),
        ]
    );
}

#[test]
fn group_lifecycle_observed_cleanup_error_keeps_status_and_error() {
    let (mut group, model) = model_group();
    ready(group.wait_unreaped()).unwrap();
    model.borrow_mut().signals.push_back(Some(libc::EIO));
    assert_eq!(
        ready(group.finish()).unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(
        group.exit_status.and_then(|status| status.code()),
        Some(23),
        "observed leader must be reaped despite a cleanup error"
    );
    let before = model.borrow().events.clone();
    assert_eq!(
        ready(group.finish()).unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(
        model.borrow().events,
        before,
        "cached error/status must not trigger cleanup again"
    );
    drop(group);
    assert_eq!(
        model.borrow().events,
        vec![
            Event::Observe,
            Event::Signal(Generation::Original, Some(libc::EIO)),
            Event::Reap(Generation::Original),
            Event::BackendDrop(Generation::Foreign),
        ]
    );
}

#[test]
fn group_lifecycle_unobserved_eperm_falls_back_without_hiding_error() {
    let (mut group, model) = model_group();
    model.borrow_mut().default_observation = Observation::Pending;
    model.borrow_mut().signals.push_back(Some(libc::EPERM));
    assert_eq!(
        ready(group.finish()).unwrap_err().raw_os_error(),
        Some(libc::EPERM),
        "unobserved EPERM must remain an error after safe leader cleanup"
    );
    assert_eq!(group.exit_status.and_then(|status| status.code()), Some(23));
    let before = model.borrow().events.clone();
    assert_eq!(
        ready(group.finish()).unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
    assert_eq!(
        model.borrow().events,
        before,
        "repeated finish must retain error and status"
    );
    drop(group);
    assert_eq!(
        model.borrow().events,
        vec![
            Event::Signal(Generation::Original, Some(libc::EPERM)),
            Event::LeaderKill(Generation::Original, None),
            Event::Reap(Generation::Original),
            Event::BackendDrop(Generation::Foreign),
        ],
        "fallback must signal the owned leader before its identity is consumed"
    );
}

#[test]
fn group_lifecycle_failed_observation_does_not_make_eperm_benign() {
    let (mut group, model) = model_group();
    model.borrow_mut().default_observation = Observation::Error(libc::EIO);
    assert_eq!(
        ready(group.wait_unreaped()).unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    model.borrow_mut().signals.push_back(Some(libc::EPERM));
    assert_eq!(
        ready(group.finish()).unwrap_err().raw_os_error(),
        Some(libc::EPERM),
        "failed observation must not authorize benign EPERM"
    );
    assert_eq!(group.exit_status.and_then(|status| status.code()), Some(23));
    drop(group);
    assert_eq!(
        model.borrow().events,
        vec![
            Event::Observe,
            Event::Signal(Generation::Original, Some(libc::EPERM)),
            Event::LeaderKill(Generation::Original, None),
            Event::Reap(Generation::Original),
            Event::BackendDrop(Generation::Foreign),
        ]
    );
}

#[test]
fn group_lifecycle_both_signals_denied_returns_without_reap() {
    let (mut group, model) = model_group();
    model.borrow_mut().default_observation = Observation::Pending;
    model.borrow_mut().signals.extend([Some(libc::EPERM); 2]);
    model.borrow_mut().leader_kills.push_back(Some(libc::EPERM));
    let error = ready(group.finish()).expect_err("dual denial must return without waiting");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(
        group.exit_status.is_none(),
        "dual denial must not invent a reaped status"
    );
    assert_eq!(model.borrow().generation, Generation::Original);
    assert_eq!(
        model.borrow().events,
        vec![
            Event::Signal(Generation::Original, Some(libc::EPERM)),
            Event::LeaderKill(Generation::Original, Some(libc::EPERM)),
        ],
        "dual denial must never poll reap on a possibly live leader"
    );
    assert!(error.to_string().contains("direct child signal failed"));
    drop(group);
    assert_eq!(
        model.borrow().events,
        vec![
            Event::Signal(Generation::Original, Some(libc::EPERM)),
            Event::LeaderKill(Generation::Original, Some(libc::EPERM)),
            Event::Signal(Generation::Original, Some(libc::EPERM)),
            Event::LeaderKill(Generation::Original, None),
            Event::BackendDrop(Generation::Original),
        ],
        "Drop may retry only while the original identity remains held"
    );
}

#[test]
fn group_lifecycle_fallback_reap_cancellation_never_signals_reused_identity() {
    for reap in [Reap::PendingAndReuse, Reap::ErrorAndReuse(libc::EIO)] {
        let (mut group, model) = model_group();
        model.borrow_mut().signals.push_back(Some(libc::EPERM));
        model.borrow_mut().reap = reap;
        let waker = Waker::from(Arc::new(WakeCount::default()));
        let mut cx = Context::from_waker(&waker);
        let mut finishing = Box::pin(group.finish());
        match (reap, finishing.as_mut().poll(&mut cx)) {
            (Reap::PendingAndReuse, Poll::Pending) => {}
            (Reap::ErrorAndReuse(_), Poll::Ready(Err(error))) => {
                assert!(error.to_string().contains("group signal failed"));
                assert!(error.to_string().contains("child reap failed"));
            }
            _ => panic!("fallback reap did not preserve the scripted pending/error outcome"),
        }
        drop(finishing);
        assert_eq!(
            group.cleanup_error.as_ref().unwrap().raw_os_error(),
            Some(libc::EPERM),
            "cancellation must retain the original cleanup error"
        );
        assert!(group.exit_status.is_none());
        assert_eq!(model.borrow().generation, Generation::Foreign);
        if matches!(reap, Reap::ErrorAndReuse(_)) {
            let before = model.borrow().events.clone();
            let error = ready(group.finish()).unwrap_err();
            assert!(error.to_string().contains("group signal failed"));
            assert!(error.to_string().contains("child reap failed"));
            assert_eq!(model.borrow().events, before, "reap failure must be cached");
        }
        drop(group);
        assert_eq!(
            model.borrow().events,
            vec![
                Event::Signal(Generation::Original, Some(libc::EPERM)),
                Event::LeaderKill(Generation::Original, None),
                Event::Reap(Generation::Original),
                Event::BackendDrop(Generation::Foreign),
            ],
            "fallback cancellation must not signal a foreign group or leader"
        );
    }
}

#[test]
fn group_lifecycle_esrch_disarms_and_reaps_once() {
    let (mut group, model) = model_group();
    model.borrow_mut().signals.push_back(Some(libc::ESRCH));
    assert_eq!(
        ready(group.finish())
            .expect("absent-group cleanup must reap safely")
            .code(),
        Some(23)
    );
    let before = model.borrow().events.clone();
    assert_eq!(
        ready(group.finish())
            .expect("absent-group cleanup must reap safely")
            .code(),
        Some(23)
    );
    assert_eq!(
        model.borrow().events,
        before,
        "finished child must be cached"
    );
    drop(group);
    assert_eq!(
        model.borrow().events,
        vec![
            Event::Signal(Generation::Original, Some(libc::ESRCH)),
            Event::Reap(Generation::Original),
            Event::BackendDrop(Generation::Foreign),
        ],
        "absent group must remain disarmed after reap"
    );
}

#[test]
fn group_lifecycle_reap_error_never_rearms_numeric_signals() {
    for code in [libc::EIO, libc::ECHILD] {
        let (mut group, model) = model_group();
        model.borrow_mut().reap = Reap::ErrorAndReuse(code);
        assert_eq!(
            ready(group.finish()).unwrap_err().raw_os_error(),
            Some(code)
        );
        let before = model.borrow().events.clone();
        assert_eq!(
            ready(group.finish()).unwrap_err().raw_os_error(),
            Some(code)
        );
        assert_eq!(
            model.borrow().events,
            before,
            "repeated finish must not reap a replacement"
        );
        drop(group);
        assert_eq!(
            model.borrow().events,
            vec![
                Event::Signal(Generation::Original, None),
                Event::Reap(Generation::Original),
                Event::BackendDrop(Generation::Foreign),
            ],
            "reap error must never rearm group signaling"
        );
    }
}

#[test]
fn group_lifecycle_interrupted_observation_yields_and_keeps_ownership() {
    let (mut group, model) = model_group();
    model.borrow_mut().observations = VecDeque::from(vec![
        Observation::Error(libc::EINTR);
        OBSERVE_POLL_BUDGET + 1
    ]);
    let counter = Arc::new(WakeCount::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let mut observing = Box::pin(group.wait_unreaped());
    assert!(observing.as_mut().poll(&mut cx).is_pending());
    assert_eq!(model.borrow().events.len(), OBSERVE_POLL_BUDGET);
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert!(matches!(
        observing.as_mut().poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    drop(observing);
    assert_eq!(model.borrow().generation, Generation::Original);
    assert!(signal_events(&model).is_empty());
    assert_eq!(ready(group.finish()).unwrap().code(), Some(23));
}

#[test]
fn exit_observer_registers_again_after_coalesced_unrelated_notification() {
    #[derive(Default)]
    struct Race {
        calls: Vec<&'static str>,
        signal_polls: usize,
        probes: usize,
        registered: bool,
        target_exited: bool,
        wake_delivered: bool,
    }
    let race = Rc::new(RefCell::new(Race::default()));
    let counter = Arc::new(WakeCount::default());
    let waker = Waker::from(counter);
    let mut cx = Context::from_waker(&waker);
    let signal_race = race.clone();
    let probe_race = race.clone();
    let result = poll_unreaped(
        &mut cx,
        move |_cx| {
            let mut race = signal_race.borrow_mut();
            race.calls.push("signal");
            race.signal_polls += 1;
            if race.signal_polls == 1 {
                Poll::Ready(Some(()))
            } else {
                race.registered = true;
                Poll::Pending
            }
        },
        move || {
            let mut race = probe_race.borrow_mut();
            race.calls.push("probe");
            race.probes += 1;
            if race.probes == 1 {
                return Ok(false);
            }
            let observed = race.target_exited;
            if !race.target_exited {
                // Exit just after a negative probe; only renewed interest can wake us.
                race.target_exited = true;
                race.wake_delivered = race.registered;
            }
            Ok(observed)
        },
    );
    assert!(result.is_pending(), "unrelated signal is not target exit");
    assert_eq!(
        race.borrow().calls,
        ["signal", "probe", "signal", "probe"],
        "observer must register signal interest before each probe"
    );
    assert!(
        race.borrow().wake_delivered,
        "target exit lost its only wakeup"
    );
    assert!(matches!(
        poll_unreaped(
            &mut cx,
            |_| Poll::Pending,
            || Ok(race.borrow().target_exited)
        ),
        Poll::Ready(Ok(()))
    ));
}

fn assert_exit_observer_burst_budget(interrupted: bool) {
    let signals = Rc::new(RefCell::new(0usize));
    let probes = Rc::new(RefCell::new(0usize));
    let counter = Arc::new(WakeCount::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let signal_count = signals.clone();
    let probe_count = probes.clone();
    let result = poll_unreaped(
        &mut cx,
        move |_| {
            *signal_count.borrow_mut() += 1;
            Poll::Ready(Some(()))
        },
        move || {
            let mut count = probe_count.borrow_mut();
            *count += 1;
            // A finite stream makes a missing budget an assertion failure, not a hang.
            if *count > OBSERVE_POLL_BUDGET {
                Ok(true)
            } else if interrupted {
                Err(io::Error::from_raw_os_error(libc::EINTR))
            } else {
                Ok(false)
            }
        },
    );
    assert!(
        result.is_pending(),
        "one poll consumed more than its work budget"
    );
    assert_eq!(*signals.borrow(), OBSERVE_POLL_BUDGET);
    assert_eq!(*probes.borrow(), OBSERVE_POLL_BUDGET);
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
}

#[test]
fn exit_observer_bounds_interrupted_work() {
    assert_exit_observer_burst_budget(true);
}

#[test]
fn exit_observer_bounds_ready_notification_work() {
    assert_exit_observer_burst_budget(false);
}

#[test]
fn exit_observer_ignores_unrelated_signals_and_reports_closed_stream() {
    let waker = Waker::from(Arc::new(WakeCount::default()));
    let mut cx = Context::from_waker(&waker);
    let mut signals = VecDeque::from([Poll::Ready(Some(())), Poll::Pending]);
    let mut probes = 0;
    assert!(poll_unreaped(
        &mut cx,
        |_| signals.pop_front().unwrap_or(Poll::Pending),
        || {
            probes += 1;
            Ok(false)
        }
    )
    .is_pending());
    assert_eq!(probes, 2);
    assert!(matches!(
        poll_unreaped(&mut cx, |_| Poll::Ready(None), || Ok(false)),
        Poll::Ready(Err(_))
    ));
}

fn isolated_command(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command
}

async fn original_group_fixture() -> (
    GroupChild,
    BufReader<ChildStdout>,
    tokio::process::ChildStdin,
) {
    let mut command = isolated_command(
        r#"(
            /bin/sh -c 'printf "grandchild %s\n" "$$"; exec /bin/sleep 10' &
            wait
        ) &
        IFS= read -r release
        if [ "$release" = exit ]; then exit 23; fi
        /bin/sleep 10
        "#,
    );
    let mut group = GroupChild::spawn(&mut command).expect("spawn original-group fixture");
    let stdin = group
        .lifecycle
        .backend
        .child
        .stdin
        .take()
        .expect("fixture stdin");
    let mut stdout = BufReader::new(group.take_stdout().expect("fixture stdout"));
    let mut readiness = String::new();
    tokio::time::timeout(Duration::from_secs(3), stdout.read_line(&mut readiness))
        .await
        .expect("grandchild readiness deadline")
        .expect("read grandchild readiness");
    let grandchild: libc::pid_t = readiness
        .strip_prefix("grandchild ")
        .expect("literal grandchild readiness")
        .trim()
        .parse()
        .expect("grandchild PID");
    assert!(grandchild > 1);
    assert_ne!(grandchild as u32, group.id());
    assert_eq!(
        unsafe { libc::getpgid(grandchild) },
        group.id() as libc::pid_t,
        "ready grandchild must still belong to the original group"
    );
    let mut byte = [0u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(20), stdout.read(&mut byte))
            .await
            .is_err(),
        "readiness must leave a live descriptor, not an already-closed fixture"
    );
    (group, stdout, stdin)
}

async fn assert_original_group_liveness_ended(stdout: &mut BufReader<ChildStdout>) {
    let mut remaining = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stdout.read_to_end(&mut remaining))
        .await
        .expect("original-group grandchild kept its liveness descriptor open after cleanup")
        .expect("read original-group liveness EOF");
    assert!(
        remaining.is_empty(),
        "unexpected fixture output: {remaining:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn normal_exit_kills_original_group_grandchild_before_reap() {
    let (mut group, mut stdout, mut stdin) = original_group_fixture().await;
    stdin.write_all(b"exit\n").await.unwrap();
    drop(stdin);
    tokio::time::timeout(Duration::from_secs(2), group.wait_unreaped())
        .await
        .expect("normal leader did not exit")
        .expect("non-reaping normal exit observation");
    let mut byte = [0u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(20), stdout.read(&mut byte))
            .await
            .is_err(),
        "grandchild must outlive the normally exited leader before cleanup"
    );
    let status = tokio::time::timeout(Duration::from_secs(2), group.finish())
        .await
        .expect("normal group finish deadline")
        .expect("normal group finish");
    assert_eq!(
        status.code(),
        Some(23),
        "normal leader status must survive cleanup"
    );
    assert_original_group_liveness_ended(&mut stdout).await;
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_kills_original_group_including_grandchild() {
    let (mut group, mut stdout, mut stdin) = original_group_fixture().await;
    stdin.write_all(b"wait\n").await.unwrap();
    drop(stdin);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), group.wait_unreaped())
            .await
            .is_err(),
        "timeout fixture leader must remain live until the timeout"
    );
    let status = tokio::time::timeout(Duration::from_secs(2), group.finish())
        .await
        .expect("timeout group finish deadline")
        .expect("timeout group finish");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert_original_group_liveness_ended(&mut stdout).await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_wait_cleans_original_group_before_child_drop() {
    let (mut group, mut stdout, mut stdin) = original_group_fixture().await;
    stdin.write_all(b"wait\n").await.unwrap();
    drop(stdin);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), group.wait_unreaped())
            .await
            .is_err()
    );
    drop(group);
    assert_original_group_liveness_ended(&mut stdout).await;
}

#[tokio::test(flavor = "current_thread")]
async fn non_reaping_probe_preserves_terminal_status_for_repeated_observation() {
    // Deliberately avoid GroupChild: a consuming-observer mutant must never be
    // followed by any real numeric group signal, even on assertion failure.
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 23"])
        .kill_on_drop(false)
        .spawn()
        .expect("spawn standalone observation fixture");
    let pid = child.id().unwrap() as libc::pid_t;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if probe_unreaped(pid).expect("first non-reaping probe") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("standalone child did not exit");
    assert!(
        probe_unreaped(pid).expect("second probe must still own the unreaped child"),
        "terminal observation must preserve waitability"
    );
    assert_eq!(child.wait().await.unwrap().code(), Some(23));
}

#[tokio::test(flavor = "current_thread")]
async fn exit_observer_pending_allows_current_thread_timer_and_fast_exit() {
    let mut command = isolated_command(
        "exec /usr/bin/perl -e '$SIG{ALRM}=sub { exit 7 }; alarm 2; scalar <STDIN>; exit 7'",
    );
    let mut group = GroupChild::spawn(&mut command).unwrap();
    let mut stdin = group.lifecycle.backend.child.stdin.take().unwrap();
    let progress = Rc::new(RefCell::new(Vec::new()));
    let timer_progress = progress.clone();
    let observer_progress = progress.clone();
    tokio::time::timeout(Duration::from_secs(3), async {
        let ((), observed) = tokio::join!(
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                timer_progress.borrow_mut().push("timer");
                let _ = stdin.write_all(b"release\n").await;
            },
            async {
                let result = group.wait_unreaped().await;
                observer_progress.borrow_mut().push("observed");
                result
            }
        );
        observed.unwrap();
    })
    .await
    .expect("current-thread timer/child observation did not progress");
    assert_eq!(
        *progress.borrow(),
        ["timer", "observed"],
        "current-thread timer must progress before target exit observation"
    );
    assert_eq!(group.finish().await.unwrap().code(), Some(7));

    for _ in 0..4 {
        let mut fast = GroupChild::spawn(&mut isolated_command("exit 9")).unwrap();
        tokio::time::timeout(Duration::from_secs(2), fast.wait_unreaped())
            .await
            .expect("fast exit lost its only notification")
            .unwrap();
        assert_eq!(fast.finish().await.unwrap().code(), Some(9));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn group_spawn_disables_tokio_pid_kill_on_drop() {
    let mut command = isolated_command("/bin/sleep 10");
    command.kill_on_drop(true);
    let mut group = GroupChild::spawn(&mut command).expect("spawn drop-policy fixture");
    assert!(
        !command.get_kill_on_drop(),
        "GroupChild must own cleanup without Tokio's independent numeric-PID drop kill"
    );
    assert_eq!(group.finish().await.unwrap().signal(), Some(libc::SIGKILL));
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "current_thread")]
async fn macos_exited_leader_without_descendants_keeps_status() {
    let mut group = GroupChild::spawn(&mut isolated_command("exit 23"))
        .expect("spawn isolated no-descendant fixture");
    tokio::time::timeout(Duration::from_secs(2), group.wait_unreaped())
        .await
        .expect("no-descendant leader did not exit")
        .expect("observe no-descendant exit without consuming its status");
    let finished = tokio::time::timeout(Duration::from_secs(2), group.finish())
        .await
        .expect("exited leader cleanup did not finish");
    assert!(
        finished.is_ok(),
        "macOS exited-only group EPERM must preserve successful finish: {finished:?}"
    );
    assert_eq!(finished.unwrap().code(), Some(23));
    assert_eq!(
        group.exit_status().and_then(|status| status.code()),
        Some(23)
    );
    assert_eq!(group.finish().await.unwrap().code(), Some(23));
}
