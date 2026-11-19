//! Parameter-name aliases on the task verbs: `gtd.transition(to=)` for `status`,
//! `gtd.assign(content=)` for `description` and `gtd.complete(note=)` for `result`.

mod common;

use common::{assign, pack, rt, Fixture};
use khive_runtime::{KhiveRuntime, Namespace};
use serde_json::{json, Value};

/// The stored row behind a task: its content column and its properties object.
async fn stored(runtime: &KhiveRuntime, full_id: &Value) -> (String, Value) {
    let id: uuid::Uuid = full_id.as_str().unwrap().parse().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let note = runtime
        .notes(&token)
        .expect("note store")
        .get_note(id)
        .await
        .expect("get_note")
        .expect("note must exist");
    (note.content, note.properties.expect("task properties"))
}

/// The parts of a transitioned task's properties that do not depend on the clock.
fn lifecycle_view(properties: &Value) -> Value {
    let entry = &properties["transition_history"][0];
    json!({
        "status": properties["status"],
        "transition_note": properties["transition_note"],
        "from": entry["from"],
        "to": entry["to"],
        "note": entry["note"],
    })
}

/// Read a verb's published help through the public `help=true` path and require
/// that the canonical parameter's description names the alias, while the alias is
/// not published as a parameter of its own.
async fn assert_help_names_alias(verb: &str, canonical: &str, alias: &str) {
    let fixture = pack(rt());
    let help = fixture
        .dispatch(verb, json!({"help": true}))
        .await
        .expect("help envelope");
    let params = help["params"].as_array().expect("params array");
    assert!(!params.iter().any(|p| p["name"] == alias));
    let found = params
        .iter()
        .find(|p| p["name"] == canonical)
        .expect("canonical parameter is published");
    let description = found["description"].as_str().expect("description");
    let phrase = format!("`{alias}` is accepted as an alias for `{canonical}`");
    assert!(
        description.contains(&phrase),
        "{verb}: {canonical} help must contain {phrase}, got: {description}"
    );
}

/// An aliased call succeeds, and the same call with one extra misspelled field is
/// refused with the field named and the canonical names listed.
async fn assert_typo_refused(fixture: &Fixture, verb: &str, args: Value, canonical: &str) {
    fixture
        .dispatch(verb, args.clone())
        .await
        .expect("the aliased call is accepted");
    let mut typo_args = args;
    typo_args["misspelled"] = json!(true);
    let err = fixture
        .dispatch(verb, typo_args)
        .await
        .expect_err("an unknown field is still refused");
    let message = err.to_string();
    assert!(
        message.contains("unknown field `misspelled`"),
        "{verb}: {message}"
    );
    let listed = format!("`{canonical}`");
    assert!(message.contains(&listed), "{verb}: {message}");
}

#[tokio::test]
async fn transition_to_alias_stores_what_status_stores() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let by_status = assign(&fixture, json!({"title": "spelling check"})).await;
    let by_alias = assign(&fixture, json!({"title": "spelling check"})).await;

    let args = json!({"id": by_status["full_id"], "status": "next", "note": "ready"});
    let canonical = fixture
        .dispatch("gtd.transition", args)
        .await
        .expect("canonical transition");
    let args = json!({"id": by_alias["full_id"], "to": "next", "note": "ready"});
    let aliased = fixture
        .dispatch("gtd.transition", args)
        .await
        .expect("aliased transition");
    assert_eq!(aliased["from"], canonical["from"]);
    assert_eq!(aliased["to"], canonical["to"]);
    assert_eq!(aliased["audit_persisted"], canonical["audit_persisted"]);

    let (content_c, props_c) = stored(&runtime, &by_status["full_id"]).await;
    let (content_a, props_a) = stored(&runtime, &by_alias["full_id"]).await;
    assert_eq!(content_a, content_c);
    assert_eq!(lifecycle_view(&props_a), lifecycle_view(&props_c));
    assert_eq!(props_a["status"], "next");
    assert_eq!(props_a["transition_note"], "ready");
}

#[tokio::test]
async fn assign_content_alias_stores_what_description_stores() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let args = json!({"title": "spelling check", "description": "the task body"});
    let canonical = assign(&fixture, args).await;
    let args = json!({"title": "spelling check", "content": "the task body"});
    let aliased = assign(&fixture, args).await;

    let (content_c, props_c) = stored(&runtime, &canonical["full_id"]).await;
    let (content_a, props_a) = stored(&runtime, &aliased["full_id"]).await;
    assert_eq!(content_a, "the task body");
    assert_eq!(content_a, content_c);
    assert_eq!(props_a["description"], "the task body");
    assert_eq!(props_a["description"], props_c["description"]);
    assert_eq!(props_a["status"], props_c["status"]);
    assert_eq!(props_a["priority"], props_c["priority"]);
}

#[tokio::test]
async fn complete_note_alias_stores_what_result_stores() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let by_result = assign(&fixture, json!({"title": "spelling check"})).await;
    let by_note = assign(&fixture, json!({"title": "spelling check"})).await;

    let args = json!({"id": by_result["full_id"], "result": "shipped"});
    let canonical = fixture
        .dispatch("gtd.complete", args)
        .await
        .expect("canonical complete");
    let args = json!({"id": by_note["full_id"], "note": "shipped"});
    let aliased = fixture
        .dispatch("gtd.complete", args)
        .await
        .expect("aliased complete");
    assert_eq!(aliased["from"], canonical["from"]);
    assert_eq!(aliased["to"], canonical["to"]);

    let (_, props_c) = stored(&runtime, &by_result["full_id"]).await;
    let (_, props_a) = stored(&runtime, &by_note["full_id"]).await;
    assert_eq!(props_a["result"], "shipped");
    assert_eq!(props_a["result"], props_c["result"]);
    assert_eq!(props_a["status"], props_c["status"]);
}

#[tokio::test]
async fn transition_refuses_status_and_to_together() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let task = assign(&fixture, json!({"title": "both spellings"})).await;

    for to in [json!("next"), json!("active"), Value::Null] {
        let args = json!({"id": task["full_id"], "status": "next", "to": to});
        let err = fixture
            .dispatch("gtd.transition", args)
            .await
            .expect_err("both spellings must be refused");
        let message = err.to_string();
        assert!(
            message.contains("`to` is an alias for `status`"),
            "got: {message}"
        );
    }

    let (_, props) = stored(&runtime, &task["full_id"]).await;
    assert_eq!(props["status"], "inbox");
    assert!(props.get("transition_history").is_none());
}

#[tokio::test]
async fn assign_refuses_description_and_content_together() {
    let fixture = pack(rt());

    for content in [json!("task body"), json!("other body"), Value::Null] {
        let args = json!({
            "title": "both spellings",
            "description": "task body",
            "content": content,
        });
        let err = fixture
            .dispatch("gtd.assign", args)
            .await
            .expect_err("both spellings must be refused");
        let message = err.to_string();
        assert!(
            message.contains("`content` is an alias for `description`"),
            "got: {message}"
        );
    }

    let tasks = fixture
        .dispatch("gtd.tasks", json!({"status": "inbox"}))
        .await
        .expect("list tasks after the refusals");
    assert_eq!(tasks.as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn complete_refuses_result_and_note_together() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let task = assign(&fixture, json!({"title": "both spellings"})).await;

    for note in [json!("shipped"), json!("reworked"), Value::Null] {
        let args = json!({"id": task["full_id"], "result": "shipped", "note": note});
        let err = fixture
            .dispatch("gtd.complete", args)
            .await
            .expect_err("both spellings must be refused");
        let message = err.to_string();
        assert!(
            message.contains("`note` is an alias for `result`"),
            "got: {message}"
        );
    }

    let (_, props) = stored(&runtime, &task["full_id"]).await;
    assert_eq!(props["status"], "inbox");
    assert!(props.get("result").is_none());
}

#[tokio::test]
async fn transition_help_names_the_to_alias() {
    assert_help_names_alias("gtd.transition", "status", "to").await;
}

#[tokio::test]
async fn assign_help_names_the_content_alias() {
    assert_help_names_alias("gtd.assign", "description", "content").await;
}

#[tokio::test]
async fn complete_help_names_the_note_alias() {
    assert_help_names_alias("gtd.complete", "result", "note").await;
}

#[tokio::test]
async fn aliases_do_not_loosen_the_unknown_field_refusal() {
    let fixture = pack(rt());
    let task = assign(&fixture, json!({"title": "typo check"})).await;
    let id = &task["full_id"];

    let args = json!({"id": id, "to": "next"});
    assert_typo_refused(&fixture, "gtd.transition", args, "status").await;
    let args = json!({"title": "aliased body", "content": "body"});
    assert_typo_refused(&fixture, "gtd.assign", args, "description").await;
    let args = json!({"id": id, "note": "shipped"});
    assert_typo_refused(&fixture, "gtd.complete", args, "result").await;
}
