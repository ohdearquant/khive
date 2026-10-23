//! Public request planning, static admission, typed input, and parser scale.

mod pipeline_support;

use std::cell::OnceCell;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use khive_request::{
    check_atomic_admissible, parse_request, parse_typed_json_batch, plan_request, ExecutionMode,
    MAX_OPS,
};
use pipeline_support::{fixtures, scene};

fn pipeline(c: &mut Criterion) {
    assert_eq!(MAX_OPS, 100, "scenario labels track the public cap");
    let mut group = c.benchmark_group("request_pipeline");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    let catalog = fixtures::catalog();
    let planning = [
        (
            "single/simple",
            fixtures::SINGLE_SIMPLE.to_owned(),
            1,
            ExecutionMode::Single,
        ),
        (
            "single/complex",
            fixtures::SINGLE_COMPLEX.to_owned(),
            1,
            ExecutionMode::Single,
        ),
        (
            "batch/3",
            fixtures::BATCH_3.to_owned(),
            3,
            ExecutionMode::Parallel,
        ),
        (
            "batch/10",
            fixtures::make_batch(10),
            10,
            ExecutionMode::Parallel,
        ),
        (
            "chain/2",
            fixtures::CHAIN_2.to_owned(),
            2,
            ExecutionMode::Chain,
        ),
        ("chain/5", fixtures::make_chain(5), 5, ExecutionMode::Chain),
    ];
    for (name, input, count, mode) in planning {
        let parsed = OnceCell::new();
        scene(
            &mut group,
            &format!("plan/{name}"),
            || input.as_str(),
            |text| plan_request(text, black_box(&catalog)),
            |plan| {
                let parsed = parsed.get_or_init(|| {
                    let parsed = parse_request(&input).expect("legacy planning input");
                    assert_eq!(parsed.ops.len(), count);
                    assert_eq!(parsed.mode, mode);
                    parsed
                });
                fixtures::check_plan(plan, parsed, &catalog);
            },
        );
    }
    for count in [10, 50, 100] {
        for class in ["eligible", "mixed", "rejected"] {
            let (ops, expected) = fixtures::atomic_batch(class, count);
            scene(
                &mut group,
                &format!("atomic/{class}/{count}"),
                || ops.as_slice(),
                check_atomic_admissible,
                |rejections| assert_eq!(rejections, &expected),
            );
        }
        let (ops, expected) = fixtures::typed_batch(count);
        scene(
            &mut group,
            &format!("typed_json/{count}"),
            || ops.clone(),
            parse_typed_json_batch,
            |result| {
                assert_eq!(
                    result.as_ref().expect("typed input must succeed"),
                    &expected
                )
            },
        );
    }
    for count in [50, 100] {
        let input = fixtures::make_batch(count);
        let expected = fixtures::expected_batch(count);
        scene(
            &mut group,
            &format!("parse_batch/{count}"),
            || input.as_str(),
            parse_request,
            |result| assert_eq!(result.as_ref().expect("scaled batch must parse"), &expected),
        );
    }
    for count in [10, 20] {
        let input = fixtures::make_chain(count);
        let expected = fixtures::expected_chain(count);
        scene(
            &mut group,
            &format!("parse_chain/{count}"),
            || input.as_str(),
            parse_request,
            |result| assert_eq!(result.as_ref().expect("scaled chain must parse"), &expected),
        );
    }
    group.finish();
}

criterion_group!(benches, pipeline);
criterion_main!(benches);
