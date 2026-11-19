use super::*;

fn guarded_refusal_fixture(len: usize, refused_index: usize) -> (Vec<Edge>, GuardedBatchOutcome) {
    let edges: Vec<_> = (0..len)
        .map(|index| Edge {
            id: LinkId(uuid::Uuid::from_u128(index as u128 + 1)),
            namespace: "local".into(),
            source_id: uuid::Uuid::from_u128(1001),
            target_id: uuid::Uuid::from_u128(1002),
            relation: khive_types::EdgeRelation::Extends,
            weight: 1.0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .collect();
    let refusal = GuardedBatchRefusal {
        entry_index: refused_index,
        missing: MissingEndpoints {
            source: false,
            target: true,
        },
    };
    let mut summary = BatchWriteSummary {
        attempted: len as u64,
        first_error: "original guard refusal".into(),
        ..Default::default()
    };
    for (index, edge) in edges.iter().enumerate() {
        refusal.record_failure(&mut summary, index, edge, "original guard refusal");
    }
    (
        edges,
        GuardedBatchOutcome {
            summary,
            refused: Some(refusal),
        },
    )
}

#[test]
fn guarded_refusal_page_formats_only_filtered_page_with_shared_cap() {
    let cap = MAX_BATCH_WRITE_ERROR_DETAILS;
    let (edges, outcome) = guarded_refusal_fixture(cap * 2 + 9, cap + 2);
    for (class, offset, expected_len, expected_total, expected_first) in [
        (None, cap as u64, cap, edges.len(), Some(cap)),
        (
            Some(BatchWriteErrorClass::InvalidInput),
            0,
            1,
            1,
            Some(cap + 2),
        ),
        (
            Some(BatchWriteErrorClass::BatchAborted),
            (cap + 3) as u64,
            cap,
            edges.len() - 1,
            Some(cap + 4),
        ),
        (Some(BatchWriteErrorClass::Conflict), 0, 0, 0, None),
    ] {
        let mut formatted = Vec::new();
        let page = outcome
            .refusal_page_with(
                &edges,
                class,
                PageRequest {
                    offset,
                    limit: u32::MAX,
                },
                |refusal, index, edge, message| {
                    formatted.push(index);
                    refusal.format_failure(index, edge, message)
                },
            )
            .unwrap();
        assert_eq!(page.items.len(), expected_len);
        assert_eq!(formatted.len(), expected_len);
        assert_eq!(formatted.first().copied(), expected_first);
        assert_eq!(page.total, Some(expected_total as u64));
        assert_eq!(
            formatted,
            page.items
                .iter()
                .map(|error| error.index as usize)
                .collect::<Vec<_>>()
        );
        if let Some(class) = class {
            assert!(page.items.iter().all(|error| error.class == class));
        }
    }
}

#[test]
fn guarded_refusal_page_handles_empty_success_and_page_boundaries() {
    let (edges, outcome) = guarded_refusal_fixture(3, 1);
    for page in [
        PageRequest {
            offset: 0,
            limit: 0,
        },
        PageRequest {
            offset: 3,
            limit: 1,
        },
        PageRequest {
            offset: u64::MAX,
            limit: u32::MAX,
        },
    ] {
        let result = outcome
            .refusal_page_with(&edges, None, page, |_, _, _, _| {
                panic!("empty page must not format a detail")
            })
            .unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.total, Some(3));
    }
    let success = GuardedBatchOutcome {
        summary: BatchWriteSummary {
            attempted: 3,
            affected: 3,
            ..Default::default()
        },
        refused: None,
    };
    let page = success
        .refusal_page(&edges, None, PageRequest::default())
        .unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.total, Some(0));
    let empty = GuardedBatchOutcome {
        summary: BatchWriteSummary::default(),
        refused: None,
    };
    let page = empty
        .refusal_page(&[], None, PageRequest::default())
        .unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.total, Some(0));
}

#[test]
fn guarded_refusal_page_rejects_wrong_batch_length_and_refusal_index() {
    let (edges, mut outcome) = guarded_refusal_fixture(3, 1);
    let error = outcome
        .refusal_page(&edges[..2], None, PageRequest::default())
        .unwrap_err();
    assert!(error.to_string().contains("does not match attempted count"));
    outcome.refused.as_mut().unwrap().entry_index = edges.len();
    let error = outcome
        .refusal_page(&edges, None, PageRequest::default())
        .unwrap_err();
    assert!(error.to_string().contains("refusal index 3 is outside"));
    let empty = GuardedBatchOutcome {
        summary: BatchWriteSummary::default(),
        refused: outcome.refused,
    };
    assert!(empty
        .refusal_page(&[], None, PageRequest::default())
        .is_err());
}

#[test]
fn lazy_refusal_formats_only_sampled_details_with_identical_summary() {
    let total = MAX_BATCH_WRITE_ERROR_DETAILS + 7;
    let calls = std::cell::Cell::new(0);
    let mut eager = BatchWriteSummary::default();
    let mut lazy = BatchWriteSummary::default();
    for index in 0..total {
        let class = BatchWriteErrorClass::InvalidInput;
        let retryability = BatchWriteRetryability::Permanent;
        eager.record_failure(
            index,
            Some(index.to_string()),
            class,
            retryability,
            "bad item",
        );
        lazy.record_failure_with(index, class, retryability, || {
            calls.set(calls.get() + 1);
            (Some(index.to_string()), "bad item".into())
        });
    }
    assert_eq!(calls.get(), MAX_BATCH_WRITE_ERROR_DETAILS);
    assert_eq!(
        serde_json::to_value(lazy).unwrap(),
        serde_json::to_value(eager).unwrap()
    );
}

#[test]
fn lazy_refusal_preserves_first_error_after_empty_sample_messages() {
    let mut summary = BatchWriteSummary::default();
    for index in 0..MAX_BATCH_WRITE_ERROR_DETAILS {
        summary.record_failure(
            index,
            None,
            BatchWriteErrorClass::Unknown,
            BatchWriteRetryability::Unknown,
            "",
        );
    }
    summary.record_failure_with(
        MAX_BATCH_WRITE_ERROR_DETAILS,
        BatchWriteErrorClass::Driver,
        BatchWriteRetryability::Unknown,
        || (None, "first nonempty error".into()),
    );
    assert_eq!(summary.first_error, "first nonempty error");
    assert_eq!(summary.errors.len(), MAX_BATCH_WRITE_ERROR_DETAILS);
    assert_eq!(summary.errors_omitted, 1);
}

#[test]
fn refusal_sample_is_bounded_but_counts_cover_the_complete_batch() {
    let total = MAX_BATCH_WRITE_ERROR_DETAILS + 3;
    let mut summary = BatchWriteSummary {
        attempted: total as u64,
        ..BatchWriteSummary::default()
    };

    for index in 0..total {
        summary.record_failure(
            index,
            Some(format!("item-{index}")),
            BatchWriteErrorClass::InvalidInput,
            BatchWriteRetryability::Permanent,
            "invalid item",
        );
    }

    assert_eq!(summary.failed, total as u64);
    assert_eq!(summary.errors.len(), MAX_BATCH_WRITE_ERROR_DETAILS);
    assert!(summary.errors_truncated);
    assert_eq!(summary.errors_omitted, 3);
    assert_eq!(summary.error_counts.len(), 1);
    assert_eq!(summary.error_counts[0].count, total as u64);
    assert_eq!(
        summary.error_counts[0].class,
        BatchWriteErrorClass::InvalidInput
    );
    assert_eq!(
        summary.error_counts[0].retryability,
        BatchWriteRetryability::Permanent
    );
    assert_eq!(
        summary
            .error_counts
            .iter()
            .map(|count| count.count)
            .sum::<u64>(),
        summary.failed
    );
}

#[test]
fn sampled_message_is_bounded_without_changing_legacy_first_error() {
    let message = "x".repeat(MAX_BATCH_WRITE_ERROR_MESSAGE_CHARS + 10);
    let mut summary = BatchWriteSummary::default();

    summary.record_failure(
        0,
        None,
        BatchWriteErrorClass::Driver,
        BatchWriteRetryability::Unknown,
        message.clone(),
    );

    assert_eq!(summary.first_error, message);
    assert_eq!(
        summary.errors[0].message.chars().count(),
        MAX_BATCH_WRITE_ERROR_MESSAGE_CHARS
    );
    assert!(summary.errors[0].message.ends_with('\u{2026}'));
}

#[test]
fn successful_summary_keeps_the_legacy_wire_shape() {
    let summary = BatchWriteSummary {
        attempted: 2,
        affected: 2,
        ..BatchWriteSummary::default()
    };

    let value = serde_json::to_value(summary).expect("serialize summary");
    assert_eq!(
        value,
        serde_json::json!({"attempted": 2, "affected": 2, "failed": 0})
    );
}

#[test]
fn legacy_wire_shape_deserializes_with_empty_refusal_details() {
    let summary: BatchWriteSummary = serde_json::from_value(serde_json::json!({
        "attempted": 4,
        "affected": 3,
        "failed": 1,
        "first_error": "legacy"
    }))
    .expect("deserialize legacy summary");

    assert_eq!(summary.attempted, 4);
    assert_eq!(summary.affected, 3);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.first_error, "legacy");
    assert!(summary.errors.is_empty());
    assert!(summary.error_counts.is_empty());
    assert!(!summary.errors_truncated);
    assert_eq!(summary.errors_omitted, 0);
}
