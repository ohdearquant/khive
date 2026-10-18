use super::*;

fn advance(start: Instant, nanos: u64) -> Instant {
    start.checked_add(Duration::from_nanos(nanos)).unwrap()
}

#[test]
fn lifecycle_separates_capacity_residence_and_service() {
    let metrics = WriterAdmissionMetrics::new();
    let start = Instant::now();
    let mut first = metrics.offer_at(start);
    let second = metrics.offer_at(start);
    let offered = metrics.snapshot().unwrap();
    assert_eq!(offered.queue_depth, 2);
    assert_eq!(offered.queue_depth_high_water, 2);
    assert_eq!(offered.admission_wait.count, 0);

    first.accepted_at(advance(start, 100));
    let accepted = metrics.snapshot().unwrap();
    assert_eq!(accepted.queue_depth, 2);
    assert_eq!(accepted.admission_wait.count, 1);
    assert_eq!(accepted.admission_wait.sum, Duration::from_nanos(100));
    first.start_execution_at(advance(start, 350));
    first.start_execution_at(advance(start, 900));
    let executing = metrics.snapshot().unwrap();
    assert_eq!(executing.queue_depth, 1);
    assert_eq!(executing.accepted_queue_age.count, 1);
    assert_eq!(executing.accepted_queue_age.sum, Duration::from_nanos(250));
    assert_eq!(executing.service.count, 0);
    first.reply_attempt_at(advance(start, 1_000));
    first.reply_attempt_at(advance(start, 2_000));
    first.start_execution_at(advance(start, 3_000));
    drop(first);
    drop(second);
    let settled = metrics.snapshot().unwrap();
    assert_eq!(settled.queue_depth, 0);
    assert_eq!(settled.queue_depth_high_water, 2);
    assert_eq!(settled.service.count, 1);
    assert_eq!(settled.service.sum, Duration::from_nanos(650));
    assert_eq!(settled.saturation_total, 0);
}

#[test]
fn offered_and_accepted_terminal_paths_do_not_fabricate_execution() {
    let metrics = WriterAdmissionMetrics::new();
    let start = Instant::now();
    drop(metrics.offer_at(start));
    let mut offered_reply = metrics.offer_at(start);
    offered_reply.reply_attempt_at(advance(start, 10));
    offered_reply.reply_attempt_at(advance(start, 20));
    drop(offered_reply);
    let mut accepted_drop = metrics.offer_at(start);
    accepted_drop.accepted_at(advance(start, 30));
    drop(accepted_drop);
    let mut accepted_reply = metrics.offer_at(start);
    accepted_reply.accepted_at(advance(start, 40));
    accepted_reply.reply_attempt_at(advance(start, 500));
    drop(accepted_reply);
    let state = metrics.snapshot().unwrap();
    assert_eq!(state.queue_depth, 0);
    assert_eq!(state.queue_depth_high_water, 1);
    assert_eq!(state.admission_wait.count, 2);
    assert_eq!(state.admission_wait.sum, Duration::from_nanos(70));
    assert_eq!(state.accepted_queue_age.count, 0);
    assert_eq!(state.service.count, 0);
    assert_eq!(state.saturation_total, 0);
}

#[test]
fn accepted_request_owns_metrics_until_storage_settlement() {
    let metrics = WriterAdmissionMetrics::new();
    let observer = Arc::clone(&metrics);
    let mut request = metrics.offer();
    request.accepted();
    drop(metrics);
    assert_eq!(observer.snapshot().unwrap().queue_depth, 1);
    request.start_execution();
    request.reply_attempt();
    drop(request);
    let snapshot = observer.snapshot().unwrap();
    assert_eq!(snapshot.queue_depth, 0);
    assert_eq!(snapshot.service.count, 1);
}

#[test]
fn executing_drop_preserves_partial_samples_without_reply_sample() {
    let metrics = WriterAdmissionMetrics::new();
    let start = Instant::now();
    let mut request = metrics.offer_at(start);
    request.accepted_at(advance(start, 100));
    request.start_execution_at(advance(start, 300));
    drop(request);
    let snapshot = metrics.snapshot().unwrap();
    assert_eq!(snapshot.queue_depth, 0);
    assert_eq!(snapshot.admission_wait.sum, Duration::from_nanos(100));
    assert_eq!(snapshot.accepted_queue_age.sum, Duration::from_nanos(200));
    assert_eq!(snapshot.service.count, 0);
    assert_eq!(snapshot.service.p95, P95Bound::NoSamples);
}

#[test]
fn saturation_is_explicit_and_per_writer() {
    let first = WriterAdmissionMetrics::new();
    let second = WriterAdmissionMetrics::new();
    let pending = first.offer();
    first.record_capacity_deadline();
    first.record_capacity_deadline();
    drop(pending);
    let snapshot = first.snapshot().unwrap();
    assert_eq!(snapshot.saturation_total, 2);
    assert_eq!(snapshot.queue_depth, 0);
    assert_eq!(snapshot.admission_wait.count, 0);
    assert_eq!(snapshot.accepted_queue_age.count, 0);
    assert_eq!(snapshot.service.count, 0);
    assert_eq!(second.snapshot().unwrap().saturation_total, 0);
    assert_eq!(second.snapshot().unwrap().queue_depth_high_water, 0);
}

#[test]
fn histogram_edges_use_exact_integer_nanoseconds() {
    let expected = [
        244_140,
        488_281,
        976_562,
        1_953_125,
        3_906_250,
        7_812_500,
        15_625_000,
        31_250_000,
        62_500_000,
        125_000_000,
        250_000_000,
        500_000_000,
        1_000_000_000,
        2_000_000_000,
        4_000_000_000,
        8_000_000_000,
        16_000_000_000,
        32_000_000_000,
    ];
    assert_eq!(BUCKET_MAX_NANOS, expected);
    assert_eq!(
        BUCKET_EXPONENTS,
        [-12, -11, -10, -9, -8, -7, -6, -5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5]
    );
    for (index, bound) in expected.into_iter().enumerate() {
        let mut histogram = Histogram::default();
        histogram.record(Duration::from_nanos(bound)).unwrap();
        assert_eq!(histogram.snapshot().unwrap().p95, P95Bound::Finite(index));
        histogram.record(Duration::from_nanos(bound + 1)).unwrap();
        let snapshot = histogram.snapshot().unwrap();
        assert_eq!(snapshot.buckets[index], 1);
        assert_eq!(snapshot.count, 2);
        assert_eq!(snapshot.sum, Duration::from_nanos(bound * 2 + 1));
        if index == 17 {
            assert_eq!(snapshot.overflow_count, 1);
            assert_eq!(snapshot.p95, P95Bound::Overflow);
        } else {
            assert_eq!(snapshot.buckets[index + 1], 1);
            assert_eq!(snapshot.overflow_count, 0);
            assert_eq!(snapshot.p95, P95Bound::Finite(index + 1));
        }
        assert_eq!(
            snapshot.buckets.iter().sum::<u64>() + snapshot.overflow_count,
            snapshot.count
        );
    }
}

#[test]
fn empty_zero_and_overflow_are_distinct_percentile_states() {
    let mut histogram = Histogram::default();
    assert_eq!(histogram.snapshot().unwrap().p95, P95Bound::NoSamples);
    histogram.record(Duration::ZERO).unwrap();
    assert_eq!(histogram.snapshot().unwrap().p95, P95Bound::Finite(0));
    assert_eq!(histogram.snapshot().unwrap().sum, Duration::ZERO);
    let mut overflow = Histogram::default();
    overflow.record(Duration::from_secs(33)).unwrap();
    let snapshot = overflow.snapshot().unwrap();
    assert_eq!(snapshot.p95, P95Bound::Overflow);
    assert_eq!(snapshot.overflow_count, 1);
    assert_eq!(snapshot.sum, Duration::from_secs(33));
}

#[test]
fn percentile_uses_nearest_rank_not_overflow_presence_or_interpolation() {
    let mut histogram = Histogram::default();
    for _ in 0..19 {
        histogram.record(Duration::ZERO).unwrap();
    }
    histogram.record(Duration::from_secs(33)).unwrap();
    assert_eq!(histogram.snapshot().unwrap().p95, P95Bound::Finite(0));
    histogram.record(Duration::from_secs(33)).unwrap();
    assert_eq!(histogram.snapshot().unwrap().p95, P95Bound::Overflow);
    let mut huge = Histogram {
        count: u64::MAX,
        overflow_count: 1,
        sum: Duration::from_secs(33),
        ..Histogram::default()
    };
    huge.buckets[0] = u64::MAX - 1;
    assert_eq!(huge.snapshot().unwrap().p95, P95Bound::Finite(0));
}

#[test]
fn injected_failure_is_sticky_across_all_lifecycle_calls() {
    for reason in [Unavailable::Poisoned, Unavailable::CounterOverflow] {
        let metrics = WriterAdmissionMetrics::new();
        let mut request = metrics.offer();
        request.accepted();
        metrics.disable_for_test(reason);
        request.start_execution();
        request.reply_attempt();
        drop(request);
        let mut later = metrics.offer();
        later.accepted();
        later.start_execution();
        later.reply_attempt();
        drop(later);
        metrics.record_capacity_deadline();
        metrics.disable_for_test(Unavailable::InvariantViolation);
        assert_eq!(metrics.snapshot(), Err(reason));
    }
}

#[test]
fn poison_disables_observation_without_panicking_or_recovering_counts() {
    let metrics = WriterAdmissionMetrics::new();
    let mut request = metrics.offer();
    request.accepted();
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = metrics.state.lock().unwrap();
        panic!("intentional telemetry poison");
    }));
    assert!(poisoned.is_err());
    assert_eq!(metrics.snapshot(), Err(Unavailable::Poisoned));
    let safe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        request.start_execution();
        request.reply_attempt();
        drop(request);
        let mut later = metrics.offer();
        later.accepted();
        later.reply_attempt();
        drop(later);
        metrics.record_capacity_deadline();
    }));
    assert!(safe.is_ok());
    assert_eq!(metrics.snapshot(), Err(Unavailable::Poisoned));
}

#[test]
fn counter_and_duration_overflow_never_wrap_or_resume() {
    let metrics = WriterAdmissionMetrics::new();
    {
        let mut state = metrics.state.lock().unwrap();
        state.queue_depth = u64::MAX;
        state.queue_depth_high_water = u64::MAX;
    }
    drop(metrics.offer());
    assert_eq!(metrics.snapshot(), Err(Unavailable::CounterOverflow));
    metrics.record_capacity_deadline();
    assert_eq!(metrics.snapshot(), Err(Unavailable::CounterOverflow));

    let metrics = WriterAdmissionMetrics::new();
    metrics.state.lock().unwrap().saturation_total = u64::MAX;
    metrics.record_capacity_deadline();
    assert_eq!(metrics.snapshot(), Err(Unavailable::CounterOverflow));

    for field in 0..3 {
        let metrics = WriterAdmissionMetrics::new();
        {
            let mut state = metrics.state.lock().unwrap();
            match field {
                0 => state.admission_wait.count = u64::MAX,
                1 => state.admission_wait.buckets[0] = u64::MAX,
                _ => state.admission_wait.sum = Duration::MAX,
            }
        }
        let start = Instant::now();
        let mut request = metrics.offer_at(start);
        request.accepted_at(advance(start, 1));
        drop(request);
        assert_eq!(metrics.snapshot(), Err(Unavailable::CounterOverflow));
    }
}

#[test]
fn invalid_phase_time_or_pending_count_is_unavailable_not_storage_failure() {
    let metrics = WriterAdmissionMetrics::new();
    let mut request = metrics.offer();
    request.start_execution();
    drop(request);
    assert_eq!(metrics.snapshot(), Err(Unavailable::InvariantViolation));

    let metrics = WriterAdmissionMetrics::new();
    let start = Instant::now();
    let mut request = metrics.offer_at(advance(start, 1));
    request.accepted_at(start);
    drop(request);
    assert_eq!(metrics.snapshot(), Err(Unavailable::InvariantViolation));

    let metrics = WriterAdmissionMetrics::new();
    let request = metrics.offer();
    metrics.state.lock().unwrap().queue_depth = 0;
    drop(request);
    assert_eq!(metrics.snapshot(), Err(Unavailable::InvariantViolation));

    let metrics = WriterAdmissionMetrics::new();
    metrics.state.lock().unwrap().service.count = 1;
    assert_eq!(metrics.snapshot(), Err(Unavailable::InvariantViolation));
    metrics.state.lock().unwrap().service.count = 0;
    assert_eq!(metrics.snapshot(), Err(Unavailable::InvariantViolation));
}
