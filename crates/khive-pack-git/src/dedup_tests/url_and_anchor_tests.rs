#[tokio::test]
async fn two_different_non_empty_urls_refuse_the_group() {
    let report = plan_with_urls([
        Some("https://github.com/a/b/pull/1"),
        Some("https://git.example.org/b/pull/1"),
    ])
    .await;
    assert!(report.planned.is_empty());
    assert_eq!(report.refused_groups.len(), 1);
    assert_eq!(report.refused_groups[0].reason, URL_REASON);
}

#[tokio::test]
async fn an_empty_url_beside_a_non_empty_one_is_not_proof_and_refuses_the_group() {
    for empty in [None, Some(""), Some("  ")] {
        let report = plan_with_urls([Some("https://github.com/a/b/pull/1"), empty]).await;
        assert!(report.planned.is_empty(), "{empty:?}");
        assert_eq!(report.refused_groups.len(), 1, "{empty:?}");
        assert_eq!(report.refused_groups[0].reason, URL_REASON, "{empty:?}");
    }
}

#[tokio::test]
async fn equal_urls_and_two_missing_urls_do_not_block_the_merge() {
    let same = "https://github.com/a/b/pull/1";
    for urls in [[Some(same), Some(same)], [None, Some("")]] {
        let report = plan_with_urls(urls).await;
        assert!(report.refused_groups.is_empty(), "{urls:?}");
        assert_eq!(report.planned.len(), 1, "{urls:?}");
    }
}

#[tokio::test]
async fn a_refused_anchor_keeps_its_notes_out_of_every_merge() {
    let f = Fixture::new().await;
    let kept = f.anchor().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    let guarded = f.pr(kept, Some("#8 t"), 8, "t", "body").await;
    f.pr(old, Some("#8 t"), 8, "t", "body").await;
    f.pr(other, Some("#8 t"), 8, "t", "body").await;
    f.pr(kept, Some("#9 t"), 9, "t", "body").await;
    f.pr(old, Some("#9 t"), 9, "t", "body").await;
    for anchor in [kept, old, other] {
        f.retire(anchor).await;
    }
    let mut options = f.options();
    options.refused_anchors.insert(kept);

    let plan = plan_dedup(&f.runtime, &f.token, options).await.unwrap();
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();

    assert!(report.success);
    assert_eq!(report.refused_anchors, vec![kept]);
    assert_eq!(report.planned.len(), 1, "only #8 keeps two eligible notes");
    assert_eq!(report.planned[0].number, 8);
    assert_eq!(f.current(guarded.id).await, guarded);
    assert_eq!(
        unchanged_count(&report, "note belongs to a refused anchor"),
        2
    );
    assert_eq!(
        unchanged_count(&report, "no duplicate in the group"),
        1,
        "#9 has one eligible note left"
    );
}

#[tokio::test]
async fn refusing_the_canonical_anchor_prevents_preview_and_apply() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    let canonical_note = f.pr(f.canonical, Some("#8 t"), 8, "t", "body").await;
    let old_note = f.pr(old, Some("#8 t"), 8, "t", "body").await;
    let other_note = f.pr(other, Some("#8 t"), 8, "t", "body").await;
    f.retire(old).await;
    f.retire(other).await;
    let ordinary = f.plan().await;
    assert_eq!(ordinary.report().planned.len(), 1);
    assert_eq!(ordinary.report().planned[0].survivor.id, canonical_note.id);
    assert_eq!(ordinary.report().planned[0].donors.len(), 2);
    let before = f.snapshot().await;

    for apply in [false, true] {
        let mut options = f.options();
        options.refused_anchors.insert(f.canonical);
        options.apply = apply;
        let error = match run_dedup(&f.runtime, &f.token, options).await {
            Ok(_) => panic!("canonical anchor must be refused before planning: apply={apply}"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            format!("cannot refuse canonical project anchor {}", f.canonical)
        );
        assert_eq!(f.snapshot().await, before, "apply={apply}");
        let mut canonical_live = 0;
        for id in [canonical_note.id, old_note.id, other_note.id] {
            assert!(!f.is_deleted(id).await);
            let current = f.current(id).await;
            canonical_live +=
                usize::from(property(&current, "project_id") == json!(f.canonical.to_string()));
        }
        assert_eq!(canonical_live, 1, "apply={apply}");
    }
}
