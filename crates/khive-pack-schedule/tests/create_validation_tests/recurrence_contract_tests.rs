// ── Repeat validation ───────────────────────────────────────────────────────

#[tokio::test]
async fn remind_with_invalid_repeat_is_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "hello",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "not-valid-cron"
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("repeat") || err.to_string().contains("cron"));
}

#[tokio::test]
async fn sch_aud_002_malformed_five_field_cron_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "bad cron",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "foo bar baz qux zap"
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("repeat") || msg.contains("cron") || msg.contains("invalid"),
        "SCH-AUD-002: malformed five-field cron must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_002_out_of_range_cron_minute_rejected() {
    let (registry, _rt) = build_registry();

    // Minute field 99 is out of range (0-59): the parser the executor uses refuses it,
    // so creation refuses it too.
    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "bad minute",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "99 * * * *"
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("invalid repeat expression") && msg.contains("cron"),
        "SCH-AUD-002: every cron form the executor cannot advance must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_002_cron_the_executor_advances_is_accepted() {
    // SCH-AUD-002 once required these forms to be refused because the executor could
    // not advance them. Creation and the executor now share one parser, so a cron
    // expression the executor advances is accepted, and only unadvanceable forms remain
    // refused (see the out-of-range and malformed arms above).
    for repeat in ["0 9 * * 1", "* * * * *"] {
        let (registry, _rt) = build_registry();
        let result = registry
            .dispatch(
                "schedule.remind",
                serde_json::json!({
                    "content": "monday morning",
                    "at": "2099-06-01T09:00:00Z",
                    "repeat": repeat
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("SCH-AUD-002: cron {repeat:?} must be accepted; got: {e}"));
        assert_eq!(result["status"], "pending");
    }
}

// ── Repeat contract matrix ─────────────────────────────────────────────────
// Creation and the executor share one parser: the three calendar aliases,
// `every:<N><s|m|h|d>` intervals, and five-field cron in UTC. Everything
// else is rejected at creation rather than silently consumed as a one-shot.

async fn assert_repeat_accepted(repeat: &str) {
    let (registry, _rt) = build_registry();
    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "repeat contract check",
                "at": "2099-06-01T09:00:00Z",
                "repeat": repeat
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("repeat {repeat:?} must be accepted; got: {e}"));
    assert_eq!(result["status"], "pending");
}

async fn assert_repeat_rejected(repeat: &str) {
    let (registry, _rt) = build_registry();
    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "repeat contract check",
                "at": "2099-06-01T09:00:00Z",
                "repeat": repeat
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("repeat") || msg.contains("cron"),
        "repeat {repeat:?} must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn repeat_contract_matrix_aliases_accepted() {
    assert_repeat_accepted("daily").await;
    assert_repeat_accepted("weekly").await;
    assert_repeat_accepted("monthly").await;
}

#[tokio::test]
async fn repeat_contract_matrix_intervals_accepted() {
    assert_repeat_accepted("every:30s").await;
    assert_repeat_accepted("every:15m").await;
    assert_repeat_accepted("every:2h").await;
    assert_repeat_accepted("every:1d").await;
}

#[tokio::test]
async fn repeat_contract_matrix_five_field_cron_accepted() {
    assert_repeat_accepted("* * * * *").await;
    assert_repeat_accepted("0 9 * * 1").await;
    assert_repeat_accepted("*/15 * * * *").await;
    assert_repeat_accepted("0 9-17 * * 1-5").await;
    assert_repeat_accepted("0,30 9 * * 1").await;
}

#[tokio::test]
async fn repeat_contract_matrix_malformed_intervals_rejected() {
    assert_repeat_rejected("every:0s").await;
    assert_repeat_rejected("every:15").await;
    assert_repeat_rejected("every:x").await;
    assert_repeat_rejected("every:1w").await;
}

#[tokio::test]
async fn repeat_without_a_representable_successor_is_rejected_by_both_creation_verbs() {
    let (registry, runtime) = build_registry();
    for (verb, params) in [
        (
            "schedule.remind",
            serde_json::json!({
                "content": "far future reminder",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "every:100000000d"
            }),
        ),
        (
            "schedule.schedule",
            serde_json::json!({
                "action": "stats()",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "every:100000000d"
            }),
        ),
    ] {
        let error = registry.dispatch(verb, params).await.expect_err(verb);
        let message = error.to_string();
        assert!(message.contains("every:100000000d"), "{verb}: {message}");
        assert!(
            message.contains("no representable occurrence"),
            "{verb}: {message}"
        );
    }

    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize");
    let page = runtime
        .notes(&token)
        .expect("notes")
        .query_notes(
            "local",
            Some("scheduled_event"),
            khive_storage::types::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("query scheduled events");
    assert!(
        page.items.is_empty(),
        "rejected repeats must not create rows"
    );
}

#[tokio::test]
async fn repeat_contract_matrix_out_of_range_cron_rejected() {
    assert_repeat_rejected("99 * * * *").await;
}

#[tokio::test]
async fn repeat_contract_matrix_wrong_field_count_rejected() {
    assert_repeat_rejected("* * * *").await;
    assert_repeat_rejected("0 9 * * 1 2027").await;
}

#[tokio::test]
async fn repeat_contract_matrix_nicknames_rejected() {
    assert_repeat_rejected("@daily").await;
    assert_repeat_rejected("hourly").await;
}

#[tokio::test]
async fn repeat_contract_matrix_malformed_rejected() {
    assert_repeat_rejected("foo bar baz qux zap").await;
}
