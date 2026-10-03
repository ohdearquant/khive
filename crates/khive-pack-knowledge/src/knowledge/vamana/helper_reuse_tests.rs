use super::*;

fn original_pack_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn original_pack_threshold() -> f64 {
    std::env::var("KHIVE_ANN_REBUILD_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0 && *v <= 1.0)
        .unwrap_or(0.20)
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn hit_bits(hits: &[(Uuid, f32)]) -> Vec<(Uuid, u32)> {
    hits.iter()
        .map(|(id, score)| (*id, score.to_bits()))
        .collect()
}

#[test]
fn knowledge_bridge_build_search_and_replay_keep_original_bits() {
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    let vectors = vec![4096.0_f32, 1.0, 1.0, 1.0, 1.0, 3.0, 4.0, 0.0, 0.0, 0.0];
    let mut expected = vectors.clone();
    for row in expected.chunks_exact_mut(5) {
        original_pack_normalize(row);
    }
    let mut bridge = AnnBridge::build(vectors, 5, vec![first, second]).expect("real bridge");
    let built_bits = bits(bridge.index.vectors().expect("vectors"));
    assert_eq!(built_bits, bits(&expected));

    let query = [4096.0_f32, 1.0, 1.0, 1.0, 1.0];
    let mut old_query = query;
    original_pack_normalize(&mut old_query);
    let expected_hits: Vec<_> = bridge
        .index
        .search(&old_query, 2)
        .expect("original normalized query")
        .into_iter()
        .map(|(ordinal, distance)| {
            (
                bridge.id_map[ordinal as usize],
                (1.0 - distance / 2.0).max(0.0),
            )
        })
        .collect();
    let actual_hits = bridge.search(&query, 2);
    assert_eq!(hit_bits(&actual_hits), hit_bits(&expected_hits));
    assert_eq!(expected_hits.len(), 2, "nonempty actual search premise");

    let mut reverse = bridge.reverse_map_for([first, second]);
    let input = vec![4096.0_f32, 1.0, 1.0, 1.0, 1.0];
    let mut expected_insert = input.clone();
    original_pack_normalize(&mut expected_insert);
    bridge
        .apply_final_op(&mut reverse, second, Some(input))
        .expect("replay upsert");
    let ordinal = *reverse.get(&second).expect("replacement ordinal") as usize;
    let stored = &bridge.index.vectors().expect("replay vectors")[ordinal * 5..(ordinal + 1) * 5];
    let replay_bits = bits(stored);
    assert_eq!(replay_bits, bits(&expected_insert));
    assert!(!bridge.index.is_tombstoned(ordinal as u32));
    assert_eq!(bridge.id_map[ordinal], second);
    bridge
        .apply_final_op(&mut reverse, second, None)
        .expect("replay delete");
    assert!(bridge.index.is_tombstoned(ordinal as u32));
    assert!(!reverse.contains_key(&second));
    assert!(!bridge.search(&query, 2).iter().any(|(id, _)| *id == second));
    println!(
        "ANN_HELPER_BITS {}",
        serde_json::json!({"build":built_bits,"replay":replay_bits,"search":hit_bits(&actual_hits)})
    );
}

#[test]
fn knowledge_bridge_and_exact_tail_keep_original_refusals_and_scores() {
    let id = Uuid::from_u128(1);
    assert_eq!(
        AnnBridge::build(vec![1.0], 0, vec![id])
            .err()
            .expect("zero dim"),
        "dimension must be > 0"
    );
    assert_eq!(
        AnnBridge::build(Vec::new(), 1, vec![id])
            .err()
            .expect("empty"),
        "no vectors to build ANN index from"
    );
    assert_eq!(
        AnnBridge::build(vec![1.0, 0.0], 1, vec![id])
            .err()
            .expect("id count"),
        "id_map length 1 != vector count 2"
    );
    let bridge = AnnBridge::build(vec![3.0, 4.0], 2, vec![id]).expect("real bridge");
    assert!(
        bridge.search(&[1.0], 1).is_empty(),
        "dimension refusal stays empty hits"
    );
    assert!(
        bridge.search(&[f32::NAN, 1.0], 1).is_empty(),
        "nonfinite refusal stays empty hits"
    );
    let mut expected_bad = [f32::INFINITY, 1.0];
    original_pack_normalize(&mut expected_bad);
    let expected_error = VamanaIndex::build(&expected_bad, VamanaConfig::with_dimensions(2))
        .expect_err("bad original index")
        .to_string();
    assert_eq!(
        AnnBridge::build(vec![f32::INFINITY, 1.0], 2, vec![id])
            .err()
            .expect("bad bridge"),
        expected_error
    );

    let pairs = [
        (
            vec![4096.0, 1.0, 1.0, 1.0, 1.0],
            vec![3.0, 4.0, 0.0, 0.0, 0.0],
        ),
        (vec![1e-8], vec![1e-8]),
        (vec![-3.0, 4.0], vec![3.0, 4.0]),
        (vec![f32::INFINITY, 1.0], vec![3.0, 4.0]),
        (vec![f32::from_bits(0x7fc1_2345)], vec![1.0]),
    ];
    let mut receipt = Vec::new();
    for (query, embedding) in pairs {
        let mut old_query = query.clone();
        let mut old_embedding = embedding.clone();
        original_pack_normalize(&mut old_query);
        original_pack_normalize(&mut old_embedding);
        let expected = old_query
            .iter()
            .zip(&old_embedding)
            .map(|(a, b)| a * b)
            .sum::<f32>()
            .max(0.0);
        let actual = exact_cosine(&query, &embedding);
        assert_eq!(actual.to_bits(), expected.to_bits());
        receipt.push(actual.to_bits());
    }
    assert_eq!(exact_cosine(&[], &[]), 0.0);
    assert_eq!(exact_cosine(&[1.0], &[1.0, 2.0]), 0.0);
    println!(
        "ANN_HELPER_REFUSALS {}",
        serde_json::json!({"nonfinite":expected_error,"exact_scores":receipt})
    );
}

const CASE_KEY: &str = "KHIVE_KNOWLEDGE_HELPER_TEST_CASE";
const ENV_KEY: &str = "KHIVE_ANN_REBUILD_THRESHOLD";
const CHILD: &str = "knowledge::vamana::tests::helper_reuse::knowledge_fresh_tail_threshold_child";

#[test]
fn knowledge_fresh_tail_consumer_keeps_default_range_trim_and_sampling() {
    for (case, value) in [
        ("default", None),
        ("default", Some("invalid")),
        ("default", Some(" 0.21 ")),
        ("default", Some("0")),
        ("default", Some("1.1")),
        ("two", Some("0.21")),
        ("all", Some("1")),
        ("changes", Some("0.21")),
    ] {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", CHILD, "--nocapture"])
            .env(CASE_KEY, case)
            .env_remove(ENV_KEY);
        if let Some(value) = value {
            command.env(ENV_KEY, value);
        }
        let output = command.output().expect("knowledge threshold child");
        assert!(
            output.status.success(),
            "case={case} value={value:?}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("KNOWLEDGE_THRESHOLD_CASE_COMPLETED"));
        for line in stdout
            .lines()
            .filter(|line| line.starts_with("KNOWLEDGE_TAIL_RECEIPT "))
        {
            println!("KNOWLEDGE_PARENT_RECEIPT case={case} value={value:?} {line}");
        }
    }
}

async fn assert_actual_tail(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    expected_count: usize,
) {
    let threshold = original_pack_threshold();
    let expected = fetch_fresh_tail_snapshot(rt, "local", WARM_TEST_MODEL, 0, Some(threshold))
        .await
        .expect("original threshold snapshot");
    assert_eq!(expected.live_count, Some(5), "nonzero real vector corpus");
    assert_eq!(expected.ops.len(), expected_count, "original cap premise");
    assert_eq!(
        ready_snapshot_watermark(&expected),
        Some(0),
        "real active consumer"
    );
    let actual = match fresh_tail_capped(rt, ann, key).await {
        FreshTailOutcome::Ops(ops) => ops,
        FreshTailOutcome::Replace { .. } => panic!("unexpected replacement"),
        FreshTailOutcome::Skipped => panic!("unexpected skipped real SQL read"),
    };
    let as_bits = |ops: &[(Uuid, Option<Vec<f32>>)]| {
        ops.iter()
            .map(|(id, v)| (*id, v.as_deref().map(bits)))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        as_bits(&actual),
        as_bits(&expected.ops),
        "actual production cap, row order and vector bytes"
    );
    println!(
        "KNOWLEDGE_TAIL_RECEIPT count={expected_count} threshold_bits={}",
        threshold.to_bits()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn knowledge_fresh_tail_threshold_child() {
    let Ok(case) = std::env::var(CASE_KEY) else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("threshold.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    seed_warm_corpus(&rt, &token, 5).await;
    register_consumer(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("register actual consumer");
    assert!(ann_registry::raise_watermark(
        rt.sql().as_ref(),
        ANN_CONSUMER,
        "local",
        WARM_TEST_MODEL,
        0,
        WatermarkAuthority::PendingOrActive
    )
    .await
    .expect("activate actual watermark"));
    let ann = new_shared_for_role(false);
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    match case.as_str() {
        "default" => assert_actual_tail(&rt, &ann, &key, 1).await,
        "two" => assert_actual_tail(&rt, &ann, &key, 2).await,
        "all" => assert_actual_tail(&rt, &ann, &key, 5).await,
        "changes" => {
            assert_actual_tail(&rt, &ann, &key, 2).await;
            // Only this exact child test runs in this process.
            std::env::set_var(ENV_KEY, "0.80");
            assert_actual_tail(&rt, &ann, &key, 4).await;
            std::env::set_var(ENV_KEY, "invalid");
            assert_actual_tail(&rt, &ann, &key, 1).await;
            std::env::remove_var(ENV_KEY);
        }
        other => panic!("unknown knowledge threshold case {other}"),
    }
    println!("KNOWLEDGE_THRESHOLD_CASE_COMPLETED {case}");
}
