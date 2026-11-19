#[test]
fn known_subjects_cover_notes_edges_and_both_merge_participants() {
    for (op, subject) in [
        (create(A, false), A),
        (create(A, true), A),
        (link("contains", 0.9), EDGE),
        (link("contains", 0.9), A),
        (link("contains", 0.9), B),
        (
            update(
                json!({"target":"entity", "name":"After"}),
                json!({"target":"entity", "name":"Before"}),
            ),
            A,
        ),
        (
            update(
                json!({"target":"note", "content":"After"}),
                json!({"target":"note", "content":"Before"}),
            ),
            A,
        ),
        (
            update(
                json!({"target":"edge", "weight":0.8}),
                json!({"target":"edge", "weight":0.9}),
            ),
            A,
        ),
        (delete("entity"), A),
        (delete("note"), A),
        (delete("edge"), A),
        (merge(), A),
        (merge(), B),
    ] {
        let findings = [TierFinding {
            severity: "error",
            subject_id: Some(subject),
        }];
        let result = classify("", vec![op, create(C, false)], &findings);
        assert_eq!(result[0].tier, Tier::Tier2);
        assert_eq!(
            result[0].reason_ids.last().unwrap(),
            "validation.error_severity_finding"
        );
        assert_eq!(
            result[1].tier,
            Tier::Tier1,
            "known subject {subject} became global"
        );
    }
}

#[test]
fn global_and_unmapped_errors_escalate_all_ops_but_warning_info_do_not() {
    let ops = vec![create(A, true), link("contains", 0.9)];
    for subject_id in [None, Some("unknown-record")] {
        let result = classify(
            "",
            ops.clone(),
            &[TierFinding {
                severity: "error",
                subject_id,
            }],
        );
        assert!(result.iter().all(|decision| decision.tier == Tier::Tier2));
    }
    for severity in ["warning", "info"] {
        assert!(classify(
            "",
            ops.clone(),
            &[TierFinding {
                severity,
                subject_id: None
            }]
        )
        .iter()
        .all(|decision| decision.tier == Tier::Tier1));
    }
    let result = classify(
        "",
        vec![create(A, false)],
        &[TierFinding {
            severity: "error",
            subject_id: Some(A),
        }],
    );
    assert_eq!(result[0].reason, "Create is additive and Tier 1-eligible under the ADR-102 floor. An error-severity validation finding escalates this operation to Tier 2.");
}

#[test]
fn coverage_error_is_a_real_floor_input_and_empty_changeset_is_empty() {
    let op = update(
        json!({"target":"entity", "name":"After"}),
        json!({"target":"entity", "name":"Before"}),
    );
    let result = classify(
        "",
        vec![op],
        &[TierFinding {
            severity: "error",
            subject_id: Some(A),
        }],
    );
    assert_eq!(result[0].tier, Tier::Tier2);
    assert_eq!(
        result[0].reason_ids,
        [
            "adr102.mutable_entity_update",
            "validation.error_severity_finding"
        ]
    );
    assert!(classify(
        "",
        vec![],
        &[TierFinding {
            severity: "error",
            subject_id: None
        }]
    )
    .is_empty());
}

#[test]
fn tier_syntax_errors_keep_toml_sources_and_semantic_errors_are_typed() {
    for input in [
        "[tier]\nunknown=true",
        "[tier]\ntier_1_operations='create'",
        "[tier]\nminimum_weight='high'",
        "[tier]\ntier_1_relations=[1]",
    ] {
        let error = Rules::parse_toml(input).unwrap_err();
        assert!(matches!(&error, RulesError::Syntax(_)), "{input}");
        assert!(error.source().is_some());
    }
    for entry in [
        "tier_1_operations=['restore']",
        "tier_1_relations=['typo']",
        "tier_1_entity_fields=['kind']",
        "tier_1_operations=['create','create']",
        "tier_1_relations=['contains','contains']",
        "tier_1_entity_fields=['name','name']",
        "minimum_weight=nan",
        "minimum_weight=inf",
        "minimum_weight=-inf",
        "minimum_weight=-0.01",
        "minimum_weight=1.01",
    ] {
        let error = Rules::parse_toml(&format!("[tier]\n{entry}")).unwrap_err();
        assert!(matches!(&error, RulesError::Tier(_)), "{entry}: {error}");
        assert!(error.source().is_none());
        assert!(error.to_string().starts_with("tier."));
    }
}
