//! Checked arithmetic paths. Correctness checks and input generation are untimed.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use khive_score::{
    min_max_band, min_max_band_f32, rrf_score_one_based, try_score_from_distance, ArithmeticError,
    Decay, DegenerateSpan, DeterministicScore, HalfLife, NamedWeights, ScoreError,
    ValidatedWeights,
};
use khive_types::DistanceMetric;

fn strict_cosine(c: &mut Criterion) {
    let mut group = c.benchmark_group("strict_cosine");
    for (name, distance) in [
        ("valid", 0.25_f32),
        ("below_range", -0.25),
        ("above_range", 2.25),
        ("nan", f32::NAN),
        ("infinity", f32::INFINITY),
    ] {
        let expected = match name {
            "valid" => Ok(DeterministicScore::from_f64(0.75)),
            "nan" | "infinity" => Err(ScoreError::NonFiniteDistance),
            _ => Err(ScoreError::InvalidDistanceRange {
                metric_name: "Cosine",
                dist_bits: distance.to_bits(),
            }),
        };
        assert_eq!(
            try_score_from_distance(distance, DistanceMetric::Cosine),
            expected
        );
        group.bench_function(name, |b| {
            b.iter(|| {
                try_score_from_distance(black_box(distance), black_box(DistanceMetric::Cosine))
            });
        });
    }
    group.finish();
}

fn one_based_rrf(c: &mut Criterion) {
    let ranks: Vec<_> = (1..=100)
        .map(|rank| NonZeroUsize::new(rank).unwrap())
        .collect();
    let expected: Vec<_> = ranks
        .iter()
        .map(|rank| DeterministicScore::from_f64(1.0 / (60 + rank.get()) as f64))
        .collect();
    let batch = || {
        ranks
            .iter()
            .map(|rank| rrf_score_one_based(black_box(*rank), black_box(60)))
            .collect::<Vec<_>>()
    };
    assert_eq!(batch(), expected);
    assert!(expected.windows(2).all(|pair| pair[0] > pair[1]));
    let rank = NonZeroUsize::new(50).unwrap();
    assert_eq!(rrf_score_one_based(rank, 60), expected[49]);
    let mut group = c.benchmark_group("rrf_one_based");
    group.bench_function("rank_50_k60", |b| {
        b.iter(|| rrf_score_one_based(black_box(rank), black_box(60)));
    });
    // Allocation and destruction of the complete batch output are included.
    group.bench_function("ranks_1_100_k60", |b| b.iter(batch));
    group.finish();
}

fn decay_paths(c: &mut Criterion) {
    let half = HalfLife::try_new(16.0).expect("positive finite half-life");
    assert_eq!(half.value(), 16.0);
    let variants = [
        ("exponential_rate", Decay::exponential(1.0 / 16.0).unwrap()),
        ("hyperbolic_rate", Decay::hyperbolic(1.0 / 16.0).unwrap()),
        (
            "exponential_half_life",
            Decay::exponential_with_half_life(half),
        ),
        (
            "hyperbolic_half_life",
            Decay::hyperbolic_with_half_life(half),
        ),
        ("power_law", Decay::PowerLaw(half)),
        ("none", Decay::None),
    ];
    let mut group = c.benchmark_group("decay_apply");
    for count in [100, 1000] {
        // Elapsed seconds, fixed origin zero: no clock reads or timestamp parsing.
        let elapsed: Vec<_> = (0..count).map(|index| index as f64).collect();
        for (name, decay) in variants {
            let apply = || {
                elapsed
                    .iter()
                    .map(|time| black_box(decay).apply(black_box(0.75), black_box(*time)))
                    .collect::<Result<Vec<_>, _>>()
            };
            let values = apply().expect("valid decay batch");
            assert_eq!(values.len(), count);
            assert_eq!(values[0], 0.75);
            for (&time, &actual) in elapsed.iter().zip(&values) {
                let expected = match name {
                    "exponential_rate" => 0.75 * (-time / 16.0).exp(),
                    "exponential_half_life" => 0.75 / 2.0_f64.powf(time / 16.0),
                    "none" => 0.75,
                    _ => 12.0 / (16.0 + time),
                };
                assert!(actual.is_finite() && (0.0..=0.75).contains(&actual));
                assert!((actual - expected).abs() <= 1e-12 * expected.max(f64::MIN_POSITIVE));
            }
            assert!(values.windows(2).all(|pair| pair[0] >= pair[1]));
            group.bench_function(BenchmarkId::new(name, count), |b| b.iter(apply));
        }
    }
    group.finish();
}

fn band_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("min_max_band");
    for count in [100, 1000] {
        let input: Vec<f64> = (0..count).map(|index| index as f64).collect();
        let input32: Vec<f32> = (0..count).map(|index| index as f32).collect();
        let run64 = || min_max_band(black_box(&input), 0.0, 1.0, 0.0, DegenerateSpan::Ceiling);
        let run32 =
            || min_max_band_f32(black_box(&input32), 0.0, 1.0, 0.0, DegenerateSpan::Ceiling);
        let values = run64().expect("nondegenerate f64 band");
        let values32 = run32().expect("nondegenerate f32 band");
        assert_eq!(values.len(), count);
        assert_eq!(values32.len(), count);
        assert_eq!((values[0], values[count - 1]), (0.0, 1.0));
        assert_eq!((values32[0], values32[count - 1]), (0.0, 1.0));
        for index in 0..count {
            let expected = index as f64 / (count - 1) as f64;
            assert!(values[index].is_finite() && (0.0..=1.0).contains(&values[index]));
            assert!(values32[index].is_finite() && (0.0..=1.0).contains(&values32[index]));
            assert!((values[index] - expected).abs() <= 4.0 * f64::EPSILON);
            assert!((f64::from(values32[index]) - expected).abs() <= 4.0 * f64::from(f32::EPSILON));
        }
        assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(values32.windows(2).all(|pair| pair[0] <= pair[1]));
        group.bench_function(BenchmarkId::new("f64", count), |b| b.iter(run64));
        group.bench_function(BenchmarkId::new("f32", count), |b| b.iter(run32));
    }
    group.finish();
}

fn weight_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("weight_validation");
    let invalid = ArithmeticError::OutOfRange {
        parameter: "weight",
    };
    for count in [2, 8, 32] {
        let ordered: Vec<f64> = (1..=count).map(|index| index as f64).collect();
        let named: HashMap<usize, f64> = ordered.iter().copied().enumerate().collect();
        let mut bad_ordered = ordered.clone();
        bad_ordered[count - 1] = -1.0;
        let mut bad_named = named.clone();
        bad_named.insert(count - 1, -1.0);
        assert_eq!(
            ValidatedWeights::try_new(ordered.clone())
                .unwrap()
                .as_slice(),
            ordered
        );
        assert_eq!(
            ValidatedWeights::try_new(bad_ordered.clone()).err(),
            Some(invalid)
        );
        let features: Vec<_> = (0..count).map(|key| (key, 7.5)).collect();
        let mean = NamedWeights::try_new(named.clone())
            .unwrap()
            .weighted_mean(&features)
            .unwrap();
        assert_eq!(mean, 7.5);
        assert_eq!(
            NamedWeights::try_new(bad_named.clone()).err(),
            Some(invalid)
        );
        for (name, input) in [
            ("ordered_valid", &ordered),
            ("ordered_invalid", &bad_ordered),
        ] {
            group.bench_function(BenchmarkId::new(name, count), |b| {
                b.iter_batched(
                    || input.clone(),
                    |owned| ValidatedWeights::try_new(black_box(owned)),
                    BatchSize::SmallInput,
                );
            });
        }
        for (name, input) in [("named_valid", &named), ("named_invalid", &bad_named)] {
            group.bench_function(BenchmarkId::new(name, count), |b| {
                b.iter_batched(
                    || input.clone(),
                    |owned| NamedWeights::try_new(black_box(owned)),
                    BatchSize::SmallInput,
                );
            });
        }
    }
    group.finish();
}

fn weighted_mean_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("named_weighted_mean");
    for (count, pair, expected) in [
        (2usize, [0.25, 0.625], 0.5_f64),
        (8, [0.0, 0.375], 0.25),
        (32, [0.5, 0.875], 0.75),
    ] {
        let configured: HashMap<usize, f64> = (0..count)
            .map(|key| (key, if key % 2 == 0 { 1.0 } else { 2.0 }))
            .collect();
        let weights = NamedWeights::try_new(configured).expect("valid configured weights");
        let features: Vec<_> = (0..count).map(|key| (key, pair[key % 2])).collect();
        // Each weight pair is (1, 2); its exact mean is respectively 1/2, 1/4, 3/4.
        // Max-scaled weights, feature products and accumulated sums are dyadic here.
        assert_eq!(
            weights
                .weighted_mean(&features)
                .expect("finite unique keyed features")
                .to_bits(),
            expected.to_bits()
        );
        // Time the whole method, including its duplicate-check HashSet and validation.
        group.bench_function(BenchmarkId::new("keys", count), |b| {
            b.iter(|| black_box(black_box(&weights).weighted_mean(black_box(features.as_slice()))));
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20).warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3));
    targets = strict_cosine, one_based_rrf, decay_paths, band_paths, weight_paths, weighted_mean_paths
}
criterion_main!(benches);
