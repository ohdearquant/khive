#[test]
fn absent_empty_and_explicit_seed_preserve_literal_default_reasons() {
    let ops = vec![
        create(A, false),
        create(B, true),
        link("contains", 0.7),
        link("supports", 0.69),
        update(
            json!({"target":"entity", "name":"After"}),
            json!({"target":"entity", "name":"Before"}),
        ),
        update(
            json!({"target":"note", "content":"After"}),
            json!({"target":"note", "content":"Before"}),
        ),
        update(
            json!({"target":"edge", "weight":0.69}),
            json!({"target":"edge", "weight":0.9}),
        ),
        delete("entity"),
        delete("note"),
        delete("edge"),
        merge(),
    ];
    let create_reason = expected(
        Tier::Tier1,
        &["adr102.create"],
        "Create is additive and Tier 1-eligible under the ADR-102 floor.",
    );
    let delete_reason = expected(
        Tier::Tier2,
        &["adr102.delete"],
        "Every delete requires Tier 2 review under the ADR-102 floor.",
    );
    let want = vec![
        create_reason, expected(Tier::Tier1, &["adr102.create"], "Create is additive and Tier 1-eligible under the ADR-102 floor."),
        expected(Tier::Tier1, &["adr102.non_judgment_link"], "This non-judgment link has weight at least 0.7 and is Tier 1-eligible."),
        expected(Tier::Tier2, &["adr102.judgment_relation", "adr102.resulting_weight_below_0_7"], "supports is judgment-bearing and requires Tier 2 review. The resulting weight 0.69 is below the ADR-102 floor of 0.7."),
        expected(Tier::Tier1, &["adr102.mutable_entity_update"], "A mutable entity-field update is Tier 1-eligible under the ADR-102 floor."),
        expected(Tier::Tier2, &["adr102.not_tier_1_eligible"], "Note updates are outside ADR-102's enumerated Tier 1 fast path."),
        expected(Tier::Tier2, &["adr102.existing_edge_update", "adr102.resulting_weight_below_0_7"], "Changing an existing edge relation or weight requires Tier 2 review. The resulting weight is below the ADR-102 floor of 0.7."),
        delete_reason, expected(Tier::Tier2, &["adr102.delete"], "Every delete requires Tier 2 review under the ADR-102 floor."),
        expected(Tier::Tier2, &["adr102.delete"], "Every delete requires Tier 2 review under the ADR-102 floor."),
        expected(Tier::Tier2, &["adr102.merge"], "Every merge requires Tier 2 review under the ADR-102 floor."),
    ];
    for config in [
        "",
        "[tier]",
        "[tier]\ntier_1_operations=['create','link','update']\nminimum_weight=0.7",
    ] {
        assert_eq!(classify(config, ops.clone(), &[]), want, "{config}");
    }
    assert_eq!(Tier::Tier1.as_str(), "tier_1");
    assert_eq!(Tier::Tier2.as_str(), "tier_2");
}

#[test]
fn configured_judgment_admission_cannot_lower_floor() {
    let config = "[tier]\ntier_1_relations=['supports','supersedes','refutes']\nminimum_weight=0.0";
    for relation in ["supports", "supersedes", "refutes"] {
        let result = classify(config, vec![link(relation, 0.9)], &[]);
        assert_eq!(result[0].tier, Tier::Tier2);
        assert_eq!(result[0].reason_ids, ["adr102.judgment_relation"]);
    }
}

#[test]
fn create_only_policy_changes_the_same_non_judgment_link() {
    let ops = vec![create(A, false), link("contains", 0.9)];
    assert_eq!(classify("", ops.clone(), &[])[1].tier, Tier::Tier1);
    let result = classify("[tier]\ntier_1_operations=['create']", ops, &[]);
    assert_eq!(result[0].tier, Tier::Tier1);
    assert_eq!(result[1].tier, Tier::Tier2);
    assert!(result[1]
        .reason_ids
        .iter()
        .any(|id| id == "config.operation_not_tier_1"));
}

#[test]
fn omitted_and_empty_lists_have_distinct_narrowing_semantics() {
    assert_eq!(
        classify("[tier]", vec![link("contains", 0.9)], &[])[0].tier,
        Tier::Tier1
    );
    for config in [
        "[tier]\ntier_1_operations=[]",
        "[tier]\ntier_1_relations=[]",
        "[tier]\ntier_1_relations=['extends']",
    ] {
        assert_eq!(
            classify(config, vec![link("contains", 0.9)], &[])[0].tier,
            Tier::Tier2
        );
    }
    let entity_update = update(
        json!({"target":"entity", "name":"After"}),
        json!({"target":"entity", "name":"Before"}),
    );
    assert_eq!(
        classify("[tier]", vec![entity_update.clone()], &[])[0].tier,
        Tier::Tier1
    );
    assert_eq!(
        classify("[tier]\ntier_1_entity_fields=[]", vec![entity_update], &[])[0].tier,
        Tier::Tier2
    );
}
