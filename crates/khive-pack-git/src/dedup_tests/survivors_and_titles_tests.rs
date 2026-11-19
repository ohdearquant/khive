#[tokio::test]
async fn stranded_duplicates_merge_into_one_note_that_keeps_the_twins_name() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    let unnamed = f.pr(old, None, 7, "Fix the parser", "same body").await;
    let named = f
        .pr(
            other,
            Some("#7 Fix the parser"),
            7,
            "Fix the parser",
            "same body",
        )
        .await;
    f.retire(old).await;
    f.retire(other).await;

    let report = f.apply().await;

    assert!(report.success, "{:?}", report.refused_merges);
    assert_eq!(report.planned.len(), 1);
    assert_eq!(report.applied.len(), 1);
    let survivor = f.current(report.planned[0].survivor.id).await;
    let donor_id = if survivor.id == unnamed.id {
        named.id
    } else {
        unnamed.id
    };
    assert_eq!(survivor.name.as_deref(), Some("#7 Fix the parser"));
    assert_eq!(
        property(&survivor, "project_id"),
        json!(f.canonical.to_string())
    );
    assert!(f.is_deleted(donor_id).await);
}

#[tokio::test]
async fn a_survivor_without_a_name_takes_its_twins_name_and_keeps_the_canonical_home() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let unnamed = f
        .pr(f.canonical, None, 9, "Add the cache", "cache body")
        .await;
    let named = f
        .pr(
            old,
            Some("#9 Add the cache"),
            9,
            "Add the cache",
            "cache body",
        )
        .await;
    f.retire(old).await;

    let plan = f.plan().await;
    let group = &plan.report().planned[0];
    assert_eq!(group.survivor.id, unnamed.id, "the canonical home wins");
    assert_eq!(group.renamed_to.as_deref(), Some("#9 Add the cache"));
    assert_eq!(group.rehomed_from, None);
    assert_eq!(plan.report().totals["pull_request"].survivors_renamed, 1);
    assert_eq!(plan.report().totals["pull_request"].notes_rehomed, 0);

    let report = f.apply().await;

    assert!(report.success);
    let survivor = f.current(unnamed.id).await;
    assert_eq!(survivor.name.as_deref(), Some("#9 Add the cache"));
    assert!(f.is_deleted(named.id).await);
}

#[tokio::test]
async fn a_survivor_under_a_retired_anchor_is_rehomed_to_the_canonical_project() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    f.pr(old, Some("#3 t"), 3, "t", "body").await;
    f.pr(other, Some("#3 t"), 3, "t", "body").await;
    f.retire(old).await;
    f.retire(other).await;

    let plan = f.plan().await;
    let group = plan.report().planned[0].clone();
    assert!(group.rehomed_from.is_some());
    assert_eq!(plan.report().totals["pull_request"].notes_rehomed, 1);

    f.apply().await;

    let survivor = f.current(group.survivor.id).await;
    assert_eq!(
        property(&survivor, "project_id"),
        json!(f.canonical.to_string())
    );
    let history = property(&survivor, "_merge_history");
    assert_eq!(
        history[0]["annotation"]["from_project_id"],
        json!(group.donors[0].note.project_id.to_string())
    );
    assert_eq!(
        history[0]["annotation"]["into_project_id"],
        json!(group.survivor.project_id.to_string())
    );
}

#[tokio::test]
async fn titles_that_differ_refuse_the_group_and_name_both_titles() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    f.pr(old, Some("#4 First"), 4, "First", "a").await;
    f.pr(old, Some("#4 Second"), 4, "Second", "b").await;
    f.retire(old).await;
    let before = f.snapshot().await;

    let report = f.apply().await;

    assert!(report.planned.is_empty());
    assert_eq!(report.refused_groups.len(), 1);
    let reason = &report.refused_groups[0].reason;
    assert!(
        reason.contains("\"First\"") && reason.contains("\"Second\""),
        "{reason}"
    );
    let titles: BTreeSet<_> = report.refused_groups[0]
        .notes
        .iter()
        .filter_map(|note| note.title.clone())
        .collect();
    assert_eq!(titles, BTreeSet::from(["First".into(), "Second".into()]));
    assert_eq!(
        f.snapshot().await,
        before,
        "a refused group changes nothing"
    );
}

#[tokio::test]
async fn a_note_without_a_title_refuses_the_group() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    f.pr(old, Some("#5 T"), 5, "T", "a").await;
    f.note_of("pull_request", old, Some("#5"), "b", json!({"number": 5}))
        .await;
    f.retire(old).await;

    let report = f.plan().await.report().clone();

    assert_eq!(report.refused_groups[0].reason, "a note has no title");
}
