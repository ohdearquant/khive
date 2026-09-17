use super::*;

fn budget_surface() -> (KhiveRuntime, VerbRegistry) {
    let (rt, registry) = surface();
    rt.install_kind_registry(
        registry
            .all_entity_kinds()
            .into_iter()
            .map(str::to_string)
            .collect(),
        registry
            .all_note_kinds()
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    (rt, registry)
}

fn missing_fences(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| json!({"key":format!("budget/missing/{index}"),"kind":"head","expected_version":null}))
        .collect()
}

fn guarded_appends(members: usize, entries: usize) -> Vec<Value> {
    let fences = missing_fences(entries);
    (0..members)
        .map(|index| json!({"op":"append","stream":"budget","record":index,"embed":false,"fence":fences}))
        .collect()
}

fn object_guarded_appends(members: usize) -> Vec<Value> {
    let fence = missing_fences(1).remove(0);
    (0..members)
        .map(|index| json!({"op":"append","stream":"budget","record":index,"embed":false,"fence":fence}))
        .collect()
}

fn budget_refusal(error: RuntimeError, total: usize) {
    let RuntimeError::InvalidInput(message) = error else {
        panic!("aggregate fence budget must refuse invalid_input: {error:?}")
    };
    assert!(
        message.contains("at most 100 total fence entries"),
        "{message}"
    );
    assert!(message.contains(&format!("sent {total}:")), "{message}");
    assert!(
        message.contains("each fence is a read taken while holding the writer"),
        "{message}"
    );
}

#[tokio::test]
async fn aggregate_fence_budget_accepts_exact_cap_in_both_modes() {
    for atomic in [true, false] {
        // Repeated keys across members still produce separate fence reads.
        for ops in [guarded_appends(2, 50), object_guarded_appends(100)] {
            let (_, registry) = budget_surface();
            let members = ops.len();
            let result = registry
                .dispatch("stream.batch", json!({"atomic":atomic,"ops":ops}))
                .await
                .unwrap();
            assert_eq!(result["committed"], true);
            assert_eq!(result["results"].as_array().unwrap().len(), members);
            assert_eq!(seqs(&result), (1..=members as i64).collect::<Vec<_>>());
            assert_eq!(heads(&registry, &["budget"]).await, vec![members as i64]);
        }
    }
}

#[tokio::test]
async fn aggregate_fence_budget_counts_batch_guard_and_keeps_observed_independent() {
    let (_, registry) = budget_surface();
    let observed: Vec<_> = (0..100)
        .map(|index| json!({"key":format!("budget/observed/{index}"),"kind":"head","version":null}))
        .collect();
    let batch_guard = missing_fences(1).remove(0);
    let result = registry
        .dispatch(
            "stream.batch",
            json!({
                "fence":batch_guard,"observed":observed,"ops":guarded_appends(1, 99)
            }),
        )
        .await
        .unwrap();
    assert_eq!(seqs(&result), vec![1]);
    budget_refusal(
        registry
            .dispatch(
                "stream.batch",
                json!({
                    "fence":batch_guard,"observed":observed,"ops":guarded_appends(1, 100)
                }),
            )
            .await
            .unwrap_err(),
        101,
    );
    assert_eq!(heads(&registry, &["budget"]).await, vec![1]);

    // Null keeps the existing no-batch-fence mode/default and costs no entry.
    let result = registry
        .dispatch(
            "stream.batch",
            json!({
                "fence":null,"ops":guarded_appends(1, 100)
            }),
        )
        .await
        .unwrap();
    assert_eq!(seqs(&result), vec![2]);
}

#[tokio::test]
async fn aggregate_fence_budget_refuses_one_over_in_both_modes() {
    for atomic in [true, false] {
        let mut listed = guarded_appends(2, 50);
        listed[1]["fence"] = json!(missing_fences(51));
        for ops in [listed, object_guarded_appends(101)] {
            let (rt, registry) = budget_surface();
            let before = population(&rt).await;
            budget_refusal(
                registry
                    .dispatch("stream.batch", json!({"atomic":atomic,"ops":ops}))
                    .await
                    .unwrap_err(),
                101,
            );
            assert_eq!(population(&rt).await, before);
            assert_eq!(heads(&registry, &["budget"]).await, vec![0]);
        }
    }
}

#[tokio::test]
async fn aggregate_fence_budget_refuses_1000_members_with_100_fences_each() {
    for atomic in [true, false] {
        let (rt, registry) = budget_surface();
        let before = population(&rt).await;
        budget_refusal(
            registry
                .dispatch(
                    "stream.batch",
                    json!({
                        "atomic":atomic,"ops":guarded_appends(1000, 100)
                    }),
                )
                .await
                .unwrap_err(),
            100_000,
        );
        assert_eq!(population(&rt).await, before);
        assert_eq!(heads(&registry, &["budget"]).await, vec![0]);
    }
}

#[tokio::test]
async fn aggregate_fence_budget_preserves_more_specific_input_errors() {
    for atomic in [true, false] {
        let (_, registry) = budget_surface();
        for (bad_member, expected) in [
            (
                json!({"op":"append","stream":"budget","record":3,"fence":vec![Value::Null; 101]}),
                "fence list admits at most 100 entries",
            ),
            (
                json!({"op":"append","stream":"x".repeat(513),"record":3}),
                "stream must be at most 512 UTF-8 bytes",
            ),
            (
                json!({"op":"append","stream":"budget","record":3,"extra":true}),
                "unknown field `extra`",
            ),
        ] {
            let mut ops = guarded_appends(2, 51);
            ops.push(bad_member);
            let error = registry
                .dispatch("stream.batch", json!({"atomic":atomic,"ops":ops}))
                .await
                .unwrap_err();
            let RuntimeError::InvalidInput(message) = error else {
                panic!("{error:?}")
            };
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("total fence entries"), "{message}");
        }
    }
}

#[tokio::test]
async fn aggregate_fence_budget_refuses_before_writer_admission() {
    for atomic in [true, false] {
        let (rt, registry) = budget_surface();
        let before_success = rt.backend().pool().writer_acquisition_snapshot();
        registry
            .dispatch(
                "stream.batch",
                json!({
                    "atomic":atomic,"ops":guarded_appends(2, 50)
                }),
            )
            .await
            .unwrap();
        assert!(
            rt.backend()
                .pool()
                .writer_acquisition_snapshot()
                .acquisitions
                > before_success.acquisitions,
            "the positive control must observe real writer admission"
        );

        let before = population(&rt).await;
        let before_writers = rt.backend().pool().writer_acquisition_snapshot();
        let mut ops = guarded_appends(2, 50);
        ops[1]["fence"] = json!(missing_fences(51));
        budget_refusal(
            registry
                .dispatch("stream.batch", json!({"atomic":atomic,"ops":ops}))
                .await
                .unwrap_err(),
            101,
        );
        // Unchanged rows alone cannot distinguish admission from rollback.
        assert_eq!(
            rt.backend().pool().writer_acquisition_snapshot(),
            before_writers,
            "aggregate refusal must precede writer admission (atomic={atomic})"
        );
        assert_eq!(population(&rt).await, before);
        assert_eq!(heads(&registry, &["budget"]).await, vec![2]);
    }
}

#[tokio::test]
async fn aggregate_fence_budget_help_names_bound_and_counting_scope() {
    let (_, registry) = budget_surface();
    let help = registry
        .dispatch("stream.batch", json!({"help":true}))
        .await
        .unwrap();
    let text = help["description"].as_str().unwrap();
    for required in [
        "100 total fence entries",
        "batch-wide fence",
        "append-member",
        "both modes",
        "before preparation",
        "invalid_input",
        "1000 members",
        "100 observed",
    ] {
        assert!(text.contains(required), "missing {required}: {text}");
    }
}

#[tokio::test]
async fn aggregate_fence_budget_pins_preparation_error_precedence() {
    for atomic in [true, false] {
        let (rt, registry) = budget_surface();
        let before = population(&rt).await;
        let invalid_write = json!({
            "op":"write","key":"budget/invalid-kind","kind":"head",
            "doc":{},"tags":[format!("kind:{}", "x".repeat(65))],"embed":false
        });
        let mut at_cap = guarded_appends(2, 50);
        at_cap.push(invalid_write);
        let mut over_cap = at_cap.clone();
        over_cap[1]["fence"] = json!(missing_fences(51));

        // The same valid-shaped member reaches preparation at the exact cap.
        let error = registry
            .dispatch("stream.batch", json!({"atomic":atomic,"ops":at_cap}))
            .await
            .unwrap_err();
        let RuntimeError::InvalidInput(message) = error else {
            panic!("preparation must retain its invalid_input: {error:?}")
        };
        assert_eq!(
            message,
            "head document kind must be at most 64 bytes without U+0000"
        );
        assert_eq!(population(&rt).await, before);
        assert_eq!(heads(&registry, &["budget"]).await, vec![0]);

        // Over budget, admission refuses before evaluating that member's tag.
        budget_refusal(
            registry
                .dispatch("stream.batch", json!({"atomic":atomic,"ops":over_cap}))
                .await
                .unwrap_err(),
            101,
        );
        assert_eq!(population(&rt).await, before);
        assert_eq!(heads(&registry, &["budget"]).await, vec![0]);
    }
}
