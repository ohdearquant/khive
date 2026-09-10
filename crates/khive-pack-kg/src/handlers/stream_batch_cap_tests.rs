//! ADR-174 A7: the batch admits a bounded list.
use super::*;

use crate::handlers::stream::{MAX_BATCH_MEMBERS, MAX_BATCH_OBSERVED};

fn appends(count: usize) -> Vec<Value> {
    (0..count)
        .map(|n| json!({"op":"append","stream":"cap/a","record":n}))
        .collect()
}

fn invalid_input(error: RuntimeError) -> String {
    match error {
        RuntimeError::InvalidInput(message) => message,
        other => panic!("invalid_input, got {other:?}"),
    }
}

#[tokio::test]
async fn cap_arm1_a_list_at_the_member_cap_commits() {
    let (_, reg) = surface();
    let result = reg
        .dispatch(
            "stream.batch",
            json!({"atomic":true,"ops":appends(MAX_BATCH_MEMBERS)}),
        )
        .await
        .unwrap();
    assert_eq!(result["committed"], true);
    assert_eq!(
        result["results"].as_array().unwrap().len(),
        MAX_BATCH_MEMBERS
    );
    assert_eq!(result["results"][MAX_BATCH_MEMBERS - 1]["seq"], 1000);
    assert_eq!(heads(&reg, &["cap/a"]).await, vec![1000]);
}

#[tokio::test]
async fn cap_arm1_b_the_member_cap_and_the_observation_cap_hold_together() {
    let (_, reg) = surface();
    let mut observed = Vec::with_capacity(MAX_BATCH_OBSERVED);
    for n in 0..MAX_BATCH_OBSERVED {
        let key = format!("cap-key-{n}");
        lease(&reg, &key).await;
        observed.push(json!({"key":key,"kind":"head","version":1}));
    }
    let result = reg
        .dispatch(
            "stream.batch",
            json!({"atomic":true,"observed":observed,"ops":appends(MAX_BATCH_MEMBERS)}),
        )
        .await
        .unwrap();
    assert_eq!(result["committed"], true);
    assert_eq!(heads(&reg, &["cap/a"]).await, vec![1000]);
}

#[tokio::test]
async fn cap_arm2_one_member_over_the_cap_refuses_naming_the_cap_and_the_count() {
    let (rt, reg) = surface();
    let before = population(&rt).await;
    let error = reg
        .dispatch(
            "stream.batch",
            json!({"atomic":true,"ops":appends(MAX_BATCH_MEMBERS + 1)}),
        )
        .await
        .unwrap_err();
    let message = invalid_input(error);
    assert!(message.contains("1001"), "{message}");
    assert!(message.contains("1000"), "{message}");
    // The refusal preceded the writer: the head never moved and no row landed.
    assert_eq!(heads(&reg, &["cap/a"]).await, vec![0]);
    assert_eq!(population(&rt).await, before);
}

#[tokio::test]
async fn cap_arm3_one_observation_over_the_cap_refuses_on_its_own_number() {
    let (rt, reg) = surface();
    let before = population(&rt).await;
    let observed: Vec<Value> = (0..MAX_BATCH_OBSERVED + 1)
        .map(|n| json!({"key":format!("cap-key-{n}"),"kind":"head","version":1}))
        .collect();
    let error = reg
        .dispatch(
            "stream.batch",
            json!({"atomic":true,"observed":observed,"ops":appends(3)}),
        )
        .await
        .unwrap_err();
    let message = invalid_input(error);
    assert!(message.contains("101"), "{message}");
    assert!(message.contains("100"), "{message}");
    assert!(message.contains("observed"), "{message}");
    assert_eq!(population(&rt).await, before);
}

#[tokio::test]
async fn cap_arm4_per_member_mode_refuses_the_same_list_the_same_way() {
    let (rt, reg) = surface();
    let before = population(&rt).await;
    let error = reg
        .dispatch(
            "stream.batch",
            json!({"atomic":false,"ops":appends(MAX_BATCH_MEMBERS + 1)}),
        )
        .await
        .unwrap_err();
    let message = invalid_input(error);
    assert!(message.contains("1001"), "{message}");
    assert!(message.contains("1000"), "{message}");
    // No member ran, so there are no per-member values to inspect and nothing
    // in the store: the mode that could have absorbed the list still refuses it.
    assert_eq!(heads(&reg, &["cap/a"]).await, vec![0]);
    assert_eq!(population(&rt).await, before);
}

#[tokio::test]
async fn cap_arm5_the_cap_is_read_before_any_member_is_interpreted() {
    let (_, reg) = surface();
    // Every member here is independently invalid: no op string at all. If the
    // cap were checked after members were parsed, this would refuse on member 0.
    let ops: Vec<Value> = (0..MAX_BATCH_MEMBERS + 1)
        .map(|n| json!({"not_an_op":n}))
        .collect();
    let message = invalid_input(
        reg.dispatch("stream.batch", json!({"atomic":true,"ops":ops}))
            .await
            .unwrap_err(),
    );
    assert!(message.contains("1001"), "{message}");
    assert!(!message.contains("member 0"), "{message}");
}

#[tokio::test]
async fn cap_arm6_help_names_both_caps() {
    let (_, reg) = surface();
    let help = reg
        .dispatch("stream.batch", json!({"help":true}))
        .await
        .unwrap();
    let text = help["description"].as_str().unwrap();
    for required in ["1000 members", "100 observed", "invalid_input", "both modes"] {
        assert!(text.contains(required), "missing {required}: {help}");
    }
}

/// Writer hold time for one atomic batch, measured with the store's own
/// open-transaction registry rather than with wall time around the dispatch,
/// so preparation before the writer is not counted as hold.
///
/// Ignored: it is a measurement, not an assertion. Run it as
/// `KHIVE_CAP_BENCH_MEMBERS=1000 cargo test -p khive-pack-kg --lib
/// cap_measure_writer_hold -- --ignored --nocapture`. The count above the cap
/// is measured with the cap constant raised, which is the only way to observe
/// the hold this amendment exists to prevent.
#[tokio::test]
#[ignore]
async fn cap_measure_writer_hold() {
    use khive_runtime::RuntimeConfig;
    use std::time::{Duration, Instant};

    let members: usize = std::env::var("KHIVE_CAP_BENCH_MEMBERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_BATCH_MEMBERS);
    let root = std::env::temp_dir().join(format!("khive-cap-bench-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(root.join("khive.db")),
        ..Default::default()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    let reg = builder.build().unwrap();

    let ops = appends(members);
    let peak_micros = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let sampler = peak_micros.clone();
    let watcher = tokio::spawn(async move {
        loop {
            if let Some((_, age, _)) = khive_storage::tx_registry::oldest() {
                sampler.fetch_max(age.as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let started = Instant::now();
    let result = reg
        .dispatch("stream.batch", json!({"atomic":true,"ops":ops}))
        .await;
    let dispatch = started.elapsed();
    watcher.abort();
    let peak = Duration::from_micros(peak_micros.load(std::sync::atomic::Ordering::Relaxed));
    let committed = result.as_ref().map(|v| v["committed"].clone());
    println!(
        "cap-bench members={members} dispatch={dispatch:?} peak_writer_hold={peak:?} committed={committed:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
    result.unwrap();
}
