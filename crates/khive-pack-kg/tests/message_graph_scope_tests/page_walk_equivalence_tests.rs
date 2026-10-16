#[derive(Clone, Copy)]
enum Run {
    Readable(usize),
    Withheld(usize),
}

/// Ids are seeded by position, so two stores written from the same runs hold
/// the readable records under the same ids and their pages compare as values.
/// Edge ids rise with creation order, which keeps every sort tie in that order.
fn seeded(kind: u128, position: usize) -> Uuid {
    Uuid::from_u128((kind << 64) | position as u128)
}

/// Writes `runs` in order. A readable edge joins two concepts. A withheld edge
/// runs from one of `messages` to a concept, and is skipped when there are no
/// messages: that is the store that never held it. Returns the ids of the
/// readable edges and of the withheld ones, which a store without them still
/// names.
async fn write_runs(
    main: &KhiveRuntime,
    messages: &[Uuid],
    runs: &[Run],
) -> (Vec<Uuid>, Vec<Uuid>) {
    let mut readable = Vec::new();
    let mut withheld = Vec::new();
    let mut position = 0;
    for run in runs {
        let (count, hidden) = match *run {
            Run::Readable(count) => (count, false),
            Run::Withheld(count) => (count, true),
        };
        for _ in 0..count {
            position += 1;
            let edge = seeded(0xed, position);
            let target = seeded(0xc0, position);
            if hidden {
                withheld.push(edge);
                let Some(&message) = messages.get(position % 2) else {
                    continue;
                };
                store_seeded_concept(main, target).await;
                store_edge(
                    main,
                    "local",
                    edge,
                    message,
                    target,
                    EdgeRelation::Annotates,
                    1.0,
                )
                .await;
            } else {
                let source = seeded(0x5c, position);
                store_seeded_concept(main, source).await;
                store_seeded_concept(main, target).await;
                store_edge(
                    main,
                    "local",
                    edge,
                    source,
                    target,
                    EdgeRelation::Extends,
                    1.0,
                )
                .await;
                readable.push(edge);
            }
        }
    }
    (readable, withheld)
}

/// A page as a value, without the stamps the store assigns when it writes.
fn page_without_stamps(page: &Value) -> Value {
    let mut page = page.clone();
    for key in ["items", "edges"] {
        if let Some(edges) = page.get_mut(key).and_then(Value::as_array_mut) {
            for edge in edges {
                for stamp in ["created_at", "updated_at"] {
                    edge.as_object_mut().expect("edge object").remove(stamp);
                }
            }
        }
    }
    page
}

fn page_keys(page: &Value) -> Vec<&String> {
    page.as_object().expect("page object").keys().collect()
}

/// Reads every page of the observer's edge list, advancing the offset by
/// `limit` or following `next_after`.
async fn walk_pages(registry: &VerbRegistry, limit: u32, cursor: bool) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut after = String::new();
    let mut offset = 0;
    loop {
        let args = if cursor {
            json!({"kind": "edge", "limit": limit, "after": after})
        } else {
            json!({"kind": "edge", "limit": limit, "offset": offset})
        };
        let page = read(registry, "observer", "list", args).await;
        pages.push(page_without_stamps(&page));
        assert!(pages.len() <= 64, "the walk does not end: {page}");
        if cursor {
            let Some(next) = page["next_after"].as_str() else {
                return pages;
            };
            after = next.to_string();
        } else {
            if page["has_more"] != json!(true) {
                return pages;
            }
            offset += limit;
        }
    }
}

/// The observer cannot read the messages that the withheld edges touch. Every
/// answer it gets from the edge list is the answer of a store built from the
/// same readable records in which those messages and edges were never written.
async fn assert_walks_match_the_store_without_them(topology: Topology, runs: &[Run]) {
    let f = fixture(topology).await;
    let (readable, withheld) = write_runs(&f.main, &[f.outbound.id, f.inbound.id], runs).await;
    let (twin_main, _, twin_registry) = scoped_registry(topology);
    let (twin_readable, twin_withheld) = write_runs(&twin_main, &[], runs).await;
    assert_eq!(readable, twin_readable);
    assert_eq!(withheld, twin_withheld);

    for cursor in [false, true] {
        for limit in [1, 2, 3] {
            let mode = if cursor { "cursor" } else { "offset" };
            let seen = walk_pages(&f.registry, limit, cursor).await;
            let expected = walk_pages(&twin_registry, limit, cursor).await;
            assert_eq!(
                seen.len(),
                expected.len(),
                "{mode} limit {limit}: pages in the walk"
            );
            for (index, (page, twin)) in seen.iter().zip(&expected).enumerate() {
                let context = format!("{mode} limit {limit} page {index}");
                assert_eq!(page_keys(page), page_keys(twin), "{context}: keys");
                assert_eq!(page, twin, "{context}");
            }
        }
    }

    // At limit 0 the offset form returns no rows and a cursor read is refused.
    let args = json!({"kind": "edge", "limit": 0});
    let seen = read(&f.registry, "observer", "list", args.clone()).await;
    let twin = read(&twin_registry, "observer", "list", args.clone()).await;
    assert_eq!(
        page_without_stamps(&seen),
        page_without_stamps(&twin),
        "{args}"
    );
    let args = json!({"kind": "edge", "limit": 0, "after": ""});
    let seen = try_read(&f.registry, "observer", "list", args.clone())
        .await
        .expect_err("a cursor read at limit 0 is refused");
    let twin = try_read(&twin_registry, "observer", "list", args)
        .await
        .expect_err("a cursor read at limit 0 is refused");
    assert_eq!(seen.to_string(), twin.to_string());
    assert_eq!(std::mem::discriminant(&seen), std::mem::discriminant(&twin));

    let args = json!({"kind": "edge", "limit": 1, "after": withheld[0].to_string()});
    let seen = try_read(&f.registry, "observer", "list", args.clone())
        .await
        .expect_err("a cursor naming an unreadable edge is refused");
    let twin = try_read(&twin_registry, "observer", "list", args)
        .await
        .expect_err("a cursor naming no edge is refused");
    assert_eq!(seen.to_string(), twin.to_string());
    assert_eq!(std::mem::discriminant(&seen), std::mem::discriminant(&twin));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_withheld_run_between_readable_edges() {
    for topology in [Topology::Shared, Topology::Split] {
        let runs = [Run::Readable(1), Run::Withheld(250), Run::Readable(2)];
        assert_walks_match_the_store_without_them(topology, &runs).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_withheld_run_before_last_readable_edge() {
    for topology in [Topology::Shared, Topology::Split] {
        let runs = [Run::Withheld(250), Run::Readable(1)];
        assert_walks_match_the_store_without_them(topology, &runs).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_withheld_run_after_last_readable_edge() {
    for topology in [Topology::Shared, Topology::Split] {
        let runs = [Run::Readable(1), Run::Withheld(250)];
        assert_walks_match_the_store_without_them(topology, &runs).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_only_withheld_edges() {
    for topology in [Topology::Shared, Topology::Split] {
        assert_walks_match_the_store_without_them(topology, &[Run::Withheld(250)]).await;
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_walk_equals_twin_with_interleaved_withheld_edges() {
    let mut runs = vec![Run::Readable(1)];
    for size in [1, 2, 4, 8, 16, 32, 64, 128] {
        runs.extend([Run::Withheld(size), Run::Readable(1)]);
    }
    for topology in [Topology::Shared, Topology::Split] {
        assert_walks_match_the_store_without_them(topology, &runs).await;
    }
}
