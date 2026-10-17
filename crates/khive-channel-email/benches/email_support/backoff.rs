use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::time::{Duration, Instant};

use futures::task::noop_waker_ref;
use khive_channel_email::{BackoffTick, ImapBackoff, ImapSingleFlight};
use tokio::runtime::Runtime;
use tokio::sync::OwnedSemaphorePermit;

pub fn executor() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .expect("benchmark executor")
}

pub struct BackoffFixture {
    pub state: ImapBackoff,
    attempt: u32,
    step: Duration,
    warn: bool,
}
impl BackoffFixture {
    pub fn new(prior_failures: u32) -> Self {
        assert!([0, 3, 10].contains(&prior_failures));
        let mut state = ImapBackoff::new(Duration::from_secs(1), Duration::from_secs(16));
        for _ in 0..prior_failures {
            state.record_failure();
        }
        let step = Duration::from_secs((1_u64 << prior_failures).min(16));
        let fixture = Self {
            state,
            attempt: prior_failures + 1,
            step,
            warn: prior_failures < 5,
        };
        let mut reset = fixture.state.clone();
        reset.record_success();
        assert_eq!(reset.attempt(), 0);
        let first = reset.record_failure();
        assert_eq!(first.attempt, 1);
        assert_eq!(first.step, Duration::from_secs(1));
        assert!(first.should_warn);
        fixture
    }
    pub fn validate(&self, tick: BackoffTick) {
        assert_eq!(tick.attempt, self.attempt);
        assert_eq!(tick.step, self.step);
        assert_eq!(tick.should_warn, self.warn);
        assert!((self.step..=Duration::from_secs(16)).contains(&tick.delay));
    }
}

type Acquire = Pin<Box<dyn Future<Output = OwnedSemaphorePermit>>>;

pub struct Queue {
    guard: ImapSingleFlight,
    blocker: Option<OwnedSemaphorePermit>,
    futures: Vec<Acquire>,
    pub waits: Vec<Duration>,
    queued: bool,
}
impl Queue {
    pub fn new(rt: &Runtime, count: usize) -> Self {
        assert!([1, 100].contains(&count));
        let guard = ImapSingleFlight::new();
        let blocker = Some(rt.block_on(guard.acquire()));
        let futures = (0..count)
            .map(|_| {
                let guard = guard.clone();
                Box::pin(async move { guard.acquire().await }) as Acquire
            })
            .collect();
        Self {
            guard,
            blocker,
            futures,
            waits: Vec::with_capacity(count),
            queued: false,
        }
    }
    pub fn enqueue(&mut self) {
        assert!(!self.queued);
        let mut cx = Context::from_waker(noop_waker_ref());
        for future in &mut self.futures {
            assert!(
                future.as_mut().poll(&mut cx).is_pending(),
                "every acquisition must queue behind the held permit"
            );
        }
        self.queued = true;
    }
    pub fn release_and_collect(&mut self, rt: &Runtime) -> Duration {
        assert!(self.queued);
        rt.block_on(async {
            let start = Instant::now();
            drop(self.blocker.take());
            for future in &mut self.futures {
                let permit = future.await;
                self.waits.push(start.elapsed());
                drop(permit);
            }
            start.elapsed()
        })
    }
    pub fn validate(&self, rt: &Runtime) {
        assert_eq!(self.waits.len(), self.futures.len());
        assert!(self.waits.windows(2).all(|pair| pair[0] <= pair[1]));
        // Retaining one permit must keep a second acquisition Pending, also after the batch.
        let permit = rt.block_on(self.guard.acquire());
        let mut second = Box::pin(self.guard.acquire());
        let mut cx = Context::from_waker(noop_waker_ref());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        drop(permit);
        drop(rt.block_on(second));
    }
}
