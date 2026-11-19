// ── Schedule core indexes are owned by migration V51 ────────────────────────

#[tokio::test]
async fn schedule_pack_has_no_auxiliary_schema() {
    use khive_runtime::PackRuntime;
    let pack = SchedulePack::new(support::memory_runtime());
    assert!(
        pack.schema_plan().is_empty(),
        "schedule indexes on core tables belong to migration V51"
    );
    assert!(<SchedulePack as Pack>::SCHEMA_PLAN.is_none());
}

#[tokio::test]
async fn registry_includes_schedule_without_core_table_ddl() {
    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime));
    let registry = builder.build().expect("registry builds");
    let mut names = registry.pack_names();
    names.sort_unstable();
    assert_eq!(names, ["kg", "schedule"]);
    let plans = registry.all_schema_plans();
    assert_eq!(plans.len(), 2, "both registered packs contribute a plan");
    assert!(
        plans.iter().all(|plan| plan.is_empty()),
        "kg and schedule need no auxiliary DDL; core indexes come from migrations"
    );
}

// ── SCH-AUD-002 regression: malformed five-field cron rejected ───────────────

#[tokio::test]
async fn sch_aud_002_malformed_five_field_cron_rejected() {
    let (registry, _rt) = build_registry();

    // Five fields but all garbage — must be rejected.
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

// ── SCH-AUD-003 regression: agenda limit=0 and limit>200 rejected ────────────

#[tokio::test]
async fn sch_aud_003_agenda_limit_zero_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 0 }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("limit") || msg.contains("range") || msg.contains('0'),
        "SCH-AUD-003: limit=0 must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_003_agenda_limit_over_max_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 201 }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("limit") || msg.contains("range") || msg.contains("201"),
        "SCH-AUD-003: limit=201 must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_003_agenda_limit_boundary_values_accepted() {
    let (registry, _rt) = build_registry();

    // limit=1 and limit=200 are valid boundary values.
    for limit in [1u32, 200u32] {
        registry
            .dispatch("schedule.agenda", serde_json::json!({ "limit": limit }))
            .await
            .unwrap_or_else(|e| panic!("SCH-AUD-003: limit={limit} must be accepted; got: {e}"));
    }
}
