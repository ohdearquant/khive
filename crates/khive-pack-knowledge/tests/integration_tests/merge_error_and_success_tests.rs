#[tokio::test]
async fn issue_558_merge_missing_and_validation_errors_unchanged() {
    let runtime = rt();
    let f = pack_via_registry(runtime.clone());
    let entity = issue_558_ordinary(&f, "concept", "into").await;
    let note = issue_558_ordinary(&f, "observation", "into").await;
    issue_558_edge(&f, &note, &entity, true).await;
    let atom = issue_558_private(&runtime, &f, "atom").await;
    let domain = issue_558_private(&runtime, &f, "domain").await;
    let missing = "00000000-0000-4000-8000-000000000558";
    for kind in [
        None,
        Some("entity"),
        Some("concept"),
        Some("note"),
        Some("observation"),
    ] {
        let is_note = matches!(kind, Some("note" | "observation"));
        let ordinary = if is_note { &note } else { &entity };
        // With `kind` omitted there is no substrate to name: the id resolved to
        // nothing, so the refusal is the id (#2858). The explicit-kind arms are
        // unchanged, and they are what keeps this a message decision rather than
        // a change to which side is reported.
        let expected = if kind.is_none() {
            missing.to_string()
        } else if is_note {
            "not found in this namespace".into()
        } else {
            format!("entity {missing}")
        };
        for missing_into in [true, false] {
            let (into, from) = if missing_into {
                (missing, ordinary.as_str())
            } else {
                (ordinary.as_str(), missing)
            };
            let arm = format!("missing/{kind:?}/into={missing_into}");
            let error =
                issue_558_unchanged_error(&runtime, &f, issue_558_args(into, from, kind), &arm)
                    .await;
            issue_558_not_found(error, &expected, &arm);
        }
        let arm = format!("earlier-missing-into/{kind:?}");
        let error =
            issue_558_unchanged_error(&runtime, &f, issue_558_args(missing, &atom, kind), &arm)
                .await;
        issue_558_not_found(error, &expected, &arm);
    }
    for (kind, wrong, ordinary, expected) in [
        ("concept", &note, &entity, format!("entity {note}")),
        (
            "observation",
            &entity,
            &note,
            "not found in this namespace".into(),
        ),
        ("document", &entity, &entity, format!("document {entity}")),
        ("insight", &note, &note, format!("insight {note}")),
    ] {
        let error = issue_558_unchanged_error(
            &runtime,
            &f,
            issue_558_args(wrong, ordinary, Some(kind)),
            kind,
        )
        .await;
        issue_558_not_found(error, &expected, kind);
    }
    // These exact pre-read errors are independent of the resolver result mapping.
    let validation_cases = [
        (
            json!({"kind": "edge"}),
            "merge(kind=\"edge\") is unsupported",
        ),
        (
            json!({"kind": "event"}),
            "events are immutable — create/update/delete are not permitted",
        ),
        (
            json!({"kind": "proposal"}),
            "proposal events are immutable and cannot be merged",
        ),
        (
            json!({"strategy": "invalid"}),
            "unknown strategy \"invalid\"; use prefer_into | prefer_from | union",
        ),
        (
            json!({"content_strategy": "invalid"}),
            "unknown content_strategy \"invalid\"; use append | prefer_into | prefer_from",
        ),
        (
            json!({"winner_id": atom}),
            "bad params: duplicate field `into_id`",
        ),
        (
            json!({"into_id": "fffffffffffffffffffffffffffffff"}),
            "no record matches prefix: \"fffffffffffffffffffffffffffffff\"",
        ),
    ];
    for (extra, expected) in validation_cases {
        let mut args = issue_558_args(&atom, &entity, None);
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = issue_558_unchanged_error(&runtime, &f, args, expected).await;
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref message) if message == expected),
            "{expected}: {error:?}"
        );
    }
    // Freeze baseline parser/name oracles using the resolver-free registry over the same rows.
    let baseline = pack(runtime.clone());
    for extra in [
        json!({"kind": "unknown-558"}),
        json!({"into_id": "unmatched-558-name"}),
        json!({"into_id": "not-a-uuid"}),
    ] {
        let mut args = issue_558_args(&atom, &entity, None);
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let expected =
            issue_558_unchanged_error(&runtime, &baseline, args.clone(), "baseline parser").await;
        let error = issue_558_unchanged_error(&runtime, &f, args, "parser precedence").await;
        assert_eq!(format!("{error:?}"), format!("{expected:?}"));
    }
    for private in [&atom, &domain] {
        f.dispatch("delete", json!({"id": private}))
            .await
            .expect("soft delete private fixture");
        for kind in [
            None,
            Some("entity"),
            Some("concept"),
            Some("note"),
            Some("observation"),
        ] {
            let is_note = matches!(kind, Some("note" | "observation"));
            let ordinary = if is_note { &note } else { &entity };
            // A soft-deleted operand resolves to nothing, so with `kind`
            // omitted the refusal names the id and no substrate, whichever
            // side it is on (#2858).
            let expected = if kind.is_none() {
                private.to_string()
            } else if is_note {
                "not found in this namespace".into()
            } else {
                format!("entity {private}")
            };
            for private_into in [true, false] {
                let (into, from) = if private_into {
                    (private, ordinary)
                } else {
                    (ordinary, private)
                };
                let arm = format!("soft-deleted/{private}/{kind:?}/{private_into}");
                let error =
                    issue_558_unchanged_error(&runtime, &f, issue_558_args(into, from, kind), &arm)
                        .await;
                issue_558_not_found(error, &expected, &arm);
            }
        }
    }
    let consumed = issue_558_ordinary(&f, "concept", "from").await;
    f.dispatch("merge", issue_558_args(&entity, &consumed, None))
        .await
        .expect("ordinary merge for continuity control");
    for (into, from) in [(&consumed, &entity), (&entity, &consumed)] {
        let error = issue_558_unchanged_error(
            &runtime,
            &f,
            issue_558_args(into, from, None),
            "consumed entity",
        )
        .await;
        issue_558_not_found(
            error,
            &format!("{consumed} was merged into {entity}; query the kept id"),
            "consumed entity",
        );
    }
}

#[tokio::test]
async fn issue_558_merge_substrate_success_and_dry_run_unchanged() {
    for note in [false, true] {
        for dry_run in [false, true] {
            for (into_key, from_key) in [
                ("into_id", "from_id"),
                ("winner_id", "loser_id"),
                ("target_id", "source_id"),
            ] {
                for force in [false, true] {
                    let arm = format!("note={note}/dry_run={dry_run}/{into_key}/force={force}");
                    let runtime = rt();
                    let f = pack_via_registry(runtime.clone());
                    let kind = if note { "observation" } else { "concept" };
                    let into = issue_558_ordinary(&f, kind, "into").await;
                    let from = issue_558_ordinary(&f, kind, "from").await;
                    let anchor = issue_558_ordinary(&f, "concept", "anchor").await;
                    issue_558_edge(&f, &from, &anchor, note).await;
                    let before = issue_558_snapshot(&runtime).await;
                    let summary = f
                        .dispatch(
                            "merge",
                            json!({into_key: into, from_key: from,
                        "kind": kind, "dry_run": dry_run, "force": force, "strategy": "union"}),
                        )
                        .await
                        .expect(&arm);
                    assert_eq!(summary["kept_id"], into, "{arm}");
                    assert_eq!(summary["removed_id"], from, "{arm}");
                    assert_eq!(summary["dry_run"], dry_run, "{arm}");
                    assert_eq!(summary["edges_rewired"], 1, "{arm}");
                    assert_eq!(summary["properties_merged"], 1, "{arm}");
                    assert_eq!(summary["content_appended"], true, "{arm}");
                    let after = issue_558_snapshot(&runtime).await;
                    if dry_run {
                        assert_eq!(after, before, "{arm}: dry-run mutation");
                    } else {
                        let kept = f
                            .dispatch("get", json!({"id": into}))
                            .await
                            .expect("kept row");
                        let text = kept[if note { "content" } else { "description" }]
                            .as_str()
                            .expect("kept text");
                        assert!(
                            text.contains("into") && text.contains("from"),
                            "{arm}: {text}"
                        );
                        assert_eq!(kept["properties"]["into"], "into", "{arm}");
                        assert_eq!(kept["properties"]["from"], "from", "{arm}");
                        let table = if note { "notes" } else { "entities" };
                        let tombstones = issue_558_rows(&runtime, &format!("SELECT id FROM {table} WHERE id = '{from}' AND deleted_at IS NOT NULL")).await;
                        assert_eq!(
                            tombstones.len(),
                            1,
                            "{arm}: consumed row must be tombstoned"
                        );
                        let edges = issue_558_rows(&runtime, &format!("SELECT id FROM graph_edges WHERE source_id = '{into}' AND target_id = '{anchor}' AND deleted_at IS NULL")).await;
                        assert_eq!(edges.len(), 1, "{arm}: incident edge must be rewired");
                        assert_eq!(after["merge_events"].as_array().unwrap().len(), 1, "{arm}");
                        assert_eq!(after["knowledge_atoms"], before["knowledge_atoms"], "{arm}");
                        assert_eq!(
                            after["knowledge_domains"], before["knowledge_domains"],
                            "{arm}"
                        );
                    }
                }
            }
        }
    }
}
