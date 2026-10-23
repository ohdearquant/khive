use super::{add_post_commit_embedding_warning, validate_atomic_args, PostCommitEffect, Uuid};
use serde_json::json;

#[test]
fn atomic_update_result_uses_matching_post_commit_truncation_outcome() {
    let note_id = Uuid::new_v4();
    let effect = PostCommitEffect::ReindexNote {
        note_id,
        version: 2,
    };
    let outcomes = vec![khive_runtime::atomic_prepare::PostCommitEmbeddingOutcome {
        effect: effect.clone(),
        failures: Vec::new(),
        truncation: khive_runtime::retrieval::EmbeddingTruncationReport {
            truncated: 1,
            discarded_bytes: 17,
            ..Default::default()
        },
    }];
    let mut result = json!({"id": note_id});

    add_post_commit_embedding_warning(&mut result, Some(&effect), &outcomes);

    assert_eq!(
        result["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING])
    );

    let mut unrelated = json!({"id": Uuid::new_v4()});
    let other_effect = PostCommitEffect::ReindexEntity {
        entity_id: Uuid::new_v4(),
    };
    add_post_commit_embedding_warning(&mut unrelated, Some(&other_effect), &outcomes);
    assert!(unrelated.get("warnings").is_none());
}

#[test]
fn atomic_duplicate_update_effect_aggregates_later_truncation_outcome() {
    let note_id = Uuid::new_v4();
    let effect = PostCommitEffect::ReindexNote {
        note_id,
        version: 2,
    };
    let outcomes = vec![
        khive_runtime::atomic_prepare::PostCommitEmbeddingOutcome {
            effect: effect.clone(),
            failures: Vec::new(),
            truncation: khive_runtime::retrieval::EmbeddingTruncationReport::default(),
        },
        khive_runtime::atomic_prepare::PostCommitEmbeddingOutcome {
            effect: effect.clone(),
            failures: Vec::new(),
            truncation: khive_runtime::retrieval::EmbeddingTruncationReport {
                truncated: 1,
                discarded_bytes: 23,
                ..Default::default()
            },
        },
    ];
    let mut first_result = json!({"id": note_id});
    let mut second_result = json!({"id": note_id});

    add_post_commit_embedding_warning(&mut first_result, Some(&effect), &outcomes);
    add_post_commit_embedding_warning(&mut second_result, Some(&effect), &outcomes);

    let expected = json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]);
    assert_eq!(first_result["warnings"], expected);
    assert_eq!(second_result["warnings"], expected);
}

#[test]
fn update_rejects_unknown_field() {
    let err = validate_atomic_args("update", &json!({"id": "x", "conten": "hello"}))
        .expect_err("typo'd `conten` must be rejected");
    assert!(err.to_string().contains("unknown field"), "error: {err}");
}

#[test]
fn update_accepts_well_formed_args() {
    validate_atomic_args("update", &json!({"id": "x", "content": "hello"}))
        .expect("well-formed update args must be accepted");
}

#[test]
fn delete_rejects_unknown_field() {
    let err = validate_atomic_args("delete", &json!({"id": "x", "hardd": true}))
        .expect_err("typo'd `hardd` must be rejected");
    assert!(err.to_string().contains("unknown field"), "error: {err}");
}

#[test]
fn delete_accepts_well_formed_args() {
    validate_atomic_args("delete", &json!({"id": "x", "hard": true}))
        .expect("well-formed delete args must be accepted");
}

#[test]
fn link_rejects_unknown_field() {
    let err = validate_atomic_args(
        "link",
        &json!({
            "source_id": "a",
            "target_id": "b",
            "relation": "extends",
            "targt_backend": "x",
        }),
    )
    .expect_err("typo'd `targt_backend` must be rejected");
    assert!(err.to_string().contains("unknown field"), "error: {err}");
}

#[test]
fn link_accepts_well_formed_args() {
    validate_atomic_args(
        "link",
        &json!({"source_id": "a", "target_id": "b", "relation": "extends"}),
    )
    .expect("well-formed link args must be accepted");
}

#[test]
fn gtd_transition_rejects_unknown_field() {
    let err = validate_atomic_args(
        "gtd.transition",
        &json!({"id": "x", "status": "next", "notee": "typo"}),
    )
    .expect_err("typo'd `notee` must be rejected");
    assert!(err.to_string().contains("unknown field"), "error: {err}");
}

#[test]
fn gtd_transition_accepts_well_formed_args() {
    validate_atomic_args(
        "gtd.transition",
        &json!({"id": "x", "status": "next", "note": "ok"}),
    )
    .expect("well-formed gtd.transition args must be accepted");
}

#[test]
fn gtd_complete_rejects_unknown_field() {
    let err = validate_atomic_args("gtd.complete", &json!({"id": "x", "resutl": "typo"}))
        .expect_err("typo'd `resutl` must be rejected");
    assert!(err.to_string().contains("unknown field"), "error: {err}");
}

#[test]
fn gtd_complete_accepts_well_formed_args() {
    validate_atomic_args("gtd.complete", &json!({"id": "x", "result": "ok"}))
        .expect("well-formed gtd.complete args must be accepted");
}
