use super::*;
use std::cell::Cell;
thread_local! { static WORK: Cell<usize> = const { Cell::new(0) }; }
pub(super) fn visit() {
    WORK.with(|work| work.set(work.get() + 1));
}
fn reset() {
    WORK.with(|work| work.set(0));
}
fn work() -> usize {
    WORK.with(Cell::get)
}

fn legacy_flags(prefixes: &[String]) -> Vec<bool> {
    prefixes
        .iter()
        .enumerate()
        .map(|(i, prefix)| prefixes[..i].contains(prefix))
        .collect()
}
#[test]
fn duplicate_prefixes_preserve_first_keeper_and_score_bits() {
    for prefixes in [
        vec![],
        vec![""],
        vec!["a", "a", "b", "a", "b"],
        vec!["中", "中", "中文", ""],
        vec!["é", "e\u{301}", "é"],
    ] {
        let prefixes: Vec<String> = prefixes.into_iter().map(str::to_string).collect();
        let actual: Vec<bool> = duplicate_prefix_flags(&prefixes).collect();
        assert_eq!(actual, legacy_flags(&prefixes));
        for penalty in [0.01f32, 0.8, f32::INFINITY] {
            let scores = [0.0f32, 0.9, 0.1, -0.0, f32::EPSILON];
            let adjust = |flags: Vec<bool>| {
                flags
                    .into_iter()
                    .enumerate()
                    .map(|(i, duplicate)| {
                        let score = scores[i % scores.len()];
                        if duplicate {
                            (score - penalty).max(0.0).to_bits()
                        } else {
                            score.to_bits()
                        }
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(adjust(actual.clone()), adjust(legacy_flags(&prefixes)));
        }
    }
}
#[test]
fn duplicate_prefixes_visit_each_prefix_once() {
    for n in [8, 64, 500] {
        for duplicate in [false, true] {
            let prefixes: Vec<String> = (0..n)
                .map(|i| {
                    if duplicate {
                        "same".into()
                    } else {
                        format!("unique-{i}")
                    }
                })
                .collect();
            reset();
            let flags: Vec<bool> = duplicate_prefix_flags(&prefixes).collect();
            assert_eq!(work(), n, "one hash-set insertion per admitted prefix");
            assert_eq!(flags, legacy_flags(&prefixes));
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn recall_pipeline_visits_each_admitted_prefix_once() {
    use khive_pack_kg::KgPack;
    use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
    use serde_json::json;
    for n in [8, 32] {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(crate::MemoryPack::new(runtime));
        let registry = builder.build().expect("registry");
        for index in 0..n {
            registry
                .dispatch(
                    "memory.remember",
                    json!({
                        "content": format!("unique-{index} needle observation for prefix work"),
                        "memory_type": "semantic", "salience": 0.5, "decay": 0.0
                    }),
                )
                .await
                .expect("real remembered note");
        }
        reset();
        let result = registry.dispatch("memory.recall", json!({
            "query": "needle", "limit": n, "min_score": 0.0,
            "entity_names": [],
            "config": {"scoring": {"adjustments": [], "mmr_penalty": 0.01, "mmr_prefix_len": 100}}
        })).await.expect("real recall");
        assert_eq!(
            result.as_array().expect("recall array").len(),
            n,
            "all real notes must reach the response before checking work"
        );
        assert_eq!(
            work(),
            n,
            "the live recall path must visit one prefix per admitted note"
        );
    }
}
