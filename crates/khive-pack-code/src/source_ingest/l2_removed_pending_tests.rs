use super::*;

#[tokio::test]
async fn l2_removed_pending_call_does_not_materialize_after_real_parses() {
    for wal in [true, false] {
        for removed in [false, true] {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("fixture");
            manifest(&root, "fixture");
            source(&root, "a.rs", "fn helper(){ghost();}\n");
            let (rt, token) = runtime(&dir.path().join("map.db"), wal);
            let helper = symbol("a", "helper");
            let ghost = symbol("a", "ghost");
            let id = edge_uuid(EdgeRelation::DependsOn, helper, ghost);
            let (_, work) = l2(&rt, &token, &root, 10).await;
            assert_eq!(work.parsed, [root.join("a.rs").canonicalize().unwrap()]);
            assert!(get_entity_opt(&rt, &token, ghost).await.unwrap().is_none());
            assert!(rt
                .graph(&token)
                .unwrap()
                .get_edge(LinkId::from(id))
                .await
                .unwrap()
                .is_none());
            let caller = if removed {
                "fn helper(){}"
            } else {
                "fn helper(){ghost();let _=2;}"
            };
            source(&root, "a.rs", caller);
            let (_, work) = l2(&rt, &token, &root, 20).await;
            assert_eq!(work.parsed, [root.join("a.rs").canonicalize().unwrap()]);
            assert!(get_entity_opt(&rt, &token, ghost).await.unwrap().is_none());
            assert!(rt
                .graph(&token)
                .unwrap()
                .get_edge(LinkId::from(id))
                .await
                .unwrap()
                .is_none());
            let caller = if removed {
                "fn helper(){let _=1;}"
            } else {
                "fn helper(){ghost();let _=3;}"
            };
            source(&root, "a.rs", &format!("{caller}\nfn ghost(){{}}\n"));
            let (_, work) = l2(&rt, &token, &root, 30).await;
            assert_eq!(work.parsed, [root.join("a.rs").canonicalize().unwrap()]);
            for endpoint in [helper, ghost] {
                assert_eq!(
                    stored(&rt, &token, endpoint).await.properties.unwrap()["last_seen_at"],
                    time(30).to_rfc3339()
                );
            }
            let actual = rt
                .graph(&token)
                .unwrap()
                .get_edge(LinkId::from(id))
                .await
                .unwrap();
            assert_eq!(
                actual.is_some(),
                !removed,
                "only the actually retained unresolved call can materialize"
            );
        }
    }
}

fn fixture(wal: bool) -> (TempDir, PathBuf, KhiveRuntime, NamespaceToken) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("fixture");
    manifest(&root, "fixture");
    let (rt, token) = runtime(&dir.path().join("map.db"), wal);
    (dir, root, rt, token)
}

async fn pending_file(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    root: &Path,
    name: &str,
    module: &str,
) -> Value {
    let row = stored(rt, token, module_uuid("fixture", "rust", module)).await;
    row.properties.unwrap()["l2_file_pending"][root
        .join(name)
        .canonicalize()
        .unwrap()
        .display()
        .to_string()]
    .clone()
}
async fn absent(rt: &KhiveRuntime, token: &NamespaceToken, from: Uuid, to: Uuid) -> bool {
    rt.graph(token)
        .unwrap()
        .get_edge(LinkId::from(edge_uuid(EdgeRelation::DependsOn, from, to)))
        .await
        .unwrap()
        .is_none()
}

#[tokio::test]
async fn l2_unchanged_producer_resolves_its_retained_reference() {
    for wal in [true, false] {
        let (_dir, root, rt, token) = fixture(wal);
        source(
            &root,
            "a.rs",
            "fn helper(){crate::b::ghost();crate::b::ghost();}\n",
        );
        let (report, _) = l2(&rt, &token, &root, 10).await;
        assert_eq!(report.l2.unwrap().symbol_dependencies_unresolved, 1);
        assert_eq!(
            pending_file(&rt, &token, &root, "a.rs", "a").await["references"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        source(&root, "b.rs", "fn ghost(){}\n");
        let (_, work) = l2(&rt, &token, &root, 20).await;
        assert_eq!(work.parsed, [root.join("b.rs").canonicalize().unwrap()]);
        assert!(!absent(&rt, &token, symbol("a", "helper"), symbol("b", "ghost")).await);
        assert_eq!(
            pending_file(&rt, &token, &root, "a.rs", "a").await["references"],
            json!([])
        );
        let (_, work) = l2(&rt, &token, &root, 30).await;
        assert!(
            work.parsed.is_empty(),
            "resolved empty entry is accepted coverage"
        );
    }
}

#[tokio::test]
async fn l2_alias_removal_preserves_the_other_physical_producer() {
    for wal in [true, false] {
        let (_dir, root, rt, token) = fixture(wal);
        source(&root, "foo.rs", "fn helper(){self::target::ghost();}\n");
        source(
            &root.join("foo"),
            "mod.rs",
            "fn helper(){self::target::ghost();}\n",
        );
        l2(&rt, &token, &root, 10).await;
        assert_eq!(
            imports::module_path_for_file(&root.join("foo.rs"), &root, "rust"),
            imports::module_path_for_file(&root.join("foo/mod.rs"), &root, "rust")
        );
        source(&root.join("foo"), "mod.rs", "fn helper(){}\n");
        let (_, work) = l2(&rt, &token, &root, 20).await;
        assert_eq!(
            pending_file(&rt, &token, &root, "foo.rs", "foo").await["references"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            pending_file(&rt, &token, &root, "foo/mod.rs", "foo").await["references"],
            json!([])
        );
        assert_eq!(
            work.parsed,
            [root.join("foo/mod.rs").canonicalize().unwrap()]
        );
        source(&root.join("foo"), "target.rs", "fn ghost(){}\n");
        l2(&rt, &token, &root, 30).await;
        assert!(
            !absent(
                &rt,
                &token,
                symbol("foo", "helper"),
                symbol("foo::target", "ghost")
            )
            .await
        );
    }
}

#[tokio::test]
async fn l2_refused_producer_keeps_history_without_current_authority() {
    for wal in [true, false] {
        let (_dir, root, rt, token) = fixture(wal);
        source(&root, "foo.rs", "fn helper(){self::target::survives();}\n");
        source(
            &root.join("foo"),
            "mod.rs",
            "fn helper(){self::target::removed();}\n",
        );
        l2(&rt, &token, &root, 10).await;
        let before = pending_file(&rt, &token, &root, "foo/mod.rs", "foo").await;
        source(&root.join("foo"), "mod.rs", "fn helper(\n");
        source(
            &root.join("foo"),
            "target.rs",
            "fn survives(){} fn removed(){}\n",
        );
        let (report, _) = l2(&rt, &token, &root, 20).await;
        assert_eq!(report.l2.unwrap().symbol_parse_failures, 1);
        assert_eq!(
            pending_file(&rt, &token, &root, "foo/mod.rs", "foo").await,
            before
        );
        assert!(
            !absent(
                &rt,
                &token,
                symbol("foo", "helper"),
                symbol("foo::target", "survives")
            )
            .await
        );
        assert!(
            absent(
                &rt,
                &token,
                symbol("foo", "helper"),
                symbol("foo::target", "removed")
            )
            .await
        );
        let module_id = module_uuid("fixture", "rust", "foo");
        let mut row = stored(&rt, &token, module_id).await;
        let all = row.properties.as_ref().unwrap()["l2_file_pending"].clone();
        row.description = Some(["AKIA", "ABCDEFGHIJKLMNOP"].concat());
        rt.entities(&token)
            .unwrap()
            .upsert_entity(row)
            .await
            .unwrap();
        let parsed = parse_rust_file("fn helper(){}").unwrap();
        let mut state = L2SweepState::default();
        let mut report = CodeSourceIngestReport::default();
        let accepted = persist_l2_file(
            &rt,
            &token,
            "fixture",
            "rust",
            module_id,
            "foo",
            "foo.rs",
            "unversioned",
            "changed",
            Ok(&parsed),
            time(25),
            &root
                .join("foo.rs")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
            &mut state,
            &mut report,
        )
        .await
        .unwrap();
        assert!(
            accepted.is_none(),
            "a refused final coverage stamp is not accepted"
        );
        assert!(report.blocked_count > 0);
        assert_eq!(
            stored(&rt, &token, module_id).await.properties.unwrap()["l2_file_pending"],
            all
        );
        let (report, work) = l2(&rt, &token, &root, 30).await;
        assert!(report.blocked_count > 0);
        assert!(work.parsed.iter().all(|file| file.ends_with("target.rs")));
        assert_eq!(
            stored(&rt, &token, module_id).await.properties.unwrap()["l2_file_pending"],
            all
        );
    }
}

#[tokio::test]
async fn l2_legacy_pending_store_parses_once_before_accepting_empty() {
    for wal in [true, false] {
        for malformed in [false, true] {
            let (_dir, root, rt, token) = fixture(wal);
            source(&root, "a.rs", "fn helper(){crate::b::ghost();}\n");
            l2(&rt, &token, &root, 10).await;
            source(&root, "a.rs", "fn helper(){}\n");
            l2(&rt, &token, &root, 20).await;
            let declaration = stored(&rt, &token, symbol("a", "helper")).await;
            assert_eq!(
                read_l2_unresolved(declaration.properties.as_ref().unwrap()).len(),
                1,
                "legacy history remains recorded"
            );
            let id = module_uuid("fixture", "rust", "a");
            let mut module = stored(&rt, &token, id).await;
            let props = module.properties.as_mut().unwrap().as_object_mut().unwrap();
            if malformed {
                props.insert("l2_file_pending".into(), json!({"invalid": []}));
            } else {
                props.remove("l2_file_pending");
            }
            rt.entities(&token)
                .unwrap()
                .upsert_entity(module)
                .await
                .unwrap();
            source(&root, "b.rs", "fn ghost(){}\n");
            let (_, work) = l2(&rt, &token, &root, 30).await;
            assert!(work
                .parsed
                .contains(&root.join("a.rs").canonicalize().unwrap()));
            assert_eq!(
                pending_file(&rt, &token, &root, "a.rs", "a").await["references"],
                json!([])
            );
            assert!(absent(&rt, &token, symbol("a", "helper"), symbol("b", "ghost")).await);
            let (_, work) = l2(&rt, &token, &root, 40).await;
            assert!(work.parsed.is_empty());
        }
    }
}

#[tokio::test]
async fn l2_alias_shared_hash_cannot_authorize_another_producer() {
    for wal in [true, false] {
        let (_dir, root, rt, token) = fixture(wal);
        source(&root, "foo.rs", "fn helper(){missing();}\n");
        source(&root.join("foo"), "mod.rs", "fn helper(){}\n");
        l2(&rt, &token, &root, 10).await;
        let before = pending_file(&rt, &token, &root, "foo.rs", "foo").await;
        let module = stored(&rt, &token, module_uuid("fixture", "rust", "foo")).await;
        assert_ne!(
            before["content_hash"],
            module.properties.unwrap()["l2_content_hash"]
        );
        source(&root, "foo.rs", "fn helper(){}\n");
        let (_, work) = l2(&rt, &token, &root, 20).await;
        assert!(
            work.parsed
                .contains(&root.join("foo.rs").canonicalize().unwrap()),
            "own accepted hash must match the source"
        );
        let after = pending_file(&rt, &token, &root, "foo.rs", "foo").await;
        assert_ne!(before["content_hash"], after["content_hash"]);
        assert_eq!(after["references"], json!([]));
    }
}

#[tokio::test]
async fn l2_unvisited_root_pending_does_not_borrow_shared_declaration_coverage() {
    for wal in [true, false] {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("A");
        let b = dir.path().join("B");
        for root in [&a, &b] {
            manifest(root, "fixture");
        }
        source(&a, "foo.rs", "fn helper(){crate::target::ghost();}\n");
        source(&b, "foo.rs", "fn helper(){}\n");
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &a, 10).await;
        let before = pending_file(&rt, &token, &a, "foo.rs", "foo").await;
        source(&b, "target.rs", "fn ghost(){}\n");
        l2(&rt, &token, &b, 20).await;
        assert_eq!(pending_file(&rt, &token, &a, "foo.rs", "foo").await, before);
        assert_eq!(
            stored(&rt, &token, symbol("foo", "helper"))
                .await
                .properties
                .unwrap()["last_seen_at"],
            time(20).to_rfc3339()
        );
        assert!(
            absent(
                &rt,
                &token,
                symbol("foo", "helper"),
                symbol("target", "ghost")
            )
            .await
        );
    }
}
