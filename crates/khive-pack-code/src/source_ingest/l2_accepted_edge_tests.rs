use super::*;

#[tokio::test]
async fn l2_cross_file_impl_producer_stays_warm_after_endpoint_and_impl_removal_edits() {
    for wal in [true, false] {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("fixture");
        manifest(&root, "fixture");
        source(&root, "a.rs", "pub struct Item;\n");
        source(
            &root,
            "b.rs",
            "impl crate::t::Trait for crate::a::Item {}\n",
        );
        source(&root, "t.rs", "pub trait Trait {}\n");
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        let id = edge_uuid(
            EdgeRelation::Implements,
            symbol_uuid("fixture", "rust", "a", "Item", "datatype"),
            symbol_uuid("fixture", "rust", "t", "Trait", "interface"),
        );
        let (first, work) = l2(&rt, &token, &root, 10).await;
        assert_eq!(work.parsed.len(), 3);
        assert_eq!(first.l2.unwrap().symbol_edges_stamped, 1);
        assert_eq!(stamp(&edge(&rt, &token, id).await), time(10).to_rfc3339());
        source(&root, "a.rs", "pub struct Item;\nfn added() {}\n");
        let (second, work) = l2(&rt, &token, &root, 20).await;
        assert_eq!(work.parsed, [root.join("a.rs").canonicalize().unwrap()]);
        assert_eq!(second.l2.unwrap().symbol_edges_stamped, 1);
        assert_eq!(stamp(&edge(&rt, &token, id).await), time(20).to_rfc3339());
        for seconds in [30, 40] {
            let (report, work) = l2(&rt, &token, &root, seconds).await;
            assert!(
                work.parsed.is_empty(),
                "the unchanged impl producer owns its edge"
            );
            assert_eq!(report.l2.unwrap().symbol_edges_stamped, 1);
            assert_eq!(
                stamp(&edge(&rt, &token, id).await),
                time(seconds).to_rfc3339()
            );
        }
        source(&root, "b.rs", "");
        let (report, work) = l2(&rt, &token, &root, 50).await;
        assert_eq!(work.parsed, [root.join("b.rs").canonicalize().unwrap()]);
        assert_eq!(report.l2.unwrap().symbol_edges_stamped, 0);
        let historical = serde_json::to_value(edge(&rt, &token, id).await).unwrap();
        for seconds in [60, 70] {
            let (report, work) = l2(&rt, &token, &root, seconds).await;
            assert!(
                work.parsed.is_empty(),
                "removed impl history must not block reuse"
            );
            assert_eq!(report.l2.unwrap().symbol_edges_stamped, 0);
            assert_eq!(
                serde_json::to_value(edge(&rt, &token, id).await).unwrap(),
                historical
            );
        }
    }
}

#[tokio::test]
async fn l2_late_call_inventory_detects_corruption_then_returns_to_warm_reuse() {
    for wal in [true, false] {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("fixture");
        manifest(&root, "fixture");
        source(&root, "a.rs", "fn caller() { crate::z::helper(); }\n");
        source(&root, "z.rs", "pub fn helper() {}\n");
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        let id = edge_uuid(
            EdgeRelation::DependsOn,
            symbol_uuid("fixture", "rust", "a", "caller", "function"),
            symbol_uuid("fixture", "rust", "z", "helper", "function"),
        );
        let (report, work) = l2(&rt, &token, &root, 10).await;
        assert_eq!(work.parsed.len(), 2);
        assert_eq!(report.l2.unwrap().symbol_edges_stamped, 1);
        let (_, work) = l2(&rt, &token, &root, 20).await;
        assert!(work.parsed.is_empty());
        let mut corrupt = edge(&rt, &token, id).await;
        corrupt.metadata.as_mut().unwrap()["l2_observed_run_id"] =
            json!(Uuid::new_v4().to_string());
        rt.graph(&token)
            .unwrap()
            .upsert_edge(corrupt)
            .await
            .unwrap();
        let (report, work) = l2(&rt, &token, &root, 30).await;
        assert_eq!(work.parsed, [root.join("a.rs").canonicalize().unwrap()]);
        assert_eq!(report.l2.unwrap().symbol_edges_stamped, 1);
        assert_eq!(stamp(&edge(&rt, &token, id).await), time(30).to_rfc3339());
        let (_, work) = l2(&rt, &token, &root, 40).await;
        assert!(
            work.parsed.is_empty(),
            "current-edge repair restores warm reuse"
        );
    }
}

#[tokio::test]
async fn l2_missing_accepted_edge_inventory_parses_once_without_clock_fallback() {
    for wal in [true, false] {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("fixture");
        manifest(&root, "fixture");
        source(&root, "a.rs", "fn caller() { helper(); } fn helper() {}\n");
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &root, 10).await;
        let mut module = stored(&rt, &token, module_uuid("fixture", "rust", "a")).await;
        let entries = module.properties.as_mut().unwrap()["l2_file_pending"]
            .as_object_mut()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries
            .values_mut()
            .next()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("natural_edge_ids")
            .is_some());
        rt.entities(&token)
            .unwrap()
            .upsert_entity(module)
            .await
            .unwrap();
        let (_, work) = l2(&rt, &token, &root, 10).await;
        assert_eq!(work.parsed, [root.join("a.rs").canonicalize().unwrap()]);
        let (report, work) = l2(&rt, &token, &root, 10).await;
        assert!(
            work.parsed.is_empty(),
            "accepted inventory restores warm reuse"
        );
        assert_eq!(report.l2.unwrap().symbol_edges_stamped, 1);
    }
}

#[tokio::test]
async fn l2_refused_impl_inventory_preserves_the_files_accepted_coverage() {
    for wal in [true, false] {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("fixture");
        manifest(&root, "fixture");
        let refused = ["AKIA", "1234567890ABCDEF"].concat();
        let text = format!("impl {refused} for crate::b::Item {{}}\nfn helper(){{}}\n");
        source(&root, "a.rs", &text);
        source(&root, "b.rs", "pub struct Item;\n");
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        let (report, _) = l2(&rt, &token, &root, 10).await;
        assert_eq!(report.blocked_count, 1);
        let module = stored(&rt, &token, module_uuid("fixture", "rust", "a")).await;
        let entry = file_pending::read_file(
            module.properties.as_ref().unwrap(),
            &root
                .join("a.rs")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
        )
        .expect("refused impl must not refuse accepted coverage");
        assert_eq!(entry.declaration_ids, [symbol("a", "helper")]);
        assert!(entry.implementations.is_empty());
        assert_eq!(entry.natural_edge_ids, Some(vec![]));
        let (_, work) = l2(&rt, &token, &root, 20).await;
        assert!(work.parsed.is_empty());
    }
}

#[tokio::test]
async fn l2_inline_module_impl_resolved_after_parsing_stays_warm_on_unchanged_source() {
    for wal in [true, false] {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("fixture");
        manifest(&root, "fixture");
        let text = [
            "pub struct Item;",
            "pub trait Trait {}",
            "mod inner { use super::*; impl Trait for Item {} }",
            "",
        ]
        .join("\n");
        source(&root, "a.rs", &text);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        let id = edge_uuid(
            EdgeRelation::Implements,
            symbol_uuid("fixture", "rust", "a", "Item", "datatype"),
            symbol_uuid("fixture", "rust", "a", "Trait", "interface"),
        );
        let (_, work) = l2(&rt, &token, &root, 10).await;
        assert_eq!(work.parsed.len(), 1);
        // The impl names `Item` and `Trait` from inside `a::inner`, so it only
        // resolves after parsing, against the file module `a`. The producing
        // file's inventory must hold the edge that resolution wrote.
        let module = stored(&rt, &token, module_uuid("fixture", "rust", "a")).await;
        let inventory = file_pending::read_file(
            module.properties.as_ref().unwrap(),
            &root
                .join("a.rs")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
        )
        .expect("file inventory");
        assert!(
            inventory
                .natural_edge_ids
                .as_deref()
                .is_some_and(|ids| ids.contains(&id)),
            "the resolved impl edge belongs to the file that declared the impl"
        );
        let mut previous = project(&rt, &token, "fixture").await;
        assert!(is_completed(&previous));
        let written = edge(&rt, &token, id).await;
        assert_eq!(stamp(&written), time(10).to_rfc3339());
        assert_eq!(
            written.metadata.as_ref().unwrap()["l2_observed_run_id"],
            entry(&previous)["attempted"]["run_id"]
        );
        for seconds in [20, 30] {
            let (_, work) = l2(&rt, &token, &root, seconds).await;
            assert!(work.parsed.is_empty(), "the unchanged file is reused");
            let current = project(&rt, &token, "fixture").await;
            assert!(is_completed(&current));
            assert_ne!(
                entry(&current)["attempted"]["run_id"],
                entry(&previous)["attempted"]["run_id"]
            );
            let refreshed = edge(&rt, &token, id).await;
            assert_eq!(stamp(&refreshed), time(seconds).to_rfc3339());
            assert_eq!(
                refreshed.metadata.as_ref().unwrap()["l2_observed_run_id"],
                entry(&current)["attempted"]["run_id"],
                "an unchanged file must re-observe the impl edge it produced"
            );
            previous = current;
        }
    }
}
