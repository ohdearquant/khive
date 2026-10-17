#[tokio::test]
async fn preview_changes_nothing_and_a_second_apply_finds_nothing_to_do() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    f.pr(old, Some("#1 t"), 1, "t", "alpha").await;
    f.pr(old, Some("#1 t"), 1, "t", "beta").await;
    f.pr(old, Some("#2 t"), 2, "t", "only one").await;
    f.retire(old).await;
    let before = f.snapshot().await;

    let preview = run_dedup(&f.runtime, &f.token, f.options()).await.unwrap();
    assert!(!preview.apply_requested);
    assert_eq!(preview.planned.len(), 1);
    assert_eq!(f.snapshot().await, before, "a preview writes nothing");

    let mut options = f.options();
    options.apply = true;
    let first = run_dedup(&f.runtime, &f.token, options.clone())
        .await
        .unwrap();
    assert!(first.success);
    assert_eq!(first.applied.len(), 1);
    let events = f.merge_events().await;

    let second = run_dedup(&f.runtime, &f.token, options).await.unwrap();
    assert!(second.success);
    assert!(second.planned.is_empty() && second.applied.is_empty());
    assert_eq!(f.merge_events().await, events);
}

#[tokio::test]
async fn totals_account_for_every_unnamed_note_and_group_them_by_anchor() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    let kept = f.anchor().await;
    f.pr(old, None, 1, "t", "a").await;
    f.pr(other, Some("#1 t"), 1, "t", "a").await;
    f.pr(old, None, 2, "u", "a").await;
    f.pr(other, None, 2, "v", "a").await;
    f.pr(old, None, 3, "t", "a").await;
    f.pr(kept, None, 4, "t", "a").await;
    f.pr(old, None, 4, "t", "a").await;
    f.note_of("pull_request", old, None, "a", json!({"title": "t"}))
        .await;
    f.note_of(
        "issue",
        other,
        None,
        "a",
        json!({"number": 1, "title": "t"}),
    )
    .await;
    for anchor in [old, other, kept] {
        f.retire(anchor).await;
    }
    let mut options = f.options();
    options.refused_anchors.insert(kept);

    let report = plan_dedup(&f.runtime, &f.token, options)
        .await
        .unwrap()
        .report()
        .clone();

    let prs = &report.totals["pull_request"];
    assert_eq!(prs.candidates, 8);
    assert_eq!(prs.unnamed, 7);
    assert_eq!(prs.groups_found, 2);
    assert_eq!((prs.groups_planned, prs.groups_refused), (1, 1));
    assert_eq!(prs.unnamed_merged_away, 1);
    assert_eq!(prs.unnamed_survivors, 0);
    assert_eq!(prs.unnamed_in_refused_groups, 2);
    assert_eq!(prs.unnamed_unchanged, 4);
    assert_eq!(
        prs.unnamed,
        prs.unnamed_merged_away
            + prs.unnamed_survivors
            + prs.unnamed_in_refused_groups
            + prs.unnamed_unchanged
    );
    let anchors: BTreeMap<(String, String), usize> = report
        .unnamed_by_anchor
        .iter()
        .map(|row| {
            (
                (row.kind.clone(), row.project_id.clone()),
                row.unnamed_notes,
            )
        })
        .collect();
    assert_eq!(anchors[&("pull_request".into(), old.to_string())], 5);
    assert_eq!(anchors[&("pull_request".into(), other.to_string())], 1);
    assert_eq!(anchors[&("pull_request".into(), kept.to_string())], 1);
    assert_eq!(anchors[&("issue".into(), other.to_string())], 1);
    let lines = report.summary_lines().join("\n");
    assert!(
        lines.contains("pull_request: unnamed 7 = merged away"),
        "{lines}"
    );
    assert!(lines.contains("titles differ"), "{lines}");
}
