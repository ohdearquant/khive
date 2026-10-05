use super::*;
use serde_json::json;

fn runtime() -> KhiveRuntime {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(
        vec!["project".into()],
        vec!["issue".into(), "pull_request".into()],
    );
    runtime
}

async fn project(runtime: &KhiveRuntime, name: &str) -> Uuid {
    runtime
        .create_entity(
            &NamespaceToken::local(),
            "project",
            None,
            name,
            None,
            None,
            vec![],
        )
        .await
        .unwrap()
        .id
}

async fn note(
    runtime: &KhiveRuntime,
    project: Uuid,
    name: &str,
    number: Option<i64>,
    url: Option<&str>,
) -> Note {
    let token = NamespaceToken::local();
    let mut properties = json!({"project_id": project.to_string()});
    if let Some(number) = number {
        properties["number"] = json!(number);
    }
    if let Some(url) = url {
        properties["url"] = json!(url);
    }
    let result = runtime
        .create_note(
            &token,
            "pull_request",
            Some(name),
            name,
            None,
            Some(properties),
            vec![project],
        )
        .await
        .unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .get_note(result.id)
        .await
        .unwrap()
        .unwrap()
}

fn evidence(project: Uuid, into: &Note, from: &Note, url: Option<&str>) -> GitNoteMergeGuard {
    GitNoteMergeGuard {
        project_id: project,
        kind: "pull_request".into(),
        number: 17,
        into_version: into.version,
        from_version: from.version,
        placeholder_url: url.map(str::to_owned),
    }
}

async fn current(runtime: &KhiveRuntime, id: Uuid) -> Note {
    runtime
        .notes(&NamespaceToken::local())
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn merge_rechecks_expected_version_before_any_write() {
    let runtime = runtime();
    let token = NamespaceToken::local();
    let project = project(&runtime, "repository").await;
    let into = note(&runtime, project, "real title", Some(17), None).await;
    let from = note(&runtime, project, "other title", Some(17), None).await;
    let guard = evidence(project, &into, &from, None);
    let changed = runtime
        .update_note_with_embedding_report(
            &token,
            from.id,
            NotePatch {
                content: Some("concurrent writer changed this record".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .0;
    assert_ne!(changed.version, guard.from_version);
    let error = runtime
        .merge_git_note_guarded(&token, into.id, from.id, guard, false)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("changed concurrently"),
        "{error}"
    );
    assert_eq!(current(&runtime, into.id).await, into);
    assert_eq!(current(&runtime, from.id).await, changed);
}

#[tokio::test]
async fn merged_anchor_lineage_normalizes_survivor_and_chains_exact_revision() {
    let runtime = runtime();
    let token = NamespaceToken::local();
    let canonical = project(&runtime, "same repository").await;
    let old = project(&runtime, "same repository").await;
    let into = note(&runtime, old, "rich numbered title", Some(17), None).await;
    let from = note(&runtime, canonical, "first distinct body", Some(17), None).await;
    let third = note(&runtime, canonical, "second distinct body", Some(17), None).await;
    runtime
        .merge_entity(
            &token,
            canonical,
            old,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
    let first = runtime
        .merge_git_note_guarded(
            &token,
            into.id,
            from.id,
            evidence(canonical, &into, &from, None),
            false,
        )
        .await
        .unwrap();
    let mut second_guard = evidence(canonical, &into, &third, None);
    second_guard.into_version = first.kept_version;
    let second = runtime
        .merge_git_note_guarded(&token, into.id, third.id, second_guard, false)
        .await
        .unwrap();
    let survivor = current(&runtime, into.id).await;
    assert_eq!(survivor.version, second.kept_version);
    assert!(second.kept_version > first.kept_version);
    assert!(survivor.content.contains("first distinct body"));
    assert!(survivor.content.contains("second distinct body"));
    let properties = survivor.properties.unwrap();
    assert_eq!(properties["project_id"], canonical.to_string());
    let history = properties["_merge_history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[0]["git_note_repair"]["into_project_id"],
        old.to_string()
    );
    assert!(history
        .iter()
        .any(|entry| entry["merged_from"] == from.id.to_string()));
    assert!(history
        .iter()
        .any(|entry| entry["merged_from"] == third.id.to_string()));
}

#[tokio::test]
async fn project_annotations_are_rechecked_in_transaction() {
    let runtime = runtime();
    let token = NamespaceToken::local();
    let project = project(&runtime, "repository one").await;
    let other = self::project(&runtime, "repository two").await;
    let into = note(&runtime, project, "real title", Some(17), None).await;
    let from = note(&runtime, project, "duplicate", Some(17), None).await;
    let guard = evidence(project, &into, &from, None);
    // This edit changes graph evidence without advancing either note revision.
    runtime
        .link(&token, from.id, other, EdgeRelation::Annotates, 1.0, None)
        .await
        .unwrap();
    let error = runtime
        .merge_git_note_guarded(&token, into.id, from.id, guard, false)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("annotation membership"),
        "{error}"
    );
    assert_eq!(current(&runtime, into.id).await, into);
    assert_eq!(current(&runtime, from.id).await, from);
}

#[tokio::test]
async fn placeholder_url_uniqueness_is_rechecked_after_preview() {
    let runtime = runtime();
    let token = NamespaceToken::local();
    let project = project(&runtime, "repository").await;
    let url = "https://github.com/example/repository/pull/17";
    let into = note(&runtime, project, "real title", Some(17), Some(url)).await;
    let from = note(&runtime, project, "[pull_request]", None, Some(url)).await;
    let guard = evidence(project, &into, &from, Some(url));
    let preview = runtime
        .merge_git_note_guarded(&token, into.id, from.id, guard.clone(), true)
        .await
        .unwrap();
    assert!(preview.summary.dry_run);
    assert_eq!(current(&runtime, into.id).await, into);
    assert_eq!(current(&runtime, from.id).await, from);
    let contradictory = note(&runtime, project, "another number", Some(18), Some(url)).await;
    let error = runtime
        .merge_git_note_guarded(&token, into.id, from.id, guard, false)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("more than one numbered group"),
        "{error}"
    );
    assert_eq!(current(&runtime, contradictory.id).await, contradictory);
    assert_eq!(current(&runtime, from.id).await, from);
}

#[tokio::test]
async fn exact_url_adopts_only_proven_numberless_placeholder() {
    let runtime = runtime();
    let token = NamespaceToken::local();
    let project = project(&runtime, "repository").await;
    let url = "https://github.com/example/repository/pull/17";
    let into = note(&runtime, project, "real title", Some(17), Some(url)).await;
    let from = note(&runtime, project, "[pull_request]", None, Some(url)).await;
    let merged = runtime
        .merge_git_note_guarded(
            &token,
            into.id,
            from.id,
            evidence(project, &into, &from, Some(url)),
            false,
        )
        .await
        .unwrap();
    assert_eq!(merged.summary.removed_id, from.id);
    let survivor = current(&runtime, into.id).await;
    assert_eq!(number(&survivor), Some(17));
    assert_eq!(survivor.name.as_deref(), Some("real title"));
    assert_eq!(survivor.version, merged.kept_version);
}

#[test]
fn forge_url_syntax_does_not_accept_normalizing_or_secret_bearing_inputs() {
    assert!(valid_git_note_forge_url(
        "https://github.com/example/repository/pull/17"
    ));
    assert!(valid_git_note_forge_url(
        "HTTPS://github.com/example/repository/pull/17"
    ));
    for invalid in [
        "",
        "file:///tmp/repo",
        "https://user:secret@example.com/a",
        "https://example.com/a?token=value",
        "https://example.com/a#17",
        " https://example.com/a",
        "https://example.com/",
        "https://@example.com/path",
        "https:///example.com/path",
        "https:example.com/path",
        "https:\\example.com/a",
        "https://example.com/\na",
    ] {
        assert!(!valid_git_note_forge_url(invalid), "{invalid:?}");
    }
}

#[tokio::test]
async fn malformed_history_refuses_without_losing_source_provenance() {
    let runtime = runtime();
    let token = NamespaceToken::local();
    let project = project(&runtime, "repository").await;
    let into = note(&runtime, project, "real title", Some(17), None).await;
    let from = note(&runtime, project, "duplicate", Some(17), None).await;
    let changed = runtime
        .update_note_with_embedding_report(
            &token,
            into.id,
            NotePatch {
                properties: Some(json!({"_merge_history": "legacy malformed field"})),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .0;
    let error = runtime
        .merge_git_note_guarded(
            &token,
            into.id,
            from.id,
            evidence(project, &changed, &from, None),
            false,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("malformed merge history"),
        "{error}"
    );
    assert_eq!(current(&runtime, into.id).await, changed);
    assert_eq!(current(&runtime, from.id).await, from);
}
