#[tokio::test]
async fn notes_without_proof_or_a_number_are_left_alone() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let second_project = f.project().await;
    f.pr(old, Some("#1 t"), 1, "t", "body").await;
    f.pr(old, Some("#1 t"), 1, "t", "body").await;
    // The note's project_id is in the lineage, but its annotation is gone.
    let unannotated = f.pr(old, Some("#2 t"), 2, "t", "body").await;
    // Annotates the canonical project and a second live project.
    let ambiguous = f.pr(old, Some("#3 t"), 3, "t", "body").await;
    f.runtime
        .link(
            &f.token,
            ambiguous.id,
            second_project,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();
    // No usable number.
    f.pr(old, Some("zero"), 0, "t", "body").await;
    f.note_of(
        "pull_request",
        old,
        Some("text"),
        "b",
        json!({"number": "12"}),
    )
    .await;
    f.retire(old).await;
    f.execute(
        "DELETE FROM graph_edges WHERE source_id = ?1 AND relation = 'annotates'",
        vec![SqlValue::Text(unannotated.id.to_string())],
    )
    .await;

    let report = f.plan().await.report().clone();

    assert_eq!(
        unchanged_count(&report, "no live annotation to the canonical project"),
        1
    );
    assert_eq!(
        unchanged_count(&report, "also annotates another live project"),
        1
    );
    assert_eq!(unchanged_count(&report, "no positive integer number"), 2);
    assert_eq!(
        report.planned.iter().map(|g| g.number).collect::<Vec<_>>(),
        vec![1]
    );
}

#[tokio::test]
async fn a_note_annotating_the_canonical_project_from_an_unrelated_project_is_left_alone() {
    let f = Fixture::new().await;
    let stray = f.project().await;
    let note = f
        .note_of(
            "pull_request",
            stray,
            Some("#6 t"),
            "body",
            json!({"number": 6, "title": "t"}),
        )
        .await;
    f.runtime
        .link(
            &f.token,
            note.id,
            f.canonical,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();

    let report = f.plan().await.report().clone();

    assert_eq!(
        unchanged_count(
            &report,
            "project_id is not in the canonical project's lineage"
        ),
        1
    );
    assert!(report.planned.is_empty());
}

#[tokio::test]
async fn issues_and_pull_requests_with_one_number_are_separate_groups() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    f.pr(old, Some("#5 t"), 5, "t", "body").await;
    f.note_of(
        "issue",
        old,
        Some("#5 t"),
        "body",
        json!({"number": 5, "title": "t"}),
    )
    .await;
    f.retire(old).await;

    let report = f.plan().await.report().clone();

    assert!(report.planned.is_empty());
    assert_eq!(report.totals["issue"].candidates, 1);
    assert_eq!(report.totals["pull_request"].candidates, 1);
}

#[tokio::test]
async fn equal_bodies_are_kept_once_and_distinct_bodies_are_all_retained() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let a = f.pr(old, Some("#1 t"), 1, "t", "alpha").await;
    f.pr(old, Some("#1 t"), 1, "t", "alpha").await;
    f.pr(old, Some("#2 t"), 2, "t", "alpha").await;
    f.pr(old, Some("#2 t"), 2, "t", "beta").await;
    f.pr(old, Some("#2 t"), 2, "t", "alpha").await;
    f.retire(old).await;

    let report = f.apply().await;

    assert!(report.success, "{:?}", report.refused_merges);
    let by_number = |number: i64| {
        report
            .planned
            .iter()
            .find(|group| group.number == number)
            .unwrap()
    };
    assert_eq!(by_number(1).donors[0].body, BodyMerge::KeepSurvivor);
    assert_eq!(f.current(by_number(1).survivor.id).await.content, "alpha");
    let two = by_number(2);
    let bodies: Vec<_> = two.donors.iter().map(|d| d.body).collect();
    assert_eq!(
        bodies.iter().filter(|b| **b == BodyMerge::Append).count(),
        1
    );
    let merged = f.current(two.survivor.id).await.content;
    assert_eq!(merged.matches("alpha").count(), 1);
    assert_eq!(merged.matches("beta").count(), 1);
    assert!(merged.contains("\n\n---\n\n"));
    let _ = a;
}
