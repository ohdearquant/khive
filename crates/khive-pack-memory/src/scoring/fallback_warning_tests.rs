use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    calculate_score, calculate_score_with_reporter, AdjustmentCondition, AdjustmentOp,
    ScoreAdjustment, ScoreFallbackWarnings, ScoreInput, ScoringConfig, ScoringWeights,
};

const TARGET: &str = "khive.memory.scoring";
const MESSAGE: &str = "Memory score arithmetic fallback returned zero";
const MEMORY_TYPE: &str = "private-memory-type-sentinel";
const CONTENT: &str = "private-content-sentinel";
const ENTITY: &str = "private-entity-sentinel";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq)]
struct FallbackEvent {
    target: String,
    level: tracing::Level,
    fields: Vec<(String, String)>,
}

#[derive(Clone, Default)]
struct FallbackEvents(Arc<Mutex<Vec<FallbackEvent>>>);

impl tracing::Subscriber for FallbackEvents {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == TARGET && *metadata.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(Vec<(String, String)>);
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.push((field.name().to_owned(), value.to_owned()));
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push((field.name().to_owned(), format!("{value:?}")));
            }
        }
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        fields.0.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        self.0.lock().unwrap().push(FallbackEvent {
            target: event.metadata().target().to_owned(),
            level: *event.metadata().level(),
            fields: fields.0,
        });
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn capture(action: impl FnOnce()) -> Vec<FallbackEvent> {
    let events = FallbackEvents::default();
    // Keep the supplied precedent's second live dispatcher so concurrent tests
    // cannot cache the callsite as disabled for this collector.
    let _second_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::subscriber::with_default(events.clone(), action);
    let captured = events.0.lock().unwrap().clone();
    captured
}

fn assert_fallbacks(events: &[FallbackEvent], reasons: &[&str]) {
    let mut actual = events.to_vec();
    actual.sort_unstable_by(|a, b| a.fields.cmp(&b.fields));
    let mut expected: Vec<_> = reasons
        .iter()
        .map(|reason| FallbackEvent {
            target: TARGET.into(),
            level: tracing::Level::WARN,
            fields: vec![
                ("message".into(), MESSAGE.into()),
                ("reason".into(), (*reason).into()),
            ],
        })
        .collect();
    expected.sort_unstable_by(|a, b| a.fields.cmp(&b.fields));
    assert_eq!(actual, expected);
    let all_fields = format!("{events:?}");
    for sentinel in [MEMORY_TYPE, CONTENT, ENTITY] {
        assert!(
            !all_fields.contains(sentinel),
            "fallback warning exposed input"
        );
    }
}

fn input(entity_names: &[String]) -> ScoreInput<'_> {
    ScoreInput {
        salience: 1.0,
        memory_type_str: MEMORY_TYPE,
        content: CONTENT,
        created_at_millis: 0,
        decay_factor: 0.0,
        now_millis: 0,
        relevance_score: 0.5,
        entity_names,
    }
}

fn finite_config() -> ScoringConfig {
    ScoringConfig {
        weights: ScoringWeights {
            salience: 0.0,
            temporal: 0.0,
            relevance: 0.5,
        },
        adjustments: Vec::new(),
        ..ScoringConfig::default()
    }
}

fn failure_cases() -> [(ScoringConfig, &'static str); 3] {
    let mut temporal = finite_config();
    temporal.weights.temporal = f32::INFINITY;
    let mut salience = finite_config();
    salience.weights.salience = f32::INFINITY;
    let mut composite = finite_config();
    composite.adjustments = vec![
        ScoreAdjustment {
            condition: AdjustmentCondition::MemoryType {
                kind: MEMORY_TYPE.into(),
            },
            operation: AdjustmentOp::Add { value: f32::MAX },
        },
        ScoreAdjustment {
            condition: AdjustmentCondition::MemoryType {
                kind: MEMORY_TYPE.into(),
            },
            operation: AdjustmentOp::Multiply { factor: f32::MAX },
        },
    ];
    [
        (temporal, "temporal_boost"),
        (salience, "salience_boost"),
        (composite, "nonfinite_composite"),
    ]
}

#[test]
fn finite_zero_and_positive_scores_emit_no_fallback_warning() {
    let reporter = ScoreFallbackWarnings::new();
    let config = finite_config();
    let mut input = input(&[]);
    let events = capture(|| {
        assert_eq!(calculate_score(&input, &config), 0.25);
        assert_eq!(
            calculate_score_with_reporter(&input, &config, &reporter),
            0.25
        );
        input.relevance_score = 0.0;
        assert_eq!(calculate_score(&input, &config), 0.0);
        assert_eq!(
            calculate_score_with_reporter(&input, &config, &reporter),
            0.0
        );
    });
    assert_fallbacks(&events, &[]);
    // Quiet scores must not spend any of this reporter's later warning budget.
    let (config, reason) = failure_cases().into_iter().next().unwrap();
    let events = capture(|| {
        assert_eq!(
            calculate_score_with_reporter(&input, &config, &reporter),
            0.0
        );
    });
    assert_fallbacks(&events, &[reason]);
}

#[test]
fn every_arithmetic_fallback_site_emits_its_fixed_warning() {
    let names = [ENTITY.to_owned()];
    let input = input(&names);
    for (config, reason) in failure_cases() {
        let reporter = ScoreFallbackWarnings::new();
        let events = capture(|| {
            assert_eq!(
                calculate_score_with_reporter(&input, &config, &reporter),
                0.0
            );
        });
        assert_fallbacks(&events, &[reason]);
    }
}

#[test]
fn repeated_fallbacks_warn_at_most_once_for_each_reason() {
    let reporter = ScoreFallbackWarnings::new();
    let input = input(&[]);
    let cases = failure_cases();
    let events = capture(|| {
        for _ in 0..16 {
            for (config, _) in &cases {
                assert_eq!(
                    calculate_score_with_reporter(&input, config, &reporter),
                    0.0
                );
            }
        }
    });
    assert_fallbacks(
        &events,
        &["temporal_boost", "salience_boost", "nonfinite_composite"],
    );
}

#[test]
fn disabled_logging_preserves_the_first_later_warning_for_each_reason() {
    let reporter = ScoreFallbackWarnings::new();
    let input = input(&[]);
    let cases = failure_cases();
    let _second_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
        for (config, _) in &cases {
            assert_eq!(
                calculate_score_with_reporter(&input, config, &reporter),
                0.0
            );
        }
    });
    let events = capture(|| {
        for (config, _) in &cases {
            assert_eq!(
                calculate_score_with_reporter(&input, config, &reporter),
                0.0
            );
        }
    });
    assert_fallbacks(
        &events,
        &["temporal_boost", "salience_boost", "nonfinite_composite"],
    );
}

#[test]
fn concurrent_callers_share_one_warning_budget_per_reason() {
    let reporter = Arc::new(ScoreFallbackWarnings::new());
    let events = FallbackEvents::default();
    let _second_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let (ready_tx, ready_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let mut release_senders = Vec::new();
    let deadline = Instant::now() + TEST_TIMEOUT;
    for _ in 0..8 {
        let reporter = reporter.clone();
        let events = events.clone();
        let ready = ready_tx.clone();
        let finished = finished_tx.clone();
        let (release_tx, release_rx) = mpsc::channel();
        release_senders.push(release_tx);
        // Completion is observed through a watchdog, never an unbounded join.
        let _worker = std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ready.send(()).expect("score worker readiness observer");
                release_rx
                    .recv_timeout(TEST_TIMEOUT)
                    .expect("score worker must be released before its deadline");
                // Install capture in this thread; scoped dispatchers are thread-local.
                tracing::subscriber::with_default(events, || {
                    let input = input(&[]);
                    let cases = failure_cases();
                    for _ in 0..16 {
                        for (config, _) in &cases {
                            assert_eq!(
                                calculate_score_with_reporter(&input, config, &reporter),
                                0.0
                            );
                        }
                    }
                });
            }));
            let _ = finished.send(outcome.is_ok());
        });
    }
    drop(ready_tx);
    drop(finished_tx);
    for _ in 0..8 {
        ready_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("all score workers must become ready before the deadline");
    }
    for release in release_senders {
        release.send(()).expect("release a score worker");
    }
    for _ in 0..8 {
        let succeeded = finished_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("all score workers must finish before the deadline");
        assert!(succeeded, "score worker assertion failed");
    }
    let captured = events.0.lock().unwrap().clone();
    assert_fallbacks(
        &captured,
        &["temporal_boost", "salience_boost", "nonfinite_composite"],
    );
}
