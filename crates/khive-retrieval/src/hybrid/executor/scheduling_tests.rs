use std::future::{pending, poll_fn, Future};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use async_trait::async_trait;
use khive_score::DeterministicScore;
use khive_storage::{
    capture_request_read_context, scope_request_read_cancellation, scope_request_read_deadline_at,
    RequestReadDeadline, RequestReadStopReason, TextQueryMode,
};
use tokio::sync::{watch, Notify};

use super::*;

type TestError = &'static str;
type TestOutcome = ExecutorOutcome<usize, usize, usize, (), TestError>;
type TestResult = Result<TestOutcome, ExecutorError<TestError, usize>>;
type TestPreparer = dyn VectorGroupPreparer<Filter = (), Evidence = usize, Error = TestError>;

const NAMESPACES: &[&str] = &["selected"];
const MODE: TextQueryMode = TextQueryMode::Plain;

#[derive(Default)]
struct Observations {
    started: Mutex<Vec<usize>>,
    finished: Mutex<Vec<usize>>,
    deadlines: Mutex<Vec<tokio::time::Instant>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

struct Active(Arc<Observations>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct FakeArm {
    id: usize,
    observations: Arc<Observations>,
    release: Option<Arc<Notify>>,
    failure: Option<TestError>,
    stage_timeout: Option<Duration>,
    cancel_on_return: Option<watch::Sender<bool>>,
    prepared: Option<Arc<AtomicBool>>,
    expected_embedding: Option<Vec<f32>>,
}

impl FakeArm {
    fn ready(id: usize, observations: &Arc<Observations>) -> Self {
        Self {
            id,
            observations: observations.clone(),
            release: None,
            failure: None,
            stage_timeout: None,
            cancel_on_return: None,
            prepared: None,
            expected_embedding: None,
        }
    }

    fn held(id: usize, observations: &Arc<Observations>) -> Self {
        Self {
            release: Some(Arc::new(Notify::new())),
            ..Self::ready(id, observations)
        }
    }

    fn release(&self) {
        self.release.as_ref().expect("held arm").notify_one();
    }
}

#[async_trait]
impl ExecutorArm for FakeArm {
    type Id = usize;
    type Label = usize;
    type Evidence = usize;
    type Filter = ();
    type Error = TestError;

    async fn search<'a>(
        &'a self,
        request: ArmRequest<'a, ()>,
    ) -> Result<ArmCompletion<usize, usize, usize>, ArmFailure<TestError, usize>> {
        assert_eq!(request.scope.namespaces, NAMESPACES);
        assert_eq!(request.candidate_limit, 1);
        if let Some(prepared) = &self.prepared {
            assert!(
                prepared.load(Ordering::SeqCst),
                "ANN preceded embedding barrier"
            );
        }
        if let Some(expected) = &self.expected_embedding {
            match request.input {
                ArmInput::Vector {
                    input: VectorInput::Embedding(actual),
                    ..
                } => {
                    assert_eq!(*actual, expected.as_slice());
                }
                _ => panic!("prepared vector was not supplied to its original arm"),
            }
        }
        self.observations.started.lock().unwrap().push(self.id);
        let active = self.observations.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.observations.peak.fetch_max(active, Ordering::SeqCst);
        let _active = Active(self.observations.clone());
        if let Some(deadline) = capture_request_read_context().deadline() {
            self.observations
                .deadlines
                .lock()
                .unwrap()
                .push(deadline.async_at());
        }
        if let Some(release) = &self.release {
            release.notified().await;
        }
        if let Some(duration) = self.stage_timeout {
            assert!(tokio::time::timeout(duration, pending::<()>())
                .await
                .is_err());
        }
        self.observations.finished.lock().unwrap().push(self.id);
        if let Some(cancel) = &self.cancel_on_return {
            cancel.send(true).expect("request remains scoped");
        }
        match self.failure {
            Some(error) => Err(ArmFailure::Ordinary(error)),
            None => Ok(ArmCompletion {
                hits: vec![(self.id, DeterministicScore::ZERO, self.id)],
                route: ArmRoute::Primary,
                evidence: self.id,
            }),
        }
    }
}

struct Identity;

#[async_trait]
impl CandidateHydrator for Identity {
    type Id = usize;
    type Label = usize;
    type Evidence = usize;
    type Filter = ();
    type Item = ();
    type State = ();
    type Error = TestError;

    fn validate_outcomes(
        &self,
        collected: CollectedArms<usize, usize, usize, TestError>,
    ) -> Result<CollectedArms<usize, usize, usize, TestError>, TestError> {
        Ok(collected)
    }

    async fn prepare_arms<'a>(
        &'a self,
        _scope: SearchScope<'a, ()>,
        arms: Vec<Vec<Scored<usize, usize>>>,
    ) -> Result<PreparedArms<usize, usize, ()>, TestError> {
        Ok(PreparedArms { arms, state: () })
    }

    async fn hydrate_filter_rank<'a>(
        &'a self,
        _scope: SearchScope<'a, ()>,
        _state: (),
        candidates: Vec<Scored<usize, usize>>,
    ) -> Result<Vec<HydratedHit<usize, usize, ()>>, TestError> {
        Ok(candidates
            .into_iter()
            .map(|(id, score, label)| HydratedHit {
                id,
                score,
                label,
                item: (),
            })
            .collect())
    }
}

fn specs<'a>(arms: &'a [FakeArm], vector_slots: &[usize]) -> Vec<ArmSpec<'a, FakeArm>> {
    const KEYS: [&str; 5] = ["zero", "one", "two", "three", "four"];
    arms.iter()
        .enumerate()
        .map(|(slot, adapter)| ArmSpec {
            key: KEYS[slot],
            adapter,
            scope: SearchScope {
                namespaces: NAMESPACES,
                filter: &(),
            },
            input: Some(if vector_slots.contains(&slot) {
                ArmInput::Vector {
                    engine: KEYS[slot],
                    model_id: "physical-model",
                    space_id: None,
                    input: VectorInput::Text("query"),
                }
            } else {
                ArmInput::Text {
                    query: "query",
                    mode: &MODE,
                    snippet_chars: 0,
                }
            }),
            candidate_limit: 1,
            failure: FailurePolicy::Required,
        })
        .collect()
}

async fn execute(
    arms: &[ArmSpec<'_, FakeArm>],
    vector_slots: &[usize],
    preparer: Option<&TestPreparer>,
    max_in_flight: usize,
    memory: bool,
) -> TestResult {
    let request = ExecutorRequest {
        operation: "executor.scheduling_test",
        scope: SearchScope {
            namespaces: NAMESPACES,
            filter: &(),
        },
        arms,
        max_in_flight: NonZeroUsize::new(max_in_flight).unwrap(),
        vector_slots,
        vector_preparer: preparer,
        max_candidates: arms.len(),
        final_limit: arms.len(),
    };
    let resolve = || {
        Ok(|arms: Vec<Vec<Scored<usize, usize>>>| async move {
            Ok::<_, TestError>(arms.into_iter().flatten().collect())
        })
    };
    if memory {
        execute_memory_hybrid(request, &Identity, resolve).await
    } else {
        execute_hybrid(request, &Identity, resolve).await
    }
}

async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

fn successful(result: TestResult) -> TestOutcome {
    match result {
        Ok(outcome) => outcome,
        Err(_) => panic!("expected executor success"),
    }
}

fn assert_stopped(result: TestResult) {
    assert!(matches!(result, Err(ExecutorError::RequestStopped(_))));
}

#[tokio::test]
async fn bounded_in_flight_preserves_plan_slots_despite_reverse_completion() {
    let observations = Arc::new(Observations::default());
    let arms: Vec<_> = (0..4).map(|id| FakeArm::held(id, &observations)).collect();
    let plan = specs(&arms, &[1, 2, 3]);
    let work = execute(&plan, &[1, 2, 3], None, 2, false);
    tokio::pin!(work);
    assert!(poll_once(work.as_mut()).await.is_pending());
    assert_eq!(*observations.started.lock().unwrap(), [0, 1]);
    arms[1].release();
    assert!(poll_once(work.as_mut()).await.is_pending());
    assert_eq!(*observations.started.lock().unwrap(), [0, 1, 2]);
    arms[2].release();
    assert!(poll_once(work.as_mut()).await.is_pending());
    assert_eq!(*observations.started.lock().unwrap(), [0, 1, 2, 3]);
    arms[3].release();
    assert!(poll_once(work.as_mut()).await.is_pending());
    arms[0].release();
    let outcome = successful(work.await);
    assert_eq!(*observations.finished.lock().unwrap(), [1, 2, 3, 0]);
    assert_eq!(observations.peak.load(Ordering::SeqCst), 2);
    assert_eq!(observations.active.load(Ordering::SeqCst), 0);
    assert_eq!(
        outcome.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(
        outcome
            .arms
            .iter()
            .map(|arm| arm.key.as_str())
            .collect::<Vec<_>>(),
        ["zero", "one", "two", "three"]
    );
}

struct Preparation {
    release: Notify,
    called: Mutex<Option<(Vec<usize>, usize)>>,
    complete: Arc<AtomicBool>,
    omitted_position: Option<usize>,
}

#[async_trait]
impl VectorGroupPreparer for Preparation {
    type Filter = ();
    type Evidence = usize;
    type Error = TestError;

    async fn prepare<'a>(
        &'a self,
        requests: Vec<(usize, ArmRequest<'a, ()>)>,
        vector_slot_count: usize,
    ) -> Result<VectorPreparation<usize>, TestError> {
        let positions: Vec<_> = requests.iter().map(|(position, _)| *position).collect();
        for (_, request) in &requests {
            assert_eq!(request.scope.namespaces, NAMESPACES);
            assert_eq!(request.candidate_limit, 1);
        }
        *self.called.lock().unwrap() = Some((positions.clone(), vector_slot_count));
        self.release.notified().await;
        self.complete.store(true, Ordering::SeqCst);
        Ok(VectorPreparation {
            arms: positions
                .into_iter()
                .map(|position| PreparedVector {
                    embedding: (Some(position) != self.omitted_position)
                        .then(|| vec![(position + 1) as f32]),
                    evidence: 100 + position,
                })
                .collect(),
            evidence: 900,
        })
    }
}

#[tokio::test]
async fn embedding_barrier_preserves_skipped_and_failed_preparation_positions() {
    let observations = Arc::new(Observations::default());
    let complete = Arc::new(AtomicBool::new(false));
    let mut arms: Vec<_> = (0..5).map(|id| FakeArm::ready(id, &observations)).collect();
    for arm in &mut arms[1..] {
        arm.prepared = Some(complete.clone());
        arm.expected_embedding = Some(vec![arm.id as f32]);
    }
    let mut plan = specs(&arms, &[1, 2, 3, 4]);
    plan[1].input = None;
    let preparation = Preparation {
        release: Notify::new(),
        called: Mutex::new(None),
        complete,
        omitted_position: Some(1),
    };
    let work = execute(&plan, &[1, 2, 3, 4], Some(&preparation), 2, true);
    tokio::pin!(work);
    assert!(poll_once(work.as_mut()).await.is_pending());
    assert_eq!(*observations.started.lock().unwrap(), [0]);
    assert_eq!(
        *preparation.called.lock().unwrap(),
        Some((vec![1, 2, 3], 4))
    );
    preparation.release.notify_one();
    let outcome = successful(work.await);
    assert_eq!(*observations.started.lock().unwrap(), [0, 3, 4]);
    assert_eq!(
        outcome.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        [0, 3, 4]
    );
    assert_eq!(outcome.vector_preparation, Some(900));
    assert!(matches!(outcome.arms[1].state, ArmState::Skipped));
    assert!(matches!(outcome.arms[2].state, ArmState::Skipped));
    assert_eq!(
        outcome
            .arms
            .iter()
            .map(|arm| arm.returned)
            .collect::<Vec<_>>(),
        [1, 0, 0, 1, 1]
    );
    assert_eq!(
        outcome
            .arms
            .iter()
            .map(|arm| arm.preparation)
            .collect::<Vec<_>>(),
        [None, None, Some(101), Some(102), Some(103)]
    );
}

#[tokio::test]
async fn memory_collects_every_vector_before_selecting_original_slot_failure() {
    for (count, omitted_position) in [(1, None), (2, None), (3, None), (3, Some(1))] {
        let observations = Arc::new(Observations::default());
        let mut arms: Vec<_> = (0..count)
            .map(|id| FakeArm::held(id, &observations))
            .collect();
        for arm in &mut arms {
            arm.failure = Some(if arm.id == 0 {
                "first-slot"
            } else {
                "later-slot"
            });
        }
        let slots: Vec<_> = (0..count).collect();
        let plan = specs(&arms, &slots);
        let preparation = Preparation {
            release: Notify::new(),
            called: Mutex::new(None),
            complete: Arc::new(AtomicBool::new(false)),
            omitted_position,
        };
        preparation.release.notify_one();
        let work = execute(&plan, &slots, Some(&preparation), count, true);
        tokio::pin!(work);
        assert!(poll_once(work.as_mut()).await.is_pending());
        let prepared_count = count - usize::from(omitted_position.is_some());
        assert_eq!(observations.started.lock().unwrap().len(), prepared_count);
        // A first-slot failure cannot short-circuit the still-running later arms.
        arms[0].release();
        if prepared_count > 1 {
            assert!(poll_once(work.as_mut()).await.is_pending());
            for arm in arms[1..]
                .iter()
                .rev()
                .filter(|arm| Some(arm.id) != omitted_position)
            {
                arm.release();
            }
        }
        assert!(matches!(
            work.await,
            Err(ExecutorError::Arm {
                slot: 0,
                error: "first-slot"
            })
        ));
        assert_eq!(observations.finished.lock().unwrap().len(), prepared_count);
        assert_eq!(observations.active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn runtime_waits_for_text_priority_while_memory_outer_failure_short_circuits() {
    for memory in [false, true] {
        let observations = Arc::new(Observations::default());
        let mut arms = [
            FakeArm::ready(0, &observations),
            FakeArm::held(1, &observations),
        ];
        arms[0].failure = Some("vector");
        arms[1].failure = Some("text");
        let plan = specs(&arms, &[0]);
        let work = execute(&plan, &[0], None, 2, memory);
        tokio::pin!(work);
        if memory {
            assert!(matches!(
                work.await,
                Err(ExecutorError::Arm {
                    slot: 0,
                    error: "vector"
                })
            ));
            assert_eq!(*observations.finished.lock().unwrap(), [0]);
        } else {
            assert!(poll_once(work.as_mut()).await.is_pending());
            arms[1].release();
            assert!(matches!(
                work.await,
                Err(ExecutorError::Arm {
                    slot: 1,
                    error: "text"
                })
            ));
            assert_eq!(*observations.finished.lock().unwrap(), [0, 1]);
        }
        assert_eq!(observations.active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn cancellation_before_or_during_tolerated_failure_cannot_degrade_to_success() {
    for before in [true, false] {
        let (cancel, receiver) = watch::channel(before);
        let observations = Arc::new(Observations::default());
        let mut arms = [
            FakeArm::held(0, &observations),
            FakeArm::ready(1, &observations),
        ];
        arms[1].failure = Some("tolerated");
        let mut plan = specs(&arms, &[1]);
        plan[1].failure = FailurePolicy::TolerateOrdinary;
        scope_request_read_cancellation(receiver, async {
            let work = execute(&plan, &[1], None, 2, false);
            tokio::pin!(work);
            if before {
                assert_stopped(work.await);
                assert!(observations.started.lock().unwrap().is_empty());
            } else {
                assert!(poll_once(work.as_mut()).await.is_pending());
                assert_eq!(*observations.finished.lock().unwrap(), [1]);
                cancel.send(true).unwrap();
                assert_stopped(work.await);
            }
        })
        .await;
        assert_eq!(observations.active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn cancellation_wins_when_arm_returns_ready_success_or_error_in_same_poll() {
    for (failure, tolerate) in [
        (None, false),
        (Some("ready-required"), false),
        (Some("ready-tolerated"), true),
    ] {
        let (cancel, receiver) = watch::channel(false);
        let observations = Arc::new(Observations::default());
        let mut arm = FakeArm::ready(0, &observations);
        arm.cancel_on_return = Some(cancel);
        arm.failure = failure;
        let arms = [arm, FakeArm::ready(1, &observations)];
        let mut plan = specs(&arms, &[0, 1]);
        if tolerate {
            plan[0].failure = FailurePolicy::TolerateOrdinary;
        }
        scope_request_read_cancellation(receiver, async {
            assert_stopped(execute(&plan, &[0, 1], None, 2, false).await);
            assert_eq!(
                capture_request_read_context().stop_reason(),
                Some(RequestReadStopReason::Cancelled)
            );
        })
        .await;
        assert_eq!(*observations.finished.lock().unwrap(), [0]);
        assert_eq!(*observations.started.lock().unwrap(), [0]);
    }
}

#[tokio::test(start_paused = true)]
async fn executor_inherits_absolute_deadline_without_renewing_nested_budget() {
    let original = RequestReadDeadline::after(Duration::from_secs(10));
    let observations = Arc::new(Observations::default());
    let arms = [FakeArm::held(0, &observations)];
    let plan = specs(&arms, &[0]);
    scope_request_read_deadline_at(original, async {
        tokio::time::advance(Duration::from_secs(9)).await;
        let later = RequestReadDeadline::after(Duration::from_secs(100));
        scope_request_read_deadline_at(later, async {
            let work = execute(&plan, &[0], None, 1, false);
            tokio::pin!(work);
            assert!(poll_once(work.as_mut()).await.is_pending());
            assert_eq!(
                *observations.deadlines.lock().unwrap(),
                [original.async_at()]
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            assert_stopped(work.await);
            assert_eq!(
                capture_request_read_context().stop_reason(),
                Some(RequestReadStopReason::Deadline)
            );
        })
        .await;
    })
    .await;
    assert_eq!(observations.active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn stage_timeout_degrades_only_while_outer_request_is_live() {
    for outer_seconds in [1, 10] {
        let observations = Arc::new(Observations::default());
        let mut arms = [
            FakeArm::ready(0, &observations),
            FakeArm::ready(1, &observations),
        ];
        arms[1].stage_timeout = Some(Duration::from_secs(2));
        arms[1].failure = Some("stage-timeout");
        let mut plan = specs(&arms, &[1]);
        plan[1].failure = FailurePolicy::TolerateOrdinary;
        let deadline = RequestReadDeadline::after(Duration::from_secs(outer_seconds));
        scope_request_read_deadline_at(deadline, async {
            let work = execute(&plan, &[1], None, 2, false);
            tokio::pin!(work);
            assert!(poll_once(work.as_mut()).await.is_pending());
            tokio::time::advance(Duration::from_secs(2)).await;
            let result = work.await;
            if outer_seconds == 1 {
                assert_stopped(result);
            } else {
                let outcome = successful(result);
                assert_eq!(
                    outcome.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
                    [0]
                );
                assert!(matches!(
                    outcome.arms[1].state,
                    ArmState::Failed("stage-timeout")
                ));
                assert_eq!(capture_request_read_context().stop_reason(), None);
            }
        })
        .await;
        assert_eq!(observations.active.load(Ordering::SeqCst), 0);
    }
}
