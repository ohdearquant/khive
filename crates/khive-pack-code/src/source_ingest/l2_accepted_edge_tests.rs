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
