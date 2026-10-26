const MODEL_KEYS: [&str; 4] = ["model.k0", "model.k1", "model.k2", "model.k3"];

#[derive(Clone, Copy, Debug)]
enum ModelOp {
    Observe(usize, usize),
    Drain(usize),
}

fn model_schedule(mut seed: u64) -> Vec<ModelOp> {
    use ModelOp::{Drain, Observe};
    let mut operations = vec![
        Observe(0, 0),
        Observe(1, 0),
        Observe(1, 1),
        Drain(0),
        Drain(0),
        Drain(1),
        Observe(0, 2),
        Observe(1, 3),
        Observe(1, 2),
        Drain(0),
        Drain(0),
        Drain(1),
    ];
    for _ in 0..240 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let choice = ((seed >> 32) % 10) as usize;
        operations.push(match choice {
            0..=3 => Observe(0, choice),
            4..=7 => Observe(1, choice - 4),
            8 => Drain(0),
            _ => Drain(1),
        });
    }
    operations.extend([Observe(0, 1), Observe(0, 3), Drain(0), Drain(1)]);
    operations
}

async fn run_model(seed: u64) {
    let backend = prepared_backend();
    let d = Fixture::from_runtime(
        runtime_on(backend.clone(), "model-d"),
        "local",
        "same-backend-model",
    );
    let e = Fixture::from_runtime(
        runtime_on(backend, "model-e"),
        "local",
        "same-backend-model",
    );
    let owners = [&d, &e];
    let mut seen = [[false; 4]; 2];
    let mut pending = [[false; 4]; 2];
    let mut delivered = [0usize; 4];
    let operations = model_schedule(seed);
    assert_eq!(
        operations.len(),
        256,
        "each fixed seed exercises at least 200 operations"
    );
    for (step, operation) in operations.iter().copied().enumerate() {
        match operation {
            ModelOp::Observe(owner, key) => {
                owners[owner]
                    .runtime
                    .record_config_locked(MODEL_KEYS[key], || "locked".into());
                if !seen[owner][key] {
                    seen[owner][key] = true;
                    pending[owner][key] = true;
                }
            }
            ModelOp::Drain(owner) => {
                owners[owner].dispatch().await;
                for (pending, delivered) in pending[owner].iter_mut().zip(&mut delivered) {
                    if *pending {
                        *delivered += 1;
                        *pending = false;
                    }
                }
                let rows = owners[owner].rows().await;
                assert!(
                    rows.iter()
                        .all(|row| row.namespace == "local"
                            && row.actor == "agent:same-backend-model"),
                    "same backend pair uses identical actual dispatch identity"
                );
                let actual = MODEL_KEYS
                    .map(|key| rows.iter().filter(|row| row.payload["key"] == key).count());
                assert_eq!(
                    actual,
                    delivered,
                    "seed={seed:#x}, zero-based step={step}, op={operation:?}, prefix={:?}",
                    &operations[..=step]
                );
                assert_eq!(
                    rows.len(),
                    delivered.iter().sum::<usize>(),
                    "no unrelated config rows"
                );
                assert!(rows.iter().all(|row| row.payload["value"] == "locked"));
                assert_eq!(
                    rows.iter().map(|row| row.id).collect::<BTreeSet<_>>().len(),
                    rows.len()
                );
            }
        }
    }
    assert_eq!(delivered, [2; 4]);
    for fixture in owners {
        for key in MODEL_KEYS {
            assert_eq!(
                fixture.store.submitted(key).len(),
                1,
                "one submission per owner and name"
            );
            fixture
                .runtime
                .record_config_locked(key, || panic!("model name already observed"));
        }
        fixture.dispatch().await;
        assert_eq!(fixture.rows().await.len(), 8);
        fixture.finish().await;
    }
}

#[tokio::test]
#[serial(config_ledger)]
async fn same_backend_model_seed_5901d3e320260001_matches_every_completed_drain() {
    run_model(0x5901_d3e3_2026_0001).await;
}

#[tokio::test]
#[serial(config_ledger)]
async fn same_backend_model_seed_5901d3e320260002_matches_every_completed_drain() {
    run_model(0x5901_d3e3_2026_0002).await;
}
