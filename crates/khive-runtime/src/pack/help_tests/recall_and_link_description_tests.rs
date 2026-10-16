/// help=true on `recall` returns a schema envelope including the `query` param.
#[tokio::test]
async fn test_help_true_returns_schema_for_recall() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let reg = build_help_registry(invocations.clone());

    let result = reg
        .dispatch("recall", serde_json::json!({ "help": true }))
        .await
        .expect("help=true must succeed for recall");

    assert_eq!(result["verb"], "recall");
    assert_eq!(result["pack"], "helptest");

    let params = result["params"]
        .as_array()
        .expect("params must be a JSON array");

    // `query` must be present and required.
    let query_param = params.iter().find(|p| p["name"] == "query");
    assert!(query_param.is_some(), "params must include 'query'");
    let query_param = query_param.unwrap();
    assert_eq!(
        query_param["required"],
        serde_json::json!(true),
        "'query' must be required"
    );

    // `limit` must be present and optional.
    let limit_param = params.iter().find(|p| p["name"] == "limit");
    assert!(limit_param.is_some(), "params must include 'limit'");
    let limit_param = limit_param.unwrap();
    assert_eq!(
        limit_param["required"],
        serde_json::json!(false),
        "'limit' must be optional"
    );
}

/// `link(help=true)` (issue #964) surfaces the composed per-relation
/// endpoint allowlist: the base entity-to-entity table, every loaded
/// pack's additive `EDGE_RULES`, and the `annotates` note-to-any rule —
/// so a batch caller can defer to the kernel's own table instead of
/// re-implementing it.
#[tokio::test]
async fn test_link_help_true_exposes_endpoint_rules() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let reg = build_help_registry(invocations.clone());

    let result = reg
        .dispatch("link", serde_json::json!({ "help": true }))
        .await
        .expect("help=true must succeed for link");

    assert_eq!(result["verb"], "link");
    let rules = result["endpoint_rules"]
        .as_array()
        .expect("link help must include an endpoint_rules array");
    assert!(!rules.is_empty(), "endpoint_rules must not be empty");

    // A base entity-to-entity rule (khive-runtime's own table) must appear.
    assert!(
        rules.iter().any(|r| r["relation"] == "contains"
            && r["source"] == "entity:concept"
            && r["target"] == "entity:concept"),
        "endpoint_rules must include the base 'contains' entity rule; got {rules:#?}"
    );

    // The pack-declared additive rule (HelpPack's task->task depends_on) must appear.
    assert!(
        rules.iter().any(|r| r["relation"] == "depends_on"
            && r["source"] == "note:task"
            && r["target"] == "note:task"),
        "endpoint_rules must include the pack-declared depends_on rule; got {rules:#?}"
    );

    // The annotates note-to-any special case must appear.
    assert!(
        rules
            .iter()
            .any(|r| r["relation"] == "annotates" && r["source"] == "note:*"),
        "endpoint_rules must document the annotates note-to-any rule; got {rules:#?}"
    );

    // help=true must remain side-effect-free.
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "link(help=true) must not invoke pack dispatch"
    );
}

/// `link(help=true)`'s `endpoint_rules` must match, set-for-set, every
/// endpoint pair `validate_edge_relation_endpoints`
/// (`crates/khive-runtime/src/operations.rs`) actually accepts for the
/// three special relations (`supersedes` / `supports` / `refutes`):
///
/// - a `note -> note` row for each of the three relations (the
///   validator's dedicated special-relation branch accepts any
///   `Resolved::Note(_), Resolved::Note(_)` pair unconditionally,
///   `operations.rs:1338` / `:1527` — before `pack_rule_allows` is ever
///   reached);
/// - the base entity->entity rows for the three relations
///   (`base_entity_endpoint_rules`, e.g. `concept -[supersedes]-> concept`);
/// - and, critically, NOT a row for `HelpPack`'s pack-declared
///   `supersedes` rule on `note:task -> note:task`
///   (`HELP_EDGE_RULES[1]`) — because the validator's special-relation
///   branch returns before `pack_rule_allows` is consulted, that pack
///   rule is never actually enforced, so advertising it would be a false
///   promise (the exact defect this test guards against, issue #991).
#[tokio::test]
async fn test_link_help_true_matches_special_relation_validator_set() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let reg = build_help_registry(invocations.clone());

    let result = reg
        .dispatch("link", serde_json::json!({ "help": true }))
        .await
        .expect("help=true must succeed for link");

    let rules = result["endpoint_rules"]
        .as_array()
        .expect("link help must include an endpoint_rules array");

    for relation in ["supersedes", "supports", "refutes"] {
        // The unconditional note -> note row must appear.
        assert!(
            rules.iter().any(|r| r["relation"] == relation
                && r["source"] == "note:*"
                && r["target"] == "note:*"),
            "endpoint_rules must include the note:*->note:* row for '{relation}' \
                 (validator accepts any note->note pair unconditionally); got {rules:#?}"
        );

        // HelpPack's pack-declared rule for this relation on note:task->note:task
        // (only Supersedes is declared in HELP_EDGE_RULES) must NOT be advertised
        // as a distinct entity — the validator never reaches pack_rule_allows for
        // special relations, so no note:task->note:task row should exist for it.
        assert!(
            !rules.iter().any(|r| r["relation"] == relation
                && r["source"] == "note:task"
                && r["target"] == "note:task"),
            "endpoint_rules must NOT advertise a pack EDGE_RULES row for special \
                 relation '{relation}' — validate_edge_relation_endpoints never consults \
                 pack_rule_allows for supersedes/supports/refutes; got {rules:#?}"
        );
    }

    // Base entity->entity rows for the three relations (from
    // base_entity_endpoint_rules) must still appear alongside the note rows.
    for (relation, kind) in [
        ("supersedes", "concept"),
        ("supports", "concept"),
        ("refutes", "concept"),
    ] {
        assert!(
            rules.iter().any(|r| r["relation"] == relation
                && r["source"] == format!("entity:{kind}")
                && r["target"] == "entity:concept"),
            "endpoint_rules must include the base entity:{kind}->entity:concept row \
                 for '{relation}'; got {rules:#?}"
        );
    }
}

#[test]
fn special_relation_predicate_matches_the_dedicated_validator_set() {
    for relation in khive_types::EdgeRelation::ALL {
        assert_eq!(
            is_special_relation(relation),
            matches!(
                relation,
                khive_types::EdgeRelation::Supersedes
                    | khive_types::EdgeRelation::Supports
                    | khive_types::EdgeRelation::Refutes
            ),
            "unexpected special-relation classification for {relation}"
        );
    }
}
