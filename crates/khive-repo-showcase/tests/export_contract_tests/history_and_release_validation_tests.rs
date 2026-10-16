#[test]
fn duplicate_history_identity_and_short_sha_mismatch_are_rejected() {
    let (_dir, request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    let properties: String = history
        .query_row(
            "SELECT properties FROM notes WHERE kind='commit'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut properties: serde_json::Value = serde_json::from_str(&properties).unwrap();
    properties["short_sha"] = serde_json::json!("deadbee");
    history
        .execute(
            "UPDATE notes SET properties=?1 WHERE kind='commit'",
            [properties.to_string()],
        )
        .unwrap();
    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("short_sha"), "{error}");

    properties["short_sha"] =
        serde_json::json!(properties["sha"].as_str().unwrap().get(..7).unwrap());
    history
        .execute(
            "UPDATE notes SET properties=?1 WHERE kind='commit'",
            [properties.to_string()],
        )
        .unwrap();
    insert_note(
        &history,
        "20000000-0000-4000-8000-000000000020",
        "commit",
        "duplicate",
        properties,
    );
    insert_edge(
        &history,
        "20000000-0000-4000-8000-000000000021",
        "20000000-0000-4000-8000-000000000020",
        "10000000-0000-4000-8000-000000000001",
        "annotates",
    );
    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("duplicate commit SHA"), "{error}");
}

#[test]
fn ambiguous_exact_history_project_is_rejected() {
    let (_dir, request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    insert_entity(
        &history,
        "10000000-0000-4000-8000-000000000099",
        "project",
        None,
        "owner/repo-copy",
        serde_json::json!({"repo_slug":"github.com/owner/repo"}),
    );
    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("matched 2 history projects"), "{error}");
}

#[test]
fn negative_pull_request_lead_time_is_rejected() {
    let (_dir, mut request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    insert_note(
        &history,
        "20000000-0000-4000-8000-000000000030",
        "pull_request",
        "time travel",
        serde_json::json!({
            "number": 30,
            "created_at": "2026-08-08T00:00:00Z",
            "merged_at": "2026-08-07T00:00:00Z"
        }),
    );
    insert_edge(
        &history,
        "20000000-0000-4000-8000-000000000031",
        "20000000-0000-4000-8000-000000000030",
        "10000000-0000-4000-8000-000000000001",
        "annotates",
    );
    if let Availability::Available { value } = &mut request.provenance.git_digest {
        value.sources.pull_requests = SourceCoverage::Completed;
    }
    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("precedes creation time"), "{error}");
}

#[test]
fn release_tag_that_does_not_resolve_to_a_commit_is_rejected() {
    let (_dir, request) = fixture();
    fs::write(request.repo_path.join("tag-target.txt"), "not a commit\n").unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&request.repo_path)
        .args(["hash-object", "-w", "tag-target.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let object = String::from_utf8(output.stdout).unwrap();
    git(
        &request.repo_path,
        &["tag", "invalid-target", object.trim()],
    );

    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("rather than a commit"), "{error}");
}

#[test]
fn release_tag_uses_the_target_commit_identity_and_timestamp() {
    let (_dir, request) = fixture();
    git(&request.repo_path, &["tag", "v1", "HEAD"]);
    let head = String::from_utf8(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&request.repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();

    let bundle = export(&request).unwrap();
    assert_eq!(
        bundle.aggregates.cadence_timeline.release_tags.items.len(),
        1
    );
    let tag = &bundle.aggregates.cadence_timeline.release_tags.items[0];
    assert_eq!(tag.target_sha, head.trim());
    assert!(matches!(tag.committed_at, Availability::Available { .. }));
}
