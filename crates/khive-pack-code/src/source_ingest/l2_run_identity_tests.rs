use super::*;

async fn observation(rt: &KhiveRuntime, token: &NamespaceToken) -> L2Observation {
    completed_l2_observation(
        project(rt, token, "fixture")
            .await
            .properties
            .as_ref()
            .unwrap(),
        "rust",
    )
    .expect("actual completed invocation")
}

async fn assert_observed(rt: &KhiveRuntime, token: &NamespaceToken, run: &L2Observation) {
    for name in ["aa", "ah"] {
        assert!(observation_matches(
            stored(rt, token, symbol("a", name))
                .await
                .properties
                .as_ref(),
            run.run_id,
        ));
    }
    for id in [
        natural("a", "aa", "ah"),
        edge_uuid(
            EdgeRelation::Contains,
            module_uuid("fixture", "rust", "a"),
            symbol("a", "aa"),
        ),
    ] {
        assert!(observation_matches(
            edge(rt, token, id).await.metadata.as_ref(),
            run.run_id
        ));
    }
}

#[test]
fn l2_observation_requires_a_canonical_v4_identity() {
    let id = Uuid::parse_str("abcdef12-abcd-4abc-8abc-abcdef123456").unwrap();
    assert!(observation_matches(
        Some(&json!({"l2_observed_run_id":id.to_string()})),
        id
    ));
    for value in [
        Value::Null,
        json!({}),
        json!({"l2_observed_run_id":null}),
        json!({"l2_observed_run_id":42}),
        json!({"l2_observed_run_id":"invalid"}),
        json!({"l2_observed_run_id":id.to_string().to_uppercase()}),
        json!({"l2_observed_run_id":id.simple().to_string()}),
        json!({"l2_observed_run_id":Uuid::new_v4().to_string()}),
        json!({"l2_observed_run_id":Uuid::new_v5(&Uuid::NAMESPACE_OID,b"old").to_string()}),
    ] {
        assert!(!observation_matches(Some(&value), id), "{value}");
    }
    assert!(!observation_matches(None, id));
    for invalid in [
        Uuid::new_v5(&Uuid::NAMESPACE_OID, b"old"),
        Uuid::parse_str("abcdef12-abcd-4abc-1abc-abcdef123456").unwrap(),
    ] {
        assert!(!observation_matches(
            Some(&json!({"l2_observed_run_id":invalid.to_string()})),
            invalid
        ));
    }
    assert!(!observation_matches(
        Some(&json!({"l2_observed_run_id":Uuid::nil().to_string()})),
        Uuid::nil()
    ));
}

#[tokio::test]
async fn l2_equal_and_backward_clocks_do_not_alias_invocations() {
    for wal in [true, false] {
        for middle_times in [vec![10], vec![5, 10]] {
            let dir = TempDir::new().unwrap();
            let a = dir.path().join("A/fixture");
            let b = dir.path().join("B/fixture");
            manifest(&a, "fixture");
            manifest(&b, "fixture");
            source(&a, "a.rs", "fn aa(){ah();} fn ah(){}\n");
            source(&b, "b.rs", "fn ba(){bh();} fn bh(){}\n");
            let (rt, token) = runtime(&dir.path().join("map.db"), wal);
            let (_, work) = l2(&rt, &token, &a, 10).await;
            assert_eq!(work.parsed.len(), 1);
            let first = observation(&rt, &token).await;
            assert_observed(&rt, &token, &first).await;
            let retained = edge(&rt, &token, natural("a", "aa", "ah")).await;
            let mut predecessor = first;
            for seconds in middle_times {
                l2(&rt, &token, &b, seconds).await;
                let middle = observation(&rt, &token).await;
                assert_ne!(middle.run_id, predecessor.run_id);
                assert_eq!(
                    serde_json::to_value(edge(&rt, &token, natural("a", "aa", "ah")).await)
                        .unwrap(),
                    serde_json::to_value(&retained).unwrap()
                );
                predecessor = middle;
            }
            let (_, work) = l2(&rt, &token, &a, 20).await;
            assert_eq!(work.parsed, [a.join("a.rs").canonicalize().unwrap()]);
            let current = observation(&rt, &token).await;
            assert_ne!(current.run_id, predecessor.run_id);
            assert_observed(&rt, &token, &current).await;
            let (_, work) = l2(&rt, &token, &a, 20).await;
            assert!(
                work.parsed.is_empty(),
                "same-root warm reuse works even with identical display time"
            );
            let warm = observation(&rt, &token).await;
            assert_ne!(warm.run_id, current.run_id);
            assert_eq!(warm.sweep_time, current.sweep_time);
            assert_observed(&rt, &token, &warm).await;
        }
    }
}

#[tokio::test]
async fn l2_missing_row_identity_reparses_without_timestamp_fallback() {
    for wal in [true, false] {
        for missing_edge in [false, true] {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("fixture");
            manifest(&root, "fixture");
            source(&root, "a.rs", "fn aa(){ah();} fn ah(){}\n");
            let (rt, token) = runtime(&dir.path().join("map.db"), wal);
            l2(&rt, &token, &root, 10).await;
            if missing_edge {
                let mut row = edge(&rt, &token, natural("a", "aa", "ah")).await;
                row.metadata
                    .as_mut()
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove("l2_observed_run_id");
                rt.graph(&token).unwrap().upsert_edge(row).await.unwrap();
            } else {
                let mut row = stored(&rt, &token, symbol("a", "aa")).await;
                row.properties
                    .as_mut()
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove("l2_observed_run_id");
                rt.entities(&token)
                    .unwrap()
                    .upsert_entity(row)
                    .await
                    .unwrap();
            }
            let (_, work) = l2(&rt, &token, &root, 10).await;
            assert_eq!(
                work.parsed.len(),
                1,
                "missing identity must parse even when display time matches"
            );
            assert_observed(&rt, &token, &observation(&rt, &token).await).await;
            let (_, work) = l2(&rt, &token, &root, 10).await;
            assert!(
                work.parsed.is_empty(),
                "successful identity repair permits warm reuse"
            );
        }
    }
}

#[tokio::test]
async fn l2_natural_identity_preflight_has_no_declaration_effects() {
    for wal in [true, false] {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("fixture");
        manifest(&root, "fixture");
        source(&root, "a.rs", "fn aa(){ah();} fn ah(){}\n");
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &root, 10).await;
        let predecessor = observation(&rt, &token).await;
        let ids = [symbol("a", "aa"), symbol("a", "ah")];
        let before = [
            stored(&rt, &token, ids[0]).await,
            stored(&rt, &token, ids[1]).await,
        ];
        let mut stale = edge(&rt, &token, natural("a", "aa", "ah")).await;
        assert_eq!(stamp(&stale), predecessor.sweep_time);
        stale.metadata.as_mut().unwrap()["l2_observed_run_id"] = json!(Uuid::new_v4().to_string());
        rt.graph(&token).unwrap().upsert_edge(stale).await.unwrap();
        let mut report = CodeSourceIngestReport {
            l2: Some(CodeSourceIngestL2Report::default()),
            ..Default::default()
        };
        assert!(!refresh_l2_declarations(
            &rt,
            &token,
            "fixture",
            "rust",
            "a.rs",
            "unversioned",
            time(20),
            Uuid::new_v4(),
            "a.rs",
            &ids,
            Some(&predecessor),
            &mut report
        )
        .await
        .unwrap());
        for (id, row) in ids.into_iter().zip(before) {
            assert_eq!(
                serde_json::to_value(stored(&rt, &token, id).await).unwrap(),
                serde_json::to_value(row).unwrap()
            );
        }
        assert_eq!(report.fts_indexed, 0);
        assert_eq!(report.l2.unwrap().symbols_updated, 0);
    }
}
