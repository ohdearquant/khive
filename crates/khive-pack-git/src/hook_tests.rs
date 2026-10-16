use super::*;
use khive_runtime::{Namespace, VerbRegistry, VerbRegistryBuilder};
use serde_json::json;

async fn fixture() -> (NamespaceToken, VerbRegistry) {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(crate::GitPack::new(rt.clone()));
    builder
        .with_runtime_event_store(&rt)
        .expect("configure trusted runtime audit store");
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(rt.backend());
    (token, registry)
}

async fn create_commit(registry: &VerbRegistry, sha: &str) -> Value {
    registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "name": sha,
                "content": "a commit",
                "properties": {"sha": sha},
            }),
        )
        .await
        .expect("create commit ok")
}

async fn create_commit_with_short_sha(
    registry: &VerbRegistry,
    sha: &str,
    short_sha: &str,
) -> Value {
    registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "name": sha,
                "content": "a commit",
                "properties": {"sha": sha, "short_sha": short_sha},
            }),
        )
        .await
        .expect("create commit with short_sha ok")
}

async fn create_issue(registry: &VerbRegistry, project_id: Uuid, number: i64) -> Value {
    registry
        .dispatch(
            "create",
            json!({
                "kind": "issue",
                "name": format!("issue {number}"),
                "content": "an issue",
                "properties": {"number": number, "project_id": project_id.to_string()},
            }),
        )
        .await
        .expect("create issue ok")
}

fn valid_sha() -> String {
    "a".repeat(40)
}

fn other_valid_sha() -> String {
    "b".repeat(40)
}

// ---------------------------------------------------------------------
// CommitHook: create rejects an invalid sha (the reported defect's
// create-side half — pinned so the mutation table below has a create
// arm to compare against).
// ---------------------------------------------------------------------

#[tokio::test]
async fn create_commit_refuses_invalid_sha() {
    let (_token, registry) = fixture().await;
    let err = registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "name": "bad",
                "content": "a commit",
                "properties": {"sha": "not-a-sha"},
            }),
        )
        .await
        .expect_err("a non-40-hex sha must be refused on create");
    assert!(
        err.to_string()
            .contains("must be a 40-character hex string"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn create_commit_refuses_reserved_key_for_another_sha() {
    let (_token, registry) = fixture().await;
    let key = format!("git.commit:{}", valid_sha());
    let sha = other_valid_sha();
    let err = registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "key": key,
                "content": "a mismatched commit",
                "properties": {"sha": sha},
            }),
        )
        .await
        .expect_err("a reserved key cannot preclaim a different SHA");
    assert!(
        err.to_string().contains(&key) && err.to_string().contains(&sha),
        "the refusal must name the key and claimed SHA: {err}"
    );
}

#[tokio::test]
async fn update_commit_cannot_rebind_a_reserved_key_to_another_sha() {
    let (_token, registry) = fixture().await;
    let original = valid_sha();
    let key = format!("git.commit:{original}");
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "key": key,
                "content": "an owned commit",
                "properties": {"sha": original},
            }),
        )
        .await
        .expect("a matching reserved key is accepted");
    let id = created["id"].as_str().expect("commit id");
    let changed = other_valid_sha();
    let err = registry
        .dispatch("update", json!({"id": id, "properties": {"sha": changed}}))
        .await
        .expect_err("a property update cannot leave a keyed commit mismatched");
    assert!(
        err.to_string().contains(&key) && err.to_string().contains(&changed),
        "the refusal must name the key and proposed SHA: {err}"
    );
    let stored = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("read unchanged commit");
    assert_eq!(stored["properties"]["sha"], json!(original));
}

// ---------------------------------------------------------------------
// CommitHook: update refuses the same values create refuses, with the
// same message.
// ---------------------------------------------------------------------

#[tokio::test]
async fn update_commit_refuses_invalid_sha_with_create_message() {
    let (_token, registry) = fixture().await;
    let created = create_commit(&registry, &valid_sha()).await;
    let id = created["id"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"sha": "not-a-sha"}}),
        )
        .await
        .expect_err("update must refuse the same invalid sha create refuses");
    assert_eq!(
        err.to_string(),
        format!(
            "invalid input: commit properties.sha {:?} must be a 40-character hex string",
            "not-a-sha"
        ),
        "update's message must match create's exactly; got: {err}"
    );

    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after refused update");
    assert_eq!(
        after["properties"]["sha"],
        json!(valid_sha()),
        "a refused update must leave the stored sha untouched"
    );
}

#[tokio::test]
async fn update_commit_refuses_invalid_parent_element() {
    let (_token, registry) = fixture().await;
    let created = create_commit(&registry, &valid_sha()).await;
    let id = created["id"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"parents": ["short"]}}),
        )
        .await
        .expect_err("update must refuse a non-40-hex parent element");
    assert!(
        err.to_string().contains("commit properties.parents[0]"),
        "error must name the offending index: {err}"
    );
}

#[tokio::test]
async fn update_commit_refuses_invalid_changed_path() {
    let (_token, registry) = fixture().await;
    let created = create_commit(&registry, &valid_sha()).await;
    let id = created["id"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"changed_paths": ["/abs/path"]}}),
        )
        .await
        .expect_err("update must refuse an absolute changed_paths entry");
    assert!(
        err.to_string()
            .contains("commit properties.changed_paths[0]"),
        "error must name the offending index: {err}"
    );
}

#[tokio::test]
async fn update_commit_refuses_short_sha_not_a_prefix_of_current_sha() {
    let (_token, registry) = fixture().await;
    let sha = valid_sha();
    let created = create_commit(&registry, &sha).await;
    let id = created["id"].as_str().unwrap().to_string();

    // short_sha is patched alone; the effective sha to check against is
    // the one already on record, read off the stored note snapshot.
    let err = registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"short_sha": "bbbbbbb"}}),
        )
        .await
        .expect_err("short_sha must still be validated against the ON-RECORD sha");
    assert!(
        err.to_string().contains("must be a non-empty prefix"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn update_commit_refuses_sha_that_conflicts_with_retained_short_sha() {
    let (_token, registry) = fixture().await;
    let sha = valid_sha();
    let short = &sha[..8];
    let created = create_commit_with_short_sha(&registry, &sha, short).await;
    let id = created["id"].as_str().unwrap().to_string();
    let before = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get before refused update");
    let new_sha = other_valid_sha();

    let err = registry
        .dispatch(
            "update",
            json!({
                "id": id,
                "content": "updated commit",
                "properties": {"sha": new_sha, "unrelated": "new value"},
            }),
        )
        .await
        .expect_err("sha-only update must not retain an inconsistent short_sha");
    assert_eq!(
            err.to_string(),
            format!(
                "invalid input: commit properties.short_sha {short:?} must be a non-empty prefix of sha {new_sha:?}"
            )
        );
    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after refused update");
    assert_eq!(
        after, before,
        "a refused patch must leave the note unchanged"
    );
}

// ---------------------------------------------------------------------
// CommitHook: update still accepts a VALID patch — the easy-to-forget
// arm that would catch an over-broad refusal.
// ---------------------------------------------------------------------

#[tokio::test]
async fn update_commit_accepts_valid_sha_and_short_sha_together() {
    let (_token, registry) = fixture().await;
    let old_sha = valid_sha();
    let created = create_commit_with_short_sha(&registry, &old_sha, &old_sha[..8]).await;
    let id = created["id"].as_str().unwrap().to_string();
    let new_sha = other_valid_sha();
    let short = new_sha[..8].to_string();

    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"sha": new_sha, "short_sha": short}}),
        )
        .await
        .expect("a valid sha + matching short_sha update must be accepted");

    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after accepted update");
    assert_eq!(after["properties"]["sha"], json!(other_valid_sha()));
    assert_eq!(after["properties"]["short_sha"], json!(short));
}

#[tokio::test]
async fn update_commit_accepts_sha_when_retained_short_sha_still_matches() {
    let (_token, registry) = fixture().await;
    let sha = valid_sha();
    let short = &sha[..8];
    let created = create_commit_with_short_sha(&registry, &sha, short).await;
    let id = created["id"].as_str().unwrap().to_string();

    for new_sha in [sha.clone(), format!("{short}{}", "b".repeat(32))] {
        registry
            .dispatch("update", json!({"id": id, "properties": {"sha": new_sha}}))
            .await
            .expect("same SHA or a new SHA with the retained prefix must be accepted");
        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after accepted SHA-only update");
        assert_eq!(after["properties"]["sha"], json!(new_sha));
        assert_eq!(after["properties"]["short_sha"], json!(short));
    }
}

#[tokio::test]
async fn update_commit_accepts_sha_without_a_string_short_sha() {
    let (_token, registry) = fixture().await;
    for short in [None, Some(Value::Null), Some(json!(7))] {
        let mut properties = json!({"sha": valid_sha()});
        if let Some(value) = &short {
            properties["short_sha"] = value.clone();
        }
        let created = registry
            .dispatch(
                "create",
                json!({
                    "kind": "commit",
                    "content": "a commit",
                    "properties": properties,
                }),
            )
            .await
            .expect("create accepts missing or non-string short_sha");
        let id = created["id"].as_str().unwrap().to_string();
        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"sha": other_valid_sha()}}),
            )
            .await
            .expect("no string short_sha is retained to conflict with the new SHA");
        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after accepted SHA-only update");
        assert_eq!(after["properties"]["sha"], json!(other_valid_sha()));
        assert_eq!(after["properties"].get("short_sha"), short.as_ref());
    }
}

#[tokio::test]
async fn update_commit_accepts_sha_while_clearing_short_sha() {
    let (_token, registry) = fixture().await;
    let sha = valid_sha();
    let created = create_commit_with_short_sha(&registry, &sha, &sha[..8]).await;
    let id = created["id"].as_str().unwrap().to_string();

    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"sha": other_valid_sha(), "short_sha": null}}),
        )
        .await
        .expect("clearing short_sha must not fall back to the old prefix");
    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after clearing short_sha and replacing SHA");
    assert_eq!(after["properties"]["sha"], json!(other_valid_sha()));
    assert_eq!(after["properties"]["short_sha"], Value::Null);
}

#[tokio::test]
async fn update_commit_accepts_valid_changed_paths() {
    let (_token, registry) = fixture().await;
    let created = create_commit(&registry, &valid_sha()).await;
    let id = created["id"].as_str().unwrap().to_string();

    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"changed_paths": ["a.rs", "b/c.rs"]}}),
        )
        .await
        .expect("a valid changed_paths update must be accepted");

    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after accepted update");
    assert_eq!(
        after["properties"]["changed_paths"],
        json!(["a.rs", "b/c.rs"])
    );
}

// ---------------------------------------------------------------------
// CommitHook: clear semantics.
// ---------------------------------------------------------------------

#[tokio::test]
async fn update_commit_refuses_to_clear_required_sha() {
    let (_token, registry) = fixture().await;
    let created = create_commit(&registry, &valid_sha()).await;
    let id = created["id"].as_str().unwrap().to_string();

    let err = registry
        .dispatch("update", json!({"id": id, "properties": {"sha": null}}))
        .await
        .expect_err("sha is required and must refuse an explicit clear");
    assert!(
        err.to_string().contains("cannot be cleared"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn update_commit_refuses_to_clear_parents_because_create_itself_would() {
    let (_token, registry) = fixture().await;
    let created = create_commit(&registry, &valid_sha()).await;
    let id = created["id"].as_str().unwrap().to_string();

    let err = registry
        .dispatch("update", json!({"id": id, "properties": {"parents": null}}))
        .await
        .expect_err("parents=null is not an array, same as create's own refusal");
    assert!(
        err.to_string()
            .contains("commit properties.parents must be an array"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn update_commit_clears_optional_short_sha_and_changed_paths() {
    let (_token, registry) = fixture().await;
    let sha = valid_sha();
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "name": "clear-me",
                "content": "a commit",
                "properties": {
                    "sha": sha,
                    "short_sha": sha[..8].to_string(),
                    "changed_paths": ["a.rs"],
                },
            }),
        )
        .await
        .expect("create commit with optional fields ok");
    let id = created["id"].as_str().unwrap().to_string();

    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"short_sha": null, "changed_paths": null}}),
        )
        .await
        .expect("clearing optional fields must be accepted");

    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after clear");
    assert_eq!(after["properties"]["short_sha"], Value::Null);
    assert_eq!(after["properties"]["changed_paths"], Value::Null);
}

// ---------------------------------------------------------------------
// CommitHook: an update naming none of these fields is left alone.
// ---------------------------------------------------------------------

#[tokio::test]
async fn update_commit_unrelated_patch_leaves_validated_fields_untouched() {
    let (_token, registry) = fixture().await;
    let sha = valid_sha();
    let short = &sha[..8];
    let created = create_commit_with_short_sha(&registry, &sha, short).await;
    let id = created["id"].as_str().unwrap().to_string();

    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"unrelated": "note"}}),
        )
        .await
        .expect("a patch naming only an unrelated key must be accepted");

    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after unrelated update");
    assert_eq!(after["properties"]["sha"], json!(sha));
    assert_eq!(after["properties"]["short_sha"], json!(short));
    assert_eq!(after["properties"]["unrelated"], json!("note"));
}

// ---------------------------------------------------------------------
// IssueLikeHook: create rejects an invalid number/project_id (pinned for
// the mutation table's create arm).
// ---------------------------------------------------------------------

#[tokio::test]
async fn create_issue_refuses_non_integer_number() {
    let (_token, registry) = fixture().await;
    let project = Uuid::new_v4();
    let err = registry
        .dispatch(
            "create",
            json!({
                "kind": "issue",
                "name": "bad number",
                "content": "an issue",
                "properties": {"number": "five", "project_id": project.to_string()},
            }),
        )
        .await
        .expect_err("a non-integer number must be refused on create");
    assert!(
        err.to_string().contains("must be an integer"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------
// IssueLikeHook: update refuses the same values create refuses.
// ---------------------------------------------------------------------

#[tokio::test]
async fn update_issue_refuses_non_integer_number_with_create_message() {
    let (_token, registry) = fixture().await;
    let project = Uuid::new_v4();
    let created = create_issue(&registry, project, 1).await;
    let id = created["id"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"number": "five"}}),
        )
        .await
        .expect_err("update must refuse the same non-integer number create refuses");
    assert_eq!(
        err.to_string(),
        "invalid input: issue properties.number must be an integer",
        "update's message must match create's exactly; got: {err}"
    );
}

#[tokio::test]
async fn update_issue_refuses_invalid_project_id() {
    let (_token, registry) = fixture().await;
    let project = Uuid::new_v4();
    let created = create_issue(&registry, project, 1).await;
    let id = created["id"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"project_id": "not-a-uuid"}}),
        )
        .await
        .expect_err("update must refuse a non-UUID project_id");
    assert!(
        err.to_string().contains("must be a full UUID"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn update_pull_request_refuses_ungoverned_state_reason_only_for_issue() {
    let (_token, registry) = fixture().await;
    let project = Uuid::new_v4();
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "pull_request",
                "name": "pr",
                "content": "a pull request",
                "properties": {"number": 1, "project_id": project.to_string()},
            }),
        )
        .await
        .expect("create pull_request ok");
    let id = created["id"].as_str().unwrap().to_string();

    // pull_request has no governed set — any non-empty string passes.
    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"state_reason": "closed-by-whoever"}}),
        )
        .await
        .expect("pull_request state_reason only needs non-emptiness");

    let err = registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"state_reason": "   "}}),
        )
        .await
        .expect_err("a whitespace-only state_reason must still be refused");
    assert!(
        err.to_string().contains("must not be empty"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn update_issue_refuses_ungoverned_state_reason() {
    let (_token, registry) = fixture().await;
    let project = Uuid::new_v4();
    let created = create_issue(&registry, project, 1).await;
    let id = created["id"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"state_reason": "closed-by-whoever"}}),
        )
        .await
        .expect_err("an ungoverned issue state_reason must be refused");
    assert!(
        err.to_string().contains("governed values"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------
// IssueLikeHook: update still accepts a VALID patch.
// ---------------------------------------------------------------------

#[tokio::test]
async fn update_issue_accepts_valid_state_reason_and_number() {
    let (_token, registry) = fixture().await;
    let project = Uuid::new_v4();
    let created = create_issue(&registry, project, 1).await;
    let id = created["id"].as_str().unwrap().to_string();

    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"number": 2, "state_reason": "completed"}}),
        )
        .await
        .expect("a valid number + governed state_reason update must be accepted");

    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after accepted update");
    assert_eq!(after["properties"]["number"], json!(2));
    assert_eq!(after["properties"]["state_reason"], json!("completed"));
}

// ---------------------------------------------------------------------
// IssueLikeHook: clear semantics.
// ---------------------------------------------------------------------

#[tokio::test]
async fn update_issue_refuses_to_clear_required_number_and_project_id() {
    let (_token, registry) = fixture().await;
    let project = Uuid::new_v4();
    let created = create_issue(&registry, project, 1).await;
    let id = created["id"].as_str().unwrap().to_string();

    let err = registry
        .dispatch("update", json!({"id": id, "properties": {"number": null}}))
        .await
        .expect_err("number is required and must refuse an explicit clear");
    assert!(err.to_string().contains("cannot be cleared"));

    let err = registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"project_id": null}}),
        )
        .await
        .expect_err("project_id is required and must refuse an explicit clear");
    assert!(err.to_string().contains("cannot be cleared"));
}

#[tokio::test]
async fn update_issue_clears_optional_state_reason() {
    let (_token, registry) = fixture().await;
    let project = Uuid::new_v4();
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "issue",
                "name": "with reason",
                "content": "an issue",
                "properties": {
                    "number": 1,
                    "project_id": project.to_string(),
                    "state_reason": "completed",
                },
            }),
        )
        .await
        .expect("create issue with state_reason ok");
    let id = created["id"].as_str().unwrap().to_string();

    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"state_reason": null}}),
        )
        .await
        .expect("clearing the optional state_reason must be accepted");

    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after clear");
    assert_eq!(after["properties"]["state_reason"], Value::Null);
}

// ---------------------------------------------------------------------
// IssueLikeHook: an update naming none of these fields is left alone.
// ---------------------------------------------------------------------

#[tokio::test]
async fn update_issue_unrelated_patch_leaves_validated_fields_untouched() {
    let (_token, registry) = fixture().await;
    let project = Uuid::new_v4();
    let created = create_issue(&registry, project, 7).await;
    let id = created["id"].as_str().unwrap().to_string();

    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"unrelated": "note"}}),
        )
        .await
        .expect("a patch naming only an unrelated key must be accepted");

    let after = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after unrelated update");
    assert_eq!(after["properties"]["number"], json!(7));
    assert_eq!(
        after["properties"]["project_id"],
        json!(project.to_string())
    );
    assert_eq!(after["properties"]["unrelated"], json!("note"));
}
