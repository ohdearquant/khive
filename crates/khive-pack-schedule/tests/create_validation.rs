//! Tests for remind/schedule creation and input validation (C1, C3, C4, repeat).

use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

fn build_registry_with_brain() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_brain::BrainPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

#[tokio::test]
async fn remind_creates_pending_event() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "check status",
                "at": "2099-06-01T09:00:00Z"
            }),
        )
        .await
        .expect("remind succeeds");

    assert!(result.get("id").is_some(), "remind returns id: {result}");
    assert_eq!(result["status"], "pending");
    assert_eq!(result["event_type"], "remind");
}

#[tokio::test]
async fn schedule_creates_pending_event_with_action() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", name=\"test\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule succeeds");

    assert!(result.get("id").is_some(), "schedule returns id: {result}");
    assert_eq!(result["event_type"], "schedule");
}

#[tokio::test]
async fn test_full_id_returns_36_char_schedule() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "check status", "at": "2099-06-01T09:00:00Z" }),
        )
        .await
        .expect("remind succeeds");

    let id = result
        .get("id")
        .and_then(|v| v.as_str())
        .expect("id present");
    let full_id = result
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present");

    assert_eq!(id.len(), 8, "id must be 8-char short prefix");
    assert_eq!(full_id.len(), 36, "full_id must be 36-char hyphenated UUID");
    assert!(
        full_id.starts_with(id),
        "full_id must start with the short id prefix"
    );
    assert!(full_id.contains('-'), "full_id must be hyphenated format");
}

// ── S-C1 regression: RFC 3339 timestamp validation ──────────────────────────

#[tokio::test]
async fn s_c1_schedule_valid_rfc3339_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
                "at": "2099-01-01T00:00:00Z"
            }),
        )
        .await
        .expect("schedule with valid RFC 3339 must succeed");

    assert_eq!(result["status"], "pending");
}

#[tokio::test]
async fn s_c1_schedule_invalid_at_not_a_date() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
                "at": "not-a-date"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("not-a-date"),
        "S-C1: error must reference RFC 3339 or the bad value; got: {msg}"
    );
}

#[tokio::test]
async fn s_c1_schedule_invalid_at_natural_language() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
                "at": "tomorrow at 3pm"
            }),
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("RFC 3339") || err.to_string().contains("timestamp"),
        "S-C1: natural-language at must be rejected with RFC 3339 hint; got: {err}"
    );
}

#[tokio::test]
async fn s_c1_schedule_invalid_at_out_of_range_date() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
                "at": "2027-13-99"
            }),
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("RFC 3339") || err.to_string().contains("timestamp"),
        "S-C1: out-of-range date must be rejected; got: {err}"
    );
}

#[tokio::test]
async fn s_c1_remind_invalid_at_is_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "hello",
                "at": "invalid"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("invalid"),
        "S-C1: remind with invalid at must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn s_c1_remind_valid_rfc3339_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "morning standup",
                "at": "2099-06-15T09:00:00+00:00"
            }),
        )
        .await
        .expect("remind with valid RFC 3339 offset format must succeed");

    assert_eq!(result["status"], "pending");
}

// ── C3 regression: past dates rejected ───────────────────────────────────────

#[tokio::test]
async fn c3_schedule_past_date_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"past\", at=\"2099-12-31T00:00:00Z\")",
                "at": "2020-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("past") || msg.contains("future"),
        "C3: past at must be rejected with past/future hint; got: {msg}"
    );
}

#[tokio::test]
async fn c3_remind_past_date_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "stale reminder",
                "at": "2019-06-01T09:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("past") || msg.contains("future"),
        "C3: past remind.at must be rejected; got: {msg}"
    );
}

// ── C4 regression: unparseable DSL action rejected ───────────────────────────

#[tokio::test]
async fn c4_schedule_bogus_action_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "bogus-not-a-valid-verb()",
                "at": "2099-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("DSL") || msg.contains("action") || msg.contains("invalid"),
        "C4: bogus action must be rejected with DSL error hint; got: {msg}"
    );
}

#[tokio::test]
async fn c4_schedule_single_char_action_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "x",
                "at": "2099-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("DSL") || msg.contains("action") || msg.contains("invalid"),
        "C4: single-char action must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn c4_schedule_valid_action_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"hello world\", at=\"2099-12-31T00:00:00Z\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule with valid DSL action must succeed");

    assert_eq!(result["status"], "pending");
}

// ── H5 regression: trigger_at preserves caller's original RFC3339 string ─────

#[tokio::test]
async fn h5_schedule_at_with_offset_preserves_original_string() {
    let (registry, _rt) = build_registry();

    let input_at = "2099-01-02T00:00:00+02:00";
    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"tz-test\", at=\"2099-12-31T00:00:00Z\")",
                "at": input_at
            }),
        )
        .await
        .expect("schedule with +02:00 offset must succeed");

    let trigger_at = result["trigger_at"].as_str().expect("trigger_at present");
    assert_eq!(
        trigger_at, input_at,
        "H5: trigger_at in response must preserve caller's original RFC3339 string; got {trigger_at}"
    );
    assert!(
        trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
        "H5: stored trigger_at must be a valid RFC 3339 timestamp; got {trigger_at}"
    );
}

#[tokio::test]
async fn h5_remind_at_with_offset_preserves_original_string() {
    let (registry, _rt) = build_registry();

    let input_at = "2099-06-15T09:00:00+05:30";
    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "tz-remind",
                "at": input_at
            }),
        )
        .await
        .expect("remind with +05:30 offset must succeed");

    let trigger_at = result["trigger_at"].as_str().expect("trigger_at present");
    assert_eq!(
        trigger_at, input_at,
        "H5: trigger_at in remind response must preserve caller's original RFC3339 string; got {trigger_at}"
    );
    assert!(
        trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
        "H5: stored trigger_at must be a valid RFC 3339 timestamp; got {trigger_at}"
    );
}

#[tokio::test]
async fn h5_utc_input_preserved_as_is() {
    let (registry, _rt) = build_registry();

    let input_at = "2099-03-10T12:00:00Z";
    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "utc-tz-test", "at": input_at }),
        )
        .await
        .expect("remind with Z suffix must succeed");

    let trigger_at = result["trigger_at"].as_str().expect("trigger_at present");
    assert_eq!(
        trigger_at, input_at,
        "H5: UTC input must be returned unchanged; got {trigger_at}"
    );
}

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
        msg.contains("minute") || msg.contains("range") || msg.contains("99"),
        "SCH-AUD-002: out-of-range minute field must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_002_valid_wildcard_cron_accepted() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "wildcard cron",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "* * * * *"
            }),
        )
        .await
        .expect("SCH-AUD-002: all-wildcard cron must be accepted");
    assert_eq!(result["status"], "pending");
}

#[tokio::test]
async fn sch_aud_002_valid_numeric_cron_accepted() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "monday morning",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "0 9 * * 1"
            }),
        )
        .await
        .expect("SCH-AUD-002: valid numeric cron must be accepted");
    assert_eq!(result["status"], "pending");
}

// ── Issue #481: repeat contract matrix (narrowed, Option B) ─────────────────
//
// Standard cron operators (steps, ranges, lists) are documented as accepted
// but rejected by the implementation, and `kkernel` does not advance
// five-field repeats yet. Rather than build a full cron parser, the contract
// is narrowed to named aliases plus a 5-field form where each field is `*`
// or one in-range integer. This matrix asserts the narrowed contract.

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
        .unwrap_or_else(|e| panic!("repeat {repeat:?} must be accepted under Option B; got: {e}"));
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
        "repeat {repeat:?} must be rejected under Option B; got: {msg}"
    );
}

#[tokio::test]
async fn repeat_contract_matrix_aliases_accepted() {
    assert_repeat_accepted("daily").await;
    assert_repeat_accepted("weekly").await;
    assert_repeat_accepted("monthly").await;
}

#[tokio::test]
async fn repeat_contract_matrix_wildcard_accepted() {
    assert_repeat_accepted("* * * * *").await;
}

#[tokio::test]
async fn repeat_contract_matrix_single_numeric_field_accepted() {
    assert_repeat_accepted("0 9 * * 1").await;
}

#[tokio::test]
async fn repeat_contract_matrix_step_operator_rejected() {
    assert_repeat_rejected("*/15 * * * *").await;
}

#[tokio::test]
async fn repeat_contract_matrix_range_operator_rejected() {
    assert_repeat_rejected("0 9-17 * * 1-5").await;
}

#[tokio::test]
async fn repeat_contract_matrix_list_operator_rejected() {
    assert_repeat_rejected("0,30 9 * * 1").await;
}

#[tokio::test]
async fn repeat_contract_matrix_out_of_range_rejected() {
    assert_repeat_rejected("99 * * * *").await;
}

#[tokio::test]
async fn repeat_contract_matrix_malformed_rejected() {
    assert_repeat_rejected("foo bar baz qux zap").await;
}

// ── Issue #461: schedule.schedule write-time replayability ──────────────────

#[tokio::test]
async fn schedule_schedule_rejects_bare_schedule_pack_action() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "remind(content=\"hello\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("not registered") || msg.contains("pack-prefixed"),
        "#461: bare unqualified schedule-pack verb must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_chain_with_prev() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "stats() | create(kind=\"entity\", name=$prev.id)",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("chain") || msg.contains("$prev"),
        "#461: chained actions with $prev must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_missing_required_replay_args() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"hello\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("missing") && msg.contains("at"),
        "#461: missing required replay arg `at` must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_create_missing_kind_and_items() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("kind") && msg.contains("items"),
        "#461: create() missing both kind and items must be rejected at write time; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_accepts_create_with_kind() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("#461: create() with kind present must be accepted");

    assert_eq!(result["status"], "pending");
}

/// Round-2 regression (codex REJECT, "create replay validation still misses
/// KG kind reconciliation failures"): `create(kind="concept",
/// entity_kind="person", name="x")` is accepted by `schedule.schedule` before
/// this fix, yet the real `create` handler
/// (`khive-pack-kg/src/handlers/create.rs` via
/// `handlers::common::reconcile_specific`) deterministically rejects it —
/// `kind="concept"` and `entity_kind="person"` contradict. Schedule-time
/// validation must reject this the same way, not merely require presence of
/// `entity_kind`.
#[tokio::test]
async fn schedule_schedule_rejects_create_with_contradicting_kind_and_entity_kind() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", entity_kind=\"person\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("contradicts"),
        "#461/#462: a granular kind that contradicts entity_kind must be rejected at \
         schedule time, mirroring KG's reconcile_specific; got: {msg}"
    );

    // Confirm this really does mirror the live KG handler's own rejection.
    let kg_err = registry
        .dispatch(
            "create",
            serde_json::json!({"kind": "concept", "entity_kind": "person", "name": "x"}),
        )
        .await
        .unwrap_err();
    assert!(
        kg_err.to_string().contains("contradicts"),
        "sanity: the live KG create handler must also reject this contradiction; got: {kg_err}"
    );
}

/// Same contradiction, but inside a bulk `create(items=[...])` entry — the
/// bulk validator (`validate_create_bulk_items`) must apply the same
/// reconciliation per-entry that `khive-pack-kg`'s bulk create path applies.
#[tokio::test]
async fn schedule_schedule_rejects_create_bulk_item_with_contradicting_kind_and_entity_kind() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(items=[{\"kind\":\"concept\",\"entity_kind\":\"person\",\"name\":\"x\"}])",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("items[0]") && msg.contains("contradicts"),
        "#461/#462: a bulk items[] entry with a kind/entity_kind contradiction must be \
         rejected at schedule time; got: {msg}"
    );

    // Confirm this really does mirror the live KG bulk create handler's own rejection.
    let kg_err = registry
        .dispatch(
            "create",
            serde_json::json!({
                "items": [{"kind": "concept", "entity_kind": "person", "name": "x"}]
            }),
        )
        .await
        .unwrap_err();
    assert!(
        kg_err.to_string().contains("contradicts"),
        "sanity: the live KG bulk create handler must also reject this contradiction; \
         got: {kg_err}"
    );
}

/// An invalid legacy `entity_kind` value (unknown to both the base
/// `khive_types::EntityKind` parser and the registry's merged vocabulary)
/// must be rejected at schedule time, not merely accepted because
/// `entity_kind` was present.
#[tokio::test]
async fn schedule_schedule_rejects_create_with_invalid_entity_kind() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"entity\", entity_kind=\"not_a_real_kind\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("unknown entity_kind"),
        "an invalid legacy entity_kind must be rejected at schedule time; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_business_namespace_arg() {
    let (registry, _rt) = build_registry_with_brain();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "brain.bind(profile_id=\"p\", namespace=\"team-a\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("namespace"),
        "#461: scheduled action with a business `namespace` arg must be rejected \
         (replay always overwrites it with the firing event's routing namespace); got: {msg}"
    );
}

/// Issue #462: omitting `namespace` from the stored action does NOT make
/// `brain.bind` replayable. `dispatch_action` (`kkernel/src/pending_events.rs`)
/// unconditionally injects the firing event's routing namespace into every
/// op's args at trigger time, and the registry passes it straight through to
/// any handler that declares `namespace` as a param — so an *omitted*
/// `namespace` (which would otherwise default to `brain.bind`'s wildcard
/// `"*"`) is silently rebound to the event's routing namespace on replay.
/// This is exactly as unsafe as an explicitly-stored `namespace` arg, so
/// `schedule.schedule` must reject any handler whose schema declares
/// `namespace`, regardless of whether the stored args include the key.
#[tokio::test]
async fn schedule_schedule_rejects_verb_declaring_namespace_even_when_omitted() {
    let (registry, _rt) = build_registry_with_brain();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "brain.bind(profile_id=\"p\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("brain.bind") && msg.contains("namespace"),
        "#462: brain.bind must be rejected even without an explicit `namespace` arg — the \
         verb declares `namespace` as a business param, and replay would inject the event's \
         routing namespace regardless of what (if anything) was stored; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_accepts_exact_replayable_single_action() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "stats()",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("#461: exact registered zero-arg verb call must be accepted");

    assert_eq!(result["status"], "pending");
}
