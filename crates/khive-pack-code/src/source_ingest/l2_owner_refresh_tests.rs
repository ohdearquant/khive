use super::*;

const CALL_AND_IMPL: &str = "fn helper() {}\nfn caller() { helper(); }\nstruct Item;\ntrait Trait {}\nimpl Trait for Item {}\n";

fn call_id(owner: &str, module: &str, target: &str) -> Uuid {
    edge_uuid(
        EdgeRelation::DependsOn,
        symbol_uuid(owner, "rust", module, "caller", "function"),
        symbol_uuid(owner, "rust", module, target, "function"),
    )
}

fn impl_id(owner: &str, module: &str) -> Uuid {
    edge_uuid(
        EdgeRelation::Implements,
        symbol_uuid(owner, "rust", module, "Item", "datatype"),
        symbol_uuid(owner, "rust", module, "Trait", "interface"),
    )
}

async fn assert_retained_declarations_stamp(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    owner: &str,
    module: &str,
    seconds: i64,
) {
    let row = stored(rt, token, module_uuid(owner, "rust", module)).await;
    let ids = read_declaration_ids(&row.properties.unwrap()["declaration_ids"])
        .expect("retained declaration identities");
    assert_eq!(
        ids.len(),
        4,
        "both functions, datatype and interface retained"
    );
    for id in ids {
        let declaration = stored(rt, token, id).await;
        let properties = declaration.properties.unwrap();
        assert_eq!(properties["source_project"], owner);
        assert_eq!(properties["language"], "rust");
        assert_eq!(
            properties["last_seen_at"],
            time(seconds).to_rfc3339(),
            "every retained current declaration must match this completed sweep"
        );
    }
}

async fn assert_completed_stamp(rt: &KhiveRuntime, token: &NamespaceToken, seconds: i64) {
    let owner = project(rt, token, "proj").await;
    assert!(is_completed(&owner));
    assert_eq!(
        owner.properties.unwrap()["sweep_clock"]["rust"],
        time(seconds).to_rfc3339()
    );
}

#[tokio::test]
async fn l2_manifest_shared_owner_reobserves_unchanged_natural_edges() {
    for wal in [true, false] {
        for all_tiers in [false, true] {
            let dir = TempDir::new().expect("directory");
            let a = dir.path().join("A/proj");
            let b = dir.path().join("B/proj");
            manifest(&a, "proj");
            manifest(&b, "proj");
            source(&a, "alpha.rs", CALL_AND_IMPL);
            source(&b, "beta.rs", CALL_AND_IMPL);
            let (rt, token) = runtime(&dir.path().join("map.db"), wal);
            let (first, first_work) = ingest(&rt, &token, &a, 10, all_tiers, all_tiers, true).await;
            assert_eq!(
                first
                    .expect("first ingest")
                    .l2
                    .unwrap()
                    .symbol_edges_stamped,
                2
            );
            assert_eq!(first_work.parsed.len(), 1);
            let ids = [call_id("proj", "alpha", "helper"), impl_id("proj", "alpha")];
            let originals = [
                edge(&rt, &token, ids[0]).await,
                edge(&rt, &token, ids[1]).await,
            ];
            let module_id = module_uuid("proj", "rust", "alpha");
            let before = stored(&rt, &token, module_id).await;
            let old = before.properties.as_ref().unwrap();
            let (middle, middle_work) =
                ingest(&rt, &token, &b, 20, all_tiers, all_tiers, true).await;
            assert_eq!(
                middle
                    .expect("second root")
                    .l2
                    .unwrap()
                    .symbol_edges_stamped,
                2
            );
            assert_eq!(middle_work.parsed.len(), 1);
            assert!(is_completed(&project(&rt, &token, "proj").await));
            for id in ids {
                assert_eq!(stamp(&edge(&rt, &token, id).await), time(10).to_rfc3339());
            }
            let (last, work) = ingest(&rt, &token, &a, 30, all_tiers, all_tiers, true).await;
            let last = last.expect("unchanged first root");
            for (id, original) in ids.into_iter().zip(originals) {
                let current = edge(&rt, &token, id).await;
                assert_eq!(
                    stamp(&current),
                    time(30).to_rfc3339(),
                    "observed shared-owner edge must match the latest owner sweep"
                );
                assert_eq!(current.id, original.id);
                assert_eq!(current.created_at, original.created_at);
                assert_eq!(current.source_id, original.source_id);
                assert_eq!(current.target_id, original.target_id);
                assert_eq!(current.relation, original.relation);
                assert!(current.deleted_at.is_none());
                assert_eq!(
                    current.metadata.as_ref().unwrap().get("l2_evidence"),
                    original.metadata.as_ref().unwrap().get("l2_evidence")
                );
            }
            assert_eq!(work.parsed, [a.join("alpha.rs").canonicalize().unwrap()]);
            assert_eq!(last.l2.unwrap().symbol_edges_stamped, 2);
            assert_retained_declarations_stamp(&rt, &token, "proj", "alpha", 30).await;
            // The unvisited other root remains historical for this invocation.
            assert_retained_declarations_stamp(&rt, &token, "proj", "beta", 20).await;
            let after = stored(&rt, &token, module_id).await;
            let new = after.properties.as_ref().unwrap();
            for key in [
                "l2_content_hash",
                "declaration_ids",
                "l2_scanner_identity_version",
            ] {
                assert_eq!(old[key], new[key], "unchanged source identity stays stable");
            }
            let owner = project(&rt, &token, "proj").await;
            assert!(is_completed(&owner));
            assert_eq!(
                owner.properties.unwrap()["sweep_clock"]["rust"],
                time(30).to_rfc3339()
            );
            let (reused, reuse_work) =
                ingest(&rt, &token, &a, 40, all_tiers, all_tiers, true).await;
            assert!(reused.is_ok());
            assert!(
                reuse_work.parsed.is_empty(),
                "completed same-root reuse remains available"
            );
            assert_retained_declarations_stamp(&rt, &token, "proj", "alpha", 40).await;
            assert_retained_declarations_stamp(&rt, &token, "proj", "beta", 20).await;
            assert_completed_stamp(&rt, &token, 40).await;
            // Distinct retained declarations are stale at every alternating-root
            // predecessor, so the real parse cost persists beyond the first return.
            for (root, module, seconds) in [
                (&b, "beta", 50),
                (&a, "alpha", 60),
                (&b, "beta", 70),
                (&a, "alpha", 80),
            ] {
                let (report, work) =
                    ingest(&rt, &token, root, seconds, all_tiers, all_tiers, true).await;
                assert_eq!(report.unwrap().l2.unwrap().symbol_edges_stamped, 2);
                assert_eq!(
                    work.parsed,
                    [root.join(format!("{module}.rs")).canonicalize().unwrap()],
                    "each alternating shared-owner root must reparse actual source"
                );
                assert_retained_declarations_stamp(&rt, &token, "proj", module, seconds).await;
                let owner = project(&rt, &token, "proj").await;
                assert!(is_completed(&owner));
                assert_eq!(
                    owner.properties.unwrap()["sweep_clock"]["rust"],
                    time(seconds).to_rfc3339()
                );
            }
            let (reused, work) = ingest(&rt, &token, &a, 90, all_tiers, all_tiers, true).await;
            assert_eq!(reused.unwrap().l2.unwrap().symbol_edges_stamped, 2);
            assert!(work.parsed.is_empty(), "one warm repeat restores reuse");
            assert_retained_declarations_stamp(&rt, &token, "proj", "alpha", 90).await;
            assert_completed_stamp(&rt, &token, 90).await;
        }
    }
}

#[tokio::test]
async fn l2_shared_owner_cost_boundary_preserves_empty_aliased_and_distinct_run_identity() {
    for wal in [true, false] {
        for content in ["", CALL_AND_IMPL] {
            let dir = TempDir::new().expect("directory");
            let a = dir.path().join("A/proj");
            let b = dir.path().join("B/proj");
            manifest(&a, "proj");
            manifest(&b, "proj");
            source(&a, "shared.rs", content);
            source(&b, "shared.rs", content);
            let (rt, token) = runtime(&dir.path().join("map.db"), wal);
            let (_, first) = l2(&rt, &token, &a, 10).await;
            assert_eq!(first.parsed.len(), 1);
            let (_, middle) = l2(&rt, &token, &b, 20).await;
            let (_, last) = l2(&rt, &token, &a, 30).await;
            assert_eq!(
                middle.parsed,
                [b.join("shared.rs").canonicalize().unwrap()],
                "a new physical producer must establish its own accepted coverage"
            );
            assert!(
                last.parsed.is_empty(),
                "root alternation alone does not forbid reuse"
            );
            let module = stored(&rt, &token, module_uuid("proj", "rust", "shared")).await;
            let ids = read_declaration_ids(&module.properties.unwrap()["declaration_ids"])
                .expect("retained identities");
            assert_eq!(ids.len(), if content.is_empty() { 0 } else { 4 });
            if !content.is_empty() {
                assert_retained_declarations_stamp(&rt, &token, "proj", "shared", 30).await;
                for id in [
                    call_id("proj", "shared", "helper"),
                    impl_id("proj", "shared"),
                ] {
                    assert_eq!(stamp(&edge(&rt, &token, id).await), time(30).to_rfc3339());
                }
            }
            assert_completed_stamp(&rt, &token, 30).await;
            for (root, seconds) in [(&b, 40), (&a, 50)] {
                let (_, work) = l2(&rt, &token, root, seconds).await;
                assert!(
                    work.parsed.is_empty(),
                    "accepted aliased producers stay warm"
                );
                assert_completed_stamp(&rt, &token, seconds).await;
            }
        }

        let dir = TempDir::new().expect("directory");
        let a = dir.path().join("A/proj");
        let b = dir.path().join("B/proj");
        manifest(&a, "proj");
        manifest(&b, "proj");
        source(&a, "alpha.rs", CALL_AND_IMPL);
        source(&b, "beta.rs", CALL_AND_IMPL);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        let (_, first) = l2(&rt, &token, &a, 10).await;
        let (_, middle) = l2(&rt, &token, &b, 10).await;
        assert_eq!(first.parsed.len(), 1);
        assert_eq!(middle.parsed.len(), 1);
        for (root, module) in [(&a, "alpha"), (&b, "beta")] {
            let (_, work) = l2(&rt, &token, root, 10).await;
            assert!(
                work.parsed.len() == 1,
                "equal display times cannot reuse a different invocation observation"
            );
            assert_retained_declarations_stamp(&rt, &token, "proj", module, 10).await;
        }
        assert_completed_stamp(&rt, &token, 10).await;
    }
}

#[tokio::test]
async fn l2_partial_shared_declarations_reobserve_cross_file_natural_calls() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let a = dir.path().join("A/proj");
        let b = dir.path().join("B/proj");
        manifest(&a, "proj");
        manifest(&b, "proj");
        source(&a, "alpha.rs", "pub fn helper() {}\n");
        source(&b, "beta.rs", "pub fn unrelated() {}\n");
        let common = "fn caller() { crate::alpha::helper(); }\n";
        source(&a, "common.rs", common);
        source(&b, "common.rs", common);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        let source_id = symbol_uuid("proj", "rust", "common", "caller", "function");
        let target_id = symbol_uuid("proj", "rust", "alpha", "helper", "function");
        let id = edge_uuid(EdgeRelation::DependsOn, source_id, target_id);
        let (first, work) = l2(&rt, &token, &a, 10).await;
        assert_eq!(work.parsed.len(), 2);
        assert_eq!(first.l2.unwrap().symbol_edges_stamped, 1);
        let original = edge(&rt, &token, id).await;
        l2(&rt, &token, &b, 20).await;
        assert_eq!(stamp(&edge(&rt, &token, id).await), time(10).to_rfc3339());
        assert_eq!(
            stored(&rt, &token, source_id).await.properties.unwrap()["last_seen_at"],
            time(20).to_rfc3339()
        );
        let (last, work) = l2(&rt, &token, &a, 30).await;
        let observed = edge(&rt, &token, id).await;
        assert_eq!(
            stamp(&observed),
            time(30).to_rfc3339(),
            "partially shared caller must re-observe its exclusive target edge"
        );
        assert_eq!(observed.id, original.id);
        assert_eq!(observed.created_at, original.created_at);
        assert_eq!(
            observed.metadata.unwrap()["l2_evidence"],
            original.metadata.unwrap()["l2_evidence"]
        );
        assert_eq!(
            work.parsed.len(),
            2,
            "both distinct target and shared caller reparse"
        );
        assert_eq!(last.l2.unwrap().symbol_edges_stamped, 1);
        for declaration in [source_id, target_id] {
            assert_eq!(
                stored(&rt, &token, declaration).await.properties.unwrap()["last_seen_at"],
                time(30).to_rfc3339()
            );
        }
        assert_completed_stamp(&rt, &token, 30).await;
        let (warm, work) = l2(&rt, &token, &a, 40).await;
        assert!(
            work.parsed.is_empty(),
            "fully observed cross-file warm repeat reuses"
        );
        assert_eq!(warm.l2.unwrap().symbol_edges_stamped, 1);
        assert_eq!(stamp(&edge(&rt, &token, id).await), time(40).to_rfc3339());
        for declaration in [source_id, target_id] {
            assert_eq!(
                stored(&rt, &token, declaration).await.properties.unwrap()["last_seen_at"],
                time(40).to_rfc3339()
            );
        }
        assert_completed_stamp(&rt, &token, 40).await;
    }
}

#[tokio::test]
async fn l2_distinct_manifest_owners_keep_completed_reuse_authority() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let a = dir.path().join("A/proj");
        let b = dir.path().join("B/proj");
        manifest(&a, "owner_a");
        manifest(&b, "owner_b");
        source(&a, "alpha.rs", CALL_AND_IMPL);
        source(&b, "beta.rs", CALL_AND_IMPL);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &a, 10).await;
        l2(&rt, &token, &b, 20).await;
        let (report, work) = l2(&rt, &token, &a, 30).await;
        assert!(
            work.parsed.is_empty(),
            "a distinct owner must retain its fast path"
        );
        assert_eq!(report.l2.unwrap().symbol_edges_stamped, 2);
        assert_retained_declarations_stamp(&rt, &token, "owner_a", "alpha", 30).await;
        for id in [
            call_id("owner_a", "alpha", "helper"),
            impl_id("owner_a", "alpha"),
        ] {
            assert_eq!(stamp(&edge(&rt, &token, id).await), time(30).to_rfc3339());
        }
        for id in [
            call_id("owner_b", "beta", "helper"),
            impl_id("owner_b", "beta"),
        ] {
            assert_eq!(stamp(&edge(&rt, &token, id).await), time(20).to_rfc3339());
        }
        assert_eq!(
            project(&rt, &token, "owner_a").await.properties.unwrap()["sweep_clock"]["rust"],
            time(30).to_rfc3339()
        );
        assert_eq!(
            project(&rt, &token, "owner_b").await.properties.unwrap()["sweep_clock"]["rust"],
            time(20).to_rfc3339()
        );
    }
}

#[tokio::test]
async fn l2_shared_owner_reparse_preserves_removed_deleted_and_manual_history() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let a = dir.path().join("A/proj");
        let b = dir.path().join("B/proj");
        manifest(&a, "proj");
        manifest(&b, "proj");
        source(&a, "alpha.rs", "fn helper() {}\nfn removed() {}\nfn deleted() {}\nfn caller() { helper(); removed(); deleted(); }\n");
        source(&b, "beta.rs", CALL_AND_IMPL);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &a, 10).await;
        let removed_id = call_id("proj", "alpha", "removed");
        let deleted_id = call_id("proj", "alpha", "deleted");
        let removed = edge(&rt, &token, removed_id).await;
        let graph = rt.graph(&token).unwrap();
        assert!(edge(&rt, &token, deleted_id).await.deleted_at.is_none());
        assert!(graph
            .delete_edge(
                LinkId::from(deleted_id),
                khive_storage::types::DeleteMode::Soft
            )
            .await
            .unwrap());
        let deleted = edge(&rt, &token, deleted_id).await;
        assert!(
            deleted.deleted_at.is_some(),
            "public soft-delete must persist a tombstone"
        );
        assert!(graph
            .get_edge(LinkId::from(deleted_id))
            .await
            .unwrap()
            .is_none());
        source(
            &a,
            "alpha.rs",
            "fn helper() {}\nfn removed() {}\nfn deleted() {}\nfn caller() { helper(); }\n",
        );
        l2(&rt, &token, &a, 20).await;
        l2(&rt, &token, &b, 30).await;
        let (report, work) = l2(&rt, &token, &a, 40).await;
        assert_eq!(
            work.parsed.len(),
            1,
            "shared owner must re-observe actual references"
        );
        assert_eq!(report.l2.unwrap().symbol_edges_stamped, 1);
        let observed_id = call_id("proj", "alpha", "helper");
        assert_eq!(
            stamp(&edge(&rt, &token, observed_id).await),
            time(40).to_rfc3339()
        );
        let mut manual = edge(&rt, &token, observed_id).await;
        manual.source_id = symbol_uuid("proj", "rust", "alpha", "helper", "function");
        manual.target_id = symbol_uuid("proj", "rust", "alpha", "removed", "function");
        manual.id = LinkId::from(edge_uuid(
            EdgeRelation::DependsOn,
            manual.source_id,
            manual.target_id,
        ));
        manual.metadata = Some(
            json!({"l2_derived":false,"language":"rust","last_seen_at":time(40).to_rfc3339(),"manual":"keep",
                "l2_observed_run_id": completed_l2_observation(project(&rt,&token,"proj").await.properties.as_ref().unwrap(),"rust").unwrap().run_id.to_string()}),
        );
        rt.graph(&token)
            .unwrap()
            .upsert_edge(manual.clone())
            .await
            .unwrap();
        // Exercise the existing edge refresh's defensive filters directly with
        // actual observed endpoint IDs, including an accepted repeated time.
        // Every history edge sits in the inventory the refresh reads, so only
        // the refresh's own checks keep it untouched. A derived edge from the
        // previous completed sweep is the control the refresh does restamp.
        let mut control = edge(&rt, &token, observed_id).await;
        control.source_id = symbol_uuid("proj", "rust", "alpha", "helper", "function");
        control.target_id = symbol_uuid("proj", "rust", "alpha", "deleted", "function");
        control.id = LinkId::from(edge_uuid(
            EdgeRelation::DependsOn,
            control.source_id,
            control.target_id,
        ));
        graph.upsert_edge(control.clone()).await.unwrap();
        let module = stored(&rt, &token, module_uuid("proj", "rust", "alpha")).await;
        let ids = read_declaration_ids(&module.properties.unwrap()["declaration_ids"]).unwrap();
        let mut state = L2SweepState::default();
        state.mark_current_declarations(&ids, "proj", "rust");
        state.unchanged_declarations.extend(ids);
        state.current_natural_edge_ids.extend([
            removed_id,
            deleted_id,
            Uuid::from(manual.id),
            Uuid::from(control.id),
        ]);
        let mut previous = PreviousL2SweepStamps::new();
        previous.stamps.insert(
            L2OwnerKey {
                source_project: "proj".into(),
                language: "rust".into(),
            },
            completed_l2_observation(
                project(&rt, &token, "proj")
                    .await
                    .properties
                    .as_ref()
                    .unwrap(),
                "rust",
            ),
        );
        let before_refresh = edge(&rt, &token, deleted_id).await;
        assert!(
            before_refresh.deleted_at.is_some(),
            "tombstone must be stored immediately before refresh"
        );
        assert_eq!(
            serde_json::to_value(&before_refresh).unwrap(),
            serde_json::to_value(&deleted).unwrap(),
            "persisted deleted history must already match the captured tombstone"
        );
        assert!(graph
            .get_edge(LinkId::from(deleted_id))
            .await
            .unwrap()
            .is_none());
        refresh_unchanged_l2_edges(
            &rt,
            &token,
            time(40),
            &previous,
            &mut state,
            &mut CodeSourceIngestReport::default(),
        )
        .await
        .unwrap();
        let restamped = edge(&rt, &token, Uuid::from(control.id)).await;
        assert_eq!(
            restamped.metadata.as_ref().unwrap()["l2_observed_run_id"],
            json!(previous.run_id.to_string()),
            "the inventory is read, so an edge that passes every check is restamped"
        );
        let history = [removed, deleted, manual];
        for original in &history {
            let current = edge(&rt, &token, Uuid::from(original.id)).await;
            assert_eq!(
                serde_json::to_value(current).unwrap(),
                serde_json::to_value(original).unwrap(),
                "unobserved historical edge must remain byte-equivalent"
            );
        }
        for seconds in [50, 60] {
            let (report, work) = l2(&rt, &token, &a, seconds).await;
            assert!(
                work.parsed.is_empty(),
                "retained removed history must not prevent accepted coverage reuse"
            );
            assert_eq!(
                stamp(&edge(&rt, &token, observed_id).await),
                time(seconds).to_rfc3339()
            );
            for original in &history {
                assert_eq!(
                    serde_json::to_value(edge(&rt, &token, Uuid::from(original.id)).await).unwrap(),
                    serde_json::to_value(original).unwrap()
                );
            }
            assert_eq!(report.l2.unwrap().symbol_edges_stamped, 1);
        }
        assert!(rt
            .graph(&token)
            .unwrap()
            .get_edge(LinkId::from(deleted_id))
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn l2_declaration_preflight_refuses_an_old_observation_before_writes() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("proj");
        manifest(&root, "proj");
        source(&root, "alpha.rs", CALL_AND_IMPL);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &root, 10).await;
        let id = symbol_uuid("proj", "rust", "alpha", "helper", "function");
        let mut old = stored(&rt, &token, id).await;
        let predecessor = completed_l2_observation(
            project(&rt, &token, "proj")
                .await
                .properties
                .as_ref()
                .unwrap(),
            "rust",
        )
        .unwrap();
        old.properties.as_mut().unwrap()["l2_observed_run_id"] = json!(Uuid::new_v4().to_string());
        rt.entities(&token)
            .unwrap()
            .upsert_entity(old.clone())
            .await
            .unwrap();
        let old = stored(&rt, &token, id).await;
        let fresh_id = symbol_uuid("proj", "rust", "alpha", "caller", "function");
        let fresh_before = stored(&rt, &token, fresh_id).await;
        let mut report = CodeSourceIngestReport {
            l2: Some(CodeSourceIngestL2Report::default()),
            ..Default::default()
        };
        let result = refresh_l2_declarations(
            &rt,
            &token,
            "proj",
            "rust",
            "alpha.rs",
            "unversioned",
            time(20),
            Uuid::new_v4(),
            "alpha.rs",
            &[fresh_id, id],
            &[],
            Some(&predecessor),
            &mut report,
        )
        .await
        .unwrap();
        assert!(
            !result,
            "a stale second declaration must refuse before refreshing the first"
        );
        assert_eq!(
            serde_json::to_value(stored(&rt, &token, fresh_id).await).unwrap(),
            serde_json::to_value(fresh_before).unwrap(),
        );
        assert_eq!(report.fts_indexed, 0);
        assert_eq!(report.l2.unwrap().symbols_updated, 0);
        assert_eq!(
            serde_json::to_value(stored(&rt, &token, id).await).unwrap(),
            serde_json::to_value(old).unwrap()
        );
    }
}

#[tokio::test]
async fn l2_final_edge_audit_reobserves_a_row_changed_during_guarded_declaration_refresh() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("proj");
        manifest(&root, "proj");
        source(&root, "alpha.rs", CALL_AND_IMPL);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &root, 10).await;
        let caller = symbol_uuid("proj", "rust", "alpha", "caller", "function");
        let id = call_id("proj", "alpha", "helper");
        let pause = Pause::new(Point::AfterEntityRead(caller));
        let mut future = Box::pin(PAUSE.scope(Arc::clone(&pause), l2(&rt, &token, &root, 20)));
        await_pause(&mut future, &pause).await;
        let mut changed = edge(&rt, &token, id).await;
        changed.metadata.as_mut().unwrap()["l2_observed_run_id"] =
            json!(Uuid::new_v4().to_string());
        changed.updated_at = time(11);
        rt.graph(&token)
            .unwrap()
            .upsert_edge(changed)
            .await
            .unwrap();
        let (report, work) = resume_paused(&mut future, &pause).await;
        assert_eq!(
            stamp(&edge(&rt, &token, id).await),
            time(20).to_rfc3339(),
            "fresh final edge audit must force actual observation after an edge change"
        );
        assert_eq!(work.parsed.len(), 1);
        assert_eq!(report.l2.unwrap().symbol_edges_stamped, 2);
        assert_retained_declarations_stamp(&rt, &token, "proj", "alpha", 20).await;
        assert_completed_stamp(&rt, &token, 20).await;
    }
}

#[tokio::test]
async fn l2_declaration_freshness_is_rechecked_after_a_guarded_rebase() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("proj");
        manifest(&root, "proj");
        source(&root, "alpha.rs", CALL_AND_IMPL);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &root, 10).await;
        let id = symbol_uuid("proj", "rust", "alpha", "caller", "function");
        let predecessor = completed_l2_observation(
            project(&rt, &token, "proj")
                .await
                .properties
                .as_ref()
                .unwrap(),
            "rust",
        )
        .unwrap();
        let pause = Pause::new(Point::AfterEntityRead(id));
        let mut refresh = Box::pin(PAUSE.scope(Arc::clone(&pause), async {
            let mut report = CodeSourceIngestReport::default();
            refresh_l2_declarations(
                &rt,
                &token,
                "proj",
                "rust",
                "alpha.rs",
                "unversioned",
                time(20),
                Uuid::new_v4(),
                "alpha.rs",
                &[id],
                &[],
                Some(&predecessor),
                &mut report,
            )
            .await
        }));
        await_pause(&mut refresh, &pause).await;
        let mut changed = stored(&rt, &token, id).await;
        let foreign = Uuid::new_v4().to_string();
        changed.properties.as_mut().unwrap()["l2_observed_run_id"] = json!(foreign);
        changed.updated_at = ts(time(11));
        rt.entities(&token)
            .unwrap()
            .upsert_entity(changed)
            .await
            .unwrap();
        let result = resume_paused(&mut refresh, &pause).await;
        assert!(
            !result.expect("guarded freshness check"),
            "rebased stale observation cannot authorize reuse"
        );
        assert_eq!(
            stored(&rt, &token, id).await.properties.unwrap()["l2_observed_run_id"],
            foreign
        );
    }
}
