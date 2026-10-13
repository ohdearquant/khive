use super::*;
use khive_pack_kg::KgPack;
use khive_runtime::{VerbRegistry, VerbRegistryBuilder};

/// Since #2956 the note-update sequence lives at the registry, so a test that means to
/// exercise the path generic update takes has to go through one.
///
/// `kg` is registered because `gtd` declares it as a required pack, so a registry holding
/// only `gtd` refuses to build with `MissingPackDependency`. This mirrors the fixture in
/// `tests/common`, which is the crate's canonical way to stand one up.
fn registry_with_gtd(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(crate::GtdPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

use serde_json::json;

fn task_note_with(properties: Value) -> Note {
    let mut task = Note::new("local", "task", "body");
    task.name = Some("a task".to_string());
    task.properties = Some(properties);
    task
}

/// A reschedule through the generic property path lands in the same shape `gtd.assign`
/// writes, and keeps the anchor the task already had rather than the host's.
#[tokio::test]
async fn due_through_properties_is_anchored_in_the_tasks_existing_zone() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let note = task_note_with(json!({
        "due": "2026-10-01T00:00:00-04:00",
        "due_timezone": "America/New_York",
        "status": "inbox",
    }));
    let mut args = json!({"properties": {"due": "2026-12-25"}});

    normalize_due_update(&runtime, &note, &mut args).expect("normalize");

    assert_eq!(
        args["properties"]["due"], "2026-12-25T00:00:00-05:00",
        "a date-only due must be anchored to the earliest instant of that local date"
    );
    assert_eq!(
        args["properties"]["due_timezone"], "America/New_York",
        "the anchor must be rewritten beside the value, not left from the create"
    );
}

/// The zone named in the update wins over the one the task carries. Only the `due` assertion
/// below discriminates: zone names parse case-sensitively, so a zone named in this spelling is
/// stored back as the same string the caller sent, and the anchor assertion would hold even if
/// nothing rewrote it. The rewrite itself is proved by the `timezone`-spelling test, where the
/// stored anchor is a value no caller supplied.
#[tokio::test]
async fn a_zone_named_in_the_update_wins_over_the_stored_anchor() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let note = task_note_with(json!({"due_timezone": "America/New_York"}));
    let mut args = json!({
        "properties": {"due": "2026-12-25", "due_timezone": "Asia/Tokyo"}
    });

    normalize_due_update(&runtime, &note, &mut args).expect("normalize");

    assert_eq!(args["properties"]["due"], "2026-12-25T00:00:00+09:00");
    assert_eq!(args["properties"]["due_timezone"], "Asia/Tokyo");
}

/// Both halves of the pair are refused when they cannot be parsed. Before this, each was
/// stored verbatim: `due` as "next tuesday-ish" and the zone as "Mars/Olympus".
#[tokio::test]
async fn an_unparseable_due_or_zone_is_refused() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let note = task_note_with(json!({"status": "inbox"}));

    let mut bad_due = json!({"properties": {"due": "next tuesday-ish"}});
    normalize_due_update(&runtime, &note, &mut bad_due)
        .expect_err("an unparseable due must not be stored verbatim");

    let mut bad_zone = json!({
        "properties": {"due": "2026-12-25", "due_timezone": "Mars/Olympus"}
    });
    let err = normalize_due_update(&runtime, &note, &mut bad_zone)
        .expect_err("an unparseable zone must not be stored verbatim");
    assert!(
        format!("{err}").contains("IANA"),
        "the refusal must name what a zone is; got: {err}"
    );

    let mut wrong_type = json!({"properties": {"due": 20261225}});
    normalize_due_update(&runtime, &note, &mut wrong_type)
        .expect_err("a non-string, non-null due must be refused");
}

/// Clearing the deadline clears its anchor, since the anchor is derived state. A surviving
/// `due_timezone` beside an absent `due` is the same stale-pair defect in its other direction.
#[tokio::test]
async fn clearing_due_clears_the_anchor_with_it() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let note = task_note_with(json!({
        "due": "2026-10-01T00:00:00-04:00",
        "due_timezone": "America/New_York",
    }));
    let mut args = json!({"properties": {"due": null}});

    normalize_due_update(&runtime, &note, &mut args).expect("normalize");

    assert!(args["properties"]["due"].is_null());
    assert!(
        args["properties"]["due_timezone"].is_null(),
        "the anchor must not outlive the value it anchors"
    );
}

/// An update that does not mention `due` leaves the pair untouched, including on a task that
/// has none: the normalizer must not mint fields nobody asked for.
#[tokio::test]
async fn an_update_without_due_touches_neither_field() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let note = task_note_with(json!({"due": "2026-10-01T00:00:00-04:00"}));
    let mut args = json!({"properties": {"priority": "p1"}});

    normalize_due_update(&runtime, &note, &mut args).expect("normalize");

    assert_eq!(args, json!({"properties": {"priority": "p1"}}));

    let mut no_properties = json!({"content": "body only"});
    normalize_due_update(&runtime, &note, &mut no_properties).expect("normalize");
    assert_eq!(no_properties, json!({"content": "body only"}));
}

/// A row written before this normalization can carry an anchor that is not a zone. That is not
/// the caller's input, so the update repairs it instead of failing on it.
#[tokio::test]
async fn a_stored_anchor_that_is_not_a_zone_falls_back_instead_of_failing() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let note = task_note_with(json!({"due": "next tuesday-ish", "due_timezone": "Mars/Olympus"}));
    let mut args = json!({"properties": {"due": "2026-12-25"}});

    normalize_due_update(&runtime, &note, &mut args)
        .expect("a bad stored anchor must not fail the repair");

    let configured = runtime.config().display_timezone.name().to_string();
    assert_eq!(args["properties"]["due_timezone"], configured);
    assert_ne!(args["properties"]["due"], "2026-12-25");
}

/// The `timezone` spelling from `gtd.assign` is accepted here and consumed rather than stored,
/// since nothing reads it and the create path never writes it.
#[tokio::test]
async fn the_assign_spelling_of_the_zone_is_consumed_not_stored() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let note = task_note_with(json!({"due_timezone": "America/New_York"}));
    let mut args = json!({
        "properties": {"due": "2026-12-25", "timezone": "Asia/Tokyo"}
    });

    normalize_due_update(&runtime, &note, &mut args).expect("normalize");

    assert_eq!(args["properties"]["due"], "2026-12-25T00:00:00+09:00");
    assert_eq!(args["properties"]["due_timezone"], "Asia/Tokyo");
    assert!(
        args["properties"].get("timezone").is_none(),
        "the assign spelling must not survive into the stored bag; got: {}",
        args["properties"]
    );
}

/// A zone the caller wrote is used or refused, never ignored. Selecting the key before
/// checking that its value is a string made a malformed `due_timezone` read as absent, which
/// discarded a `timezone` alias that did parse and stored an anchor nobody asked for.
#[tokio::test]
async fn a_malformed_zone_is_refused_rather_than_read_as_absent() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let note = task_note_with(json!({"due_timezone": "America/New_York"}));

    let mut both = json!({
        "properties": {"due": "2026-12-25", "due_timezone": 7, "timezone": "Asia/Tokyo"}
    });
    let err = normalize_due_update(&runtime, &note, &mut both)
        .expect_err("a non-string zone must be refused, not skipped for the next spelling");
    assert!(
        format!("{err}").contains("due_timezone"),
        "the refusal must name the key the caller got wrong; got: {err}"
    );
    assert_eq!(
        both["properties"]["due"], "2026-12-25",
        "a refused update must leave the caller's arguments untouched"
    );

    // An explicit null is absence, not a malformed value: it falls through to the stored
    // anchor the way an omitted key does.
    let mut nulled = json!({
        "properties": {"due": "2026-12-25", "due_timezone": null}
    });
    normalize_due_update(&runtime, &note, &mut nulled).expect("a null zone reads as absent");
    assert_eq!(nulled["properties"]["due_timezone"], "America/New_York");
}

/// The wiring, not just the function: the dispatch site every generic update path calls must
/// run it. Since #2956 that site is `VerbRegistry::prepare_note_update_hook` and not a hook
/// method, so this goes through the registry — calling the hook's normalizer directly would
/// assert only that the function does what it does.
#[tokio::test]
async fn the_update_hook_runs_the_normalization() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    let registry = registry_with_gtd(&runtime);
    let note = task_note_with(json!({
        "description": "body",
        "status": "inbox",
        "due_timezone": "America/New_York",
    }));
    let mut args = json!({"properties": {"due": "2026-12-25"}});

    registry
        .prepare_note_update_hook(&runtime, &token, &note, &mut args)
        .await
        .expect("hook");

    assert_eq!(args["properties"]["due"], "2026-12-25T00:00:00-05:00");
    assert_eq!(args["properties"]["due_timezone"], "America/New_York");
}

/// The core of #2925: `salience` is derived from `priority` at create and feeds ranking, and
/// the property path used to change one without the other. An escalated task kept the level
/// its create had written.
#[tokio::test]
async fn an_escalated_priority_rewrites_the_salience_beside_it() {
    // No note fixture: the level a task currently holds does not enter this decision. The
    // salience follows the level named in THIS update, which is what makes the escalation
    // land rather than being merged against a stored value.
    let mut args = json!({"properties": {"priority": "p0"}});

    normalize_priority_update(&mut args).expect("normalize");

    assert_eq!(args["properties"]["priority"], "p0");
    assert_eq!(
        args["salience"], 1.0,
        "the ranking value must follow the level, not the create"
    );
}

/// `gtd.assign` stores the level lowercase. The property path now agrees, so one task cannot
/// hold a spelling that its own create path would never have written.
#[tokio::test]
async fn a_priority_is_stored_in_the_spelling_the_create_path_uses() {
    let mut args = json!({"properties": {"priority": "P1"}});

    normalize_priority_update(&mut args).expect("normalize");

    assert_eq!(args["properties"]["priority"], "p1");
    assert_eq!(args["salience"], 0.75);
}

/// A value outside p0..p3 was storable and then unreachable, because `gtd.tasks(priority=..)`
/// validates its filter against the same list the create path validates writes against.
#[tokio::test]
async fn a_priority_outside_the_levels_is_refused_rather_than_stored() {
    let mut nonsense = json!({"properties": {"priority": "URGENT!!"}});
    let err = normalize_priority_update(&mut nonsense)
        .expect_err("a level outside p0..p3 must not be stored verbatim");
    assert!(
        format!("{err}").contains("p0, p1, p2, p3"),
        "the refusal must name the levels; got: {err}"
    );

    let mut wrong_type = json!({"properties": {"priority": 0}});
    normalize_priority_update(&mut wrong_type)
        .expect_err("a non-string, non-null priority must be refused");
}

/// Clearing the level lands on the salience `gtd.assign` gives a task created without one,
/// rather than leaving the cleared level's value behind as derived state with nothing to
/// derive from.
#[tokio::test]
async fn clearing_the_priority_falls_back_to_the_unprioritized_salience() {
    let mut args = json!({"properties": {"priority": null}});

    normalize_priority_update(&mut args).expect("normalize");

    assert_eq!(args["salience"], 0.5);
}

/// The caller's own value wins over the derived one, which is the same rule the zone above
/// follows: what a caller states explicitly beats what is computed for them.
#[tokio::test]
async fn a_caller_named_salience_survives_the_derivation() {
    let mut args = json!({"properties": {"priority": "p0"}, "salience": 0.1});

    normalize_priority_update(&mut args).expect("normalize");

    assert_eq!(args["properties"]["priority"], "p0");
    assert_eq!(args["salience"], 0.1);
}

/// `salience: null` is the tri-state contract's explicit clear, so a caller who sends it
/// beside a priority is asking for the level without the derived ranking. Reading null as
/// silence would overwrite the clear with the derived number and ignore the instruction.
#[tokio::test]
async fn an_explicit_null_salience_is_a_clear_and_survives_the_derivation() {
    let mut args = json!({"properties": {"priority": "p0"}, "salience": null});

    normalize_priority_update(&mut args).expect("normalize");

    assert_eq!(args["properties"]["priority"], "p0");
    assert!(
        args["salience"].is_null(),
        "an explicit clear must reach the patch; got: {}",
        args["salience"]
    );
}

/// An update that does not mention the level must not mint a salience nobody asked for: doing
/// so would rewrite the ranking of every task touched for an unrelated reason.
#[tokio::test]
async fn an_update_without_a_priority_does_not_touch_the_salience() {
    let mut args = json!({"properties": {"planning_label": "reviewed"}});
    normalize_priority_update(&mut args).expect("normalize");
    assert_eq!(args, json!({"properties": {"planning_label": "reviewed"}}));

    let mut no_properties = json!({"content": "body only"});
    normalize_priority_update(&mut no_properties).expect("normalize");
    assert_eq!(no_properties, json!({"content": "body only"}));
}

/// The wiring: the dispatch site every generic update path calls must run it, or the function
/// is correct and unreachable. Since #2956 that site is
/// `VerbRegistry::prepare_note_update_hook` and not a hook method, so this goes through the
/// registry — calling the normalizer directly would be the unreachable case this guards.
#[tokio::test]
async fn the_update_hook_runs_the_priority_normalization() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    let registry = registry_with_gtd(&runtime);
    let note = task_note_with(json!({"description": "body", "priority": "p3"}));
    let mut args = json!({"properties": {"priority": "p0"}});

    registry
        .prepare_note_update_hook(&runtime, &token, &note, &mut args)
        .await
        .expect("hook");

    assert_eq!(args["properties"]["priority"], "p0");
    assert_eq!(args["salience"], 1.0);
}

#[tokio::test]
async fn normalized_task_update_refuses_a_stale_note_snapshot() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    let mut task = Note::new("local", "task", "original body");
    task.name = Some("original title".to_string());
    task.properties = Some(json!({"description": "original body", "status": "inbox"}));
    let task_id = task.id;
    runtime
        .notes(&token)
        .expect("note store")
        .upsert_note(task)
        .await
        .expect("seed task");

    let snapshot = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    let mut args = json!({"content": "hook-derived body"});
    synchronize_description(&snapshot, &mut args).expect("normalize mirrors");

    // Deterministically land another writer after hook normalization but
    // before persistence from the original snapshot.
    let mut concurrent = snapshot.clone();
    concurrent.name = Some("concurrent title".to_string());
    concurrent.content = "concurrent body".to_string();
    concurrent.properties = Some(json!({"description": "concurrent body", "status": "inbox"}));
    concurrent.updated_at = snapshot.updated_at.saturating_add(10);
    runtime
        .notes(&token)
        .expect("note store")
        .upsert_note(concurrent)
        .await
        .expect("concurrent write");

    let patch = khive_runtime::NotePatch::new(
        None,
        args.get("content")
            .and_then(Value::as_str)
            .map(str::to_string),
        None,
        None,
        args.get("properties").cloned(),
    );
    let err = runtime
        .update_note_from_snapshot_with_embedding_report(&token, snapshot, patch)
        .await
        .expect_err("stale hook snapshot must not overwrite the concurrent task");
    let RuntimeError::Khive(conflict) = &err else {
        panic!("expected structured conflict, got: {err:?}");
    };
    assert_eq!(conflict.kind(), khive_types::ErrorKind::Conflict);
    assert!(conflict.message().contains("retry with fresh state"));

    let persisted = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read persisted task")
        .expect("task exists");
    assert_eq!(persisted.name.as_deref(), Some("concurrent title"));
    assert_eq!(persisted.content, "concurrent body");
    assert_eq!(
        persisted
            .properties
            .as_ref()
            .and_then(|props| props.get("description"))
            .and_then(Value::as_str),
        Some("concurrent body")
    );
}

#[tokio::test]
async fn note_update_refuses_a_same_timestamp_newer_version() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    let mut task = Note::new("local", "task", "original body");
    task.properties = Some(json!({"status": "inbox"}));
    let task_id = task.id;
    runtime
        .notes(&token)
        .expect("note store")
        .upsert_note(task)
        .await
        .expect("seed task");

    let snapshot = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    let mut concurrent = snapshot.clone();
    concurrent.properties = Some(json!({"status": "inbox", "gtd_repair": {"observed": true}}));
    runtime
        .notes(&token)
        .expect("note store")
        .upsert_note(concurrent)
        .await
        .expect("same-timestamp concurrent write");
    let persisted = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    assert_eq!(persisted.updated_at, snapshot.updated_at);
    assert!(persisted.version > snapshot.version);

    for patch in [
        NotePatch::new(None, Some("stale body".into()), None, None, None),
        NotePatch::new(None, None, None, None, None),
    ] {
        runtime
            .update_note_from_snapshot_with_embedding_report(&token, snapshot.clone(), patch)
            .await
            .expect_err("a stale snapshot must not replace or affirm the newer row");
    }
    let after = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    assert_eq!(after.content, "original body");
    assert_eq!(after.properties, persisted.properties);
    assert_eq!(after.version, persisted.version);
}
