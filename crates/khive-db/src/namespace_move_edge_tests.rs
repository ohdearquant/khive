use super::tests::{migrated, route, seed_note};
use super::*;

const RELATIONS: [&str; 5] = [
    "depends_on",
    "introduced_by",
    "contains",
    "derived_from",
    "supports",
];

fn id(index: usize) -> String {
    format!("eeeeeeee-eeee-4eee-8eee-{index:012}")
}

fn edge(conn: &Connection, index: usize, namespace: &str, relation: &str) {
    conn.execute(
        "INSERT INTO graph_edges \
         (id, namespace, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, metadata) \
         VALUES (?1, ?2, ?3, ?4, ?5, 0.75, 7, 8, ?6, '{\"kept\":true}')",
        rusqlite::params![
            id(index),
            namespace,
            format!("aaaaaaaa-aaaa-4aaa-8aaa-{index:012}"),
            format!("bbbbbbbb-bbbb-4bbb-8bbb-{index:012}"),
            relation,
            (index == 4).then_some(9_i64),
        ],
    )
    .unwrap();
}

fn prepared() -> Connection {
    let conn = migrated();
    for (index, relation) in RELATIONS.iter().enumerate() {
        edge(&conn, index, "source", relation);
    }
    edge(&conn, 100, "target-a", "depends_on");
    edge(&conn, 101, "foreign", "introduced_by");
    conn
}

fn places(conn: &Connection) -> BTreeMap<String, String> {
    conn.prepare("SELECT id, namespace FROM graph_edges ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn content(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT json_array(id, source_id, target_id, relation, weight, created_at, \
         updated_at, deleted_at, metadata, target_backend) FROM graph_edges ORDER BY id",
    )
    .unwrap()
    .query_map([], |row| row.get(0))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn changes(conn: &Connection) -> i64 {
    conn.query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap()
}

fn split_routes(fallback_first: bool) -> MoveRequest {
    let mut routes = vec![
        route("edge:depends_on", "target-a"),
        route("edge:introduced_by", "target-b"),
        route("edge:refutes", "unused-target"),
    ];
    if fallback_first {
        routes.insert(0, route("edge", "target-c"));
    } else {
        routes.push(route("edge", "target-c"));
    }
    MoveRequest::new("source", routes)
}

#[test]
fn relation_route_keys_round_trip_the_closed_canonical_set() {
    for relation in khive_types::EdgeRelation::VALID_NAMES {
        let key = format!("edge:{relation}");
        assert_eq!(SubjectClass::parse(&key).unwrap().render(), key);
    }
    assert_eq!(SubjectClass::parse("edge").unwrap(), SubjectClass::Edge);
    for key in ["edge:", "edge:unknown", "edge:Depends-On", "edge:dependson"] {
        assert!(matches!(SubjectClass::parse(key),
            Err(MoveError::UnknownSubjectClass { key: actual }) if actual == key));
    }
}

#[test]
fn relation_routes_override_fallback_in_either_order_with_exact_counts() {
    for fallback_first in [true, false] {
        let mut conn = prepared();
        let before = content(&conn);
        let request = split_routes(fallback_first);
        let tx = conn.transaction().unwrap();
        let counts = move_namespace(&tx, &request).unwrap();
        assert_eq!(
            counts.subjects,
            BTreeMap::from([
                ("edge:depends_on".into(), 1),
                ("edge:introduced_by".into(), 1),
                ("edge:refutes".into(), 0),
                ("edge".into(), 3),
            ])
        );
        assert_eq!(counts.rows.get("graph_edges"), Some(&5));
        assert_eq!(
            places(&tx),
            BTreeMap::from([
                (id(0), "target-a".into()),
                (id(1), "target-b".into()),
                (id(2), "target-c".into()),
                (id(3), "target-c".into()),
                (id(4), "target-c".into()),
                (id(100), "target-a".into()),
                (id(101), "foreign".into()),
            ])
        );
        assert_eq!(content(&tx), before);
        let rerun = move_namespace(&tx, &request).unwrap();
        assert_eq!(rerun.subjects.len(), 4);
        assert!(rerun.subjects.values().all(|count| *count == 0));
        assert_eq!(rerun.rows.get("graph_edges"), Some(&0));
        tx.commit().unwrap();
    }
}

#[test]
fn every_present_relation_can_move_without_a_bare_edge_route() {
    let mut conn = prepared();
    let routes = RELATIONS
        .iter()
        .enumerate()
        .map(|(index, relation)| {
            route(
                &format!("edge:{relation}"),
                if index % 2 == 0 { "one" } else { "two" },
            )
        })
        .collect();
    let request = MoveRequest::new("source", routes);
    let tx = conn.transaction().unwrap();
    let counts = move_namespace(&tx, &request).unwrap();
    assert_eq!(counts.subjects.len(), 5);
    assert!(!counts.subjects.contains_key("edge"));
    let after = places(&tx);
    for (index, relation) in RELATIONS.iter().enumerate() {
        assert_eq!(counts.subjects[&format!("edge:{relation}")], 1);
        assert_eq!(
            after[&id(index)],
            if index % 2 == 0 { "one" } else { "two" }
        );
    }
    let repeated = move_namespace(&tx, &request).unwrap();
    assert_eq!(repeated.subjects.len(), 5);
    assert!(repeated.subjects.values().all(|count| *count == 0));
    tx.commit().unwrap();
}

#[test]
fn an_unrouted_relation_refuses_before_any_write_including_deleted_edges() {
    let conn = prepared();
    let request = MoveRequest::new(
        "source",
        RELATIONS[..4]
            .iter()
            .map(|relation| route(&format!("edge:{relation}"), "target"))
            .collect(),
    );
    let before = (places(&conn), content(&conn), changes(&conn));
    let error = move_namespace(&conn, &request).unwrap_err();
    assert!(matches!(error, MoveError::UnroutedClass { class, rows: 1 }
        if class == "edge:supports"));
    assert_eq!((places(&conn), content(&conn), changes(&conn)), before);
}

#[test]
fn direct_invalid_relation_routes_cannot_bypass_preflight() {
    for relation in ["", "unknown", "Depends-On"] {
        let conn = prepared();
        let request = MoveRequest::new(
            "source",
            vec![
                MoveRoute {
                    class: SubjectClass::EdgeRelation(relation.into()),
                    target: "target".into(),
                },
                route("edge", "fallback"),
            ],
        );
        let before = (places(&conn), content(&conn), changes(&conn));
        let error = move_namespace(&conn, &request).unwrap_err();
        assert!(matches!(error, MoveError::UnknownSubjectClass { key }
            if key == format!("edge:{relation}")));
        assert_eq!((places(&conn), content(&conn), changes(&conn)), before);
    }
}

#[test]
fn duplicate_relation_routes_and_same_namespace_targets_refuse() {
    for second in ["target-a", "target-b"] {
        let conn = prepared();
        let before = (places(&conn), changes(&conn));
        let request = MoveRequest::new(
            "source",
            vec![
                route("edge:depends_on", "target-a"),
                route("edge:depends_on", second),
                route("edge", "fallback"),
            ],
        );
        assert!(matches!(move_namespace(&conn, &request),
            Err(MoveError::DuplicateRoute { class }) if class == "edge:depends_on"));
        assert_eq!((places(&conn), changes(&conn)), before);
    }
    let conn = prepared();
    let before = changes(&conn);
    let request = MoveRequest::new("source", vec![route("edge:depends_on", "source")]);
    assert!(matches!(move_namespace(&conn, &request),
        Err(MoveError::TargetIsSource { class }) if class == "edge:depends_on"));
    assert_eq!(changes(&conn), before);
}

#[test]
fn bare_edge_route_preserves_legacy_rows_and_counts() {
    let mut conn = prepared();
    edge(&conn, 5, "source", "legacy_relation");
    edge(&conn, 102, "target", "supports");
    let before = content(&conn);
    let request = MoveRequest::new("source", vec![route("edge", "target")]);
    let tx = conn.transaction().unwrap();
    let counts = move_namespace(&tx, &request).unwrap();
    assert_eq!(counts.subjects, BTreeMap::from([("edge".into(), 6)]));
    assert_eq!(counts.rows.get("graph_edges"), Some(&6));
    assert!(counts
        .rows
        .iter()
        .all(|(table, count)| *count == if table == "graph_edges" { 6 } else { 0 }));
    assert!(counts.left_behind.is_empty());
    assert_eq!(counts.ann_log_appended, 0);
    let after = places(&tx);
    for index in 0..6 {
        assert_eq!(after[&id(index)], "target");
    }
    assert_eq!(after[&id(100)], "target-a");
    assert_eq!(after[&id(101)], "foreign");
    assert_eq!(after[&id(102)], "target");
    assert_eq!(content(&tx), before);
    assert_eq!(move_namespace(&tx, &request).unwrap().subjects["edge"], 0);
    tx.commit().unwrap();
}

fn aggregates_in(conn: &Connection, namespace: &str) -> [i64; 3] {
    [
        "brain_profile_snapshots",
        "brain_event_log",
        "proposals_open",
    ]
    .map(|table| {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE namespace = ?1"),
            [namespace],
            |row| row.get(0),
        )
        .unwrap()
    })
}

#[test]
fn exhaustive_relation_routes_move_aggregates_only_to_a_single_target() {
    for with_fallback in [false, true] {
        for split in [false, true] {
            let conn = prepared();
            seed_note(&conn, "n1", "source", "observation");
            conn.execute_batch(
                "INSERT INTO brain_profile_snapshots \
                 (profile_id, namespace, snapshot_json, updated_at) VALUES ('p', 'source', '{}', 1); \
                 INSERT INTO brain_event_log \
                 (profile_id, namespace, event_kind, payload, created_at) \
                 VALUES ('p', 'source', 'fold', '{}', 1); \
                 INSERT INTO proposals_open \
                 (proposal_id, namespace, proposer, title, status, created_at, updated_at) \
                 VALUES ('q', 'source', 'someone', 'a title', 'open', 1, 1);",
            )
            .unwrap();
            let mut routes: Vec<MoveRoute> = RELATIONS
                .iter()
                .map(|relation| {
                    let target = if split && *relation == "supports" {
                        "target-b"
                    } else {
                        "target"
                    };
                    route(&format!("edge:{relation}"), target)
                })
                .collect();
            routes.push(route("note:observation", "target"));
            if with_fallback {
                routes.push(route("edge", "target"));
            }
            let request = MoveRequest::new("source", routes);
            let counts = move_namespace(&conn, &request).unwrap();
            assert_eq!(counts.rows.get("graph_edges"), Some(&5));
            assert_eq!(counts.subjects.get("note:observation"), Some(&1));
            if split {
                for table in [
                    "brain_profile_snapshots",
                    "brain_event_log",
                    "proposals_open",
                ] {
                    assert_eq!(counts.left_behind.get(table), Some(&1), "{table}");
                }
                assert_eq!(aggregates_in(&conn, "source"), [1, 1, 1]);
                assert_eq!(aggregates_in(&conn, "target"), [0, 0, 0]);
            } else {
                assert!(counts.left_behind.is_empty(), "{:?}", counts.left_behind);
                assert_eq!(aggregates_in(&conn, "source"), [0, 0, 0]);
                assert_eq!(aggregates_in(&conn, "target"), [1, 1, 1]);
            }
        }
    }
}

#[cfg(feature = "vectors")]
#[test]
fn edge_vectors_follow_the_resolved_relation_not_both_matching_routes() {
    crate::extension::ensure_extensions_loaded();
    for fallback_first in [true, false] {
        let mut conn = prepared();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE vec_edge_route USING vec0(\
             subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, \
             field TEXT NOT NULL, embedding_model TEXT NOT NULL, \
             embedding float[2] distance_metric=cosine)",
        )
        .unwrap();
        for index in [0, 1, 2, 3, 4, 100, 101] {
            let namespace = match index {
                100 => "target-a",
                101 => "foreign",
                _ => "source",
            };
            conn.execute(
                "INSERT INTO vec_edge_route \
                 (subject_id, namespace, kind, field, embedding_model, embedding) \
                 VALUES (?1, ?2, 'entity', 'knowledge.atom', 'edge_route', '[0.1,0.2]')",
                rusqlite::params![id(index), namespace],
            )
            .unwrap();
        }
        let read_vectors = |conn: &Connection| -> BTreeMap<String, (String, Vec<u8>)> {
            conn.prepare("SELECT subject_id, namespace, embedding FROM vec_edge_route")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let before = read_vectors(&conn);
        let request = split_routes(fallback_first);
        let tx = conn.transaction().unwrap();
        let counts = move_namespace(&tx, &request).unwrap();
        assert_eq!(counts.rows.get("vec_edge_route"), Some(&5));
        assert_eq!(counts.ann_log_appended, 10);
        assert!(!counts.left_behind.contains_key("vec_edge_route"));
        let after = read_vectors(&tx);
        assert_eq!(after.len(), before.len());
        for (index, target) in [
            (0, "target-a"),
            (1, "target-b"),
            (2, "target-c"),
            (3, "target-c"),
            (4, "target-c"),
            (100, "target-a"),
            (101, "foreign"),
        ] {
            let subject = id(index);
            assert_eq!(after[&subject].0, target);
            assert_eq!(after[&subject].1, before[&subject].1);
            let log: Vec<(i64, String, String)> = tx
                .prepare(
                    "SELECT seq, namespace, op FROM ann_write_log \
                     WHERE subject_id=?1 ORDER BY seq",
                )
                .unwrap()
                .query_map([&subject], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            if index < 5 {
                assert_eq!(log.len(), 2);
                assert_eq!((log[0].1.as_str(), log[0].2.as_str()), ("source", "delete"));
                assert_eq!((log[1].1.as_str(), log[1].2.as_str()), (target, "upsert"));
                assert!(log[0].0 < log[1].0);
            } else {
                assert!(log.is_empty());
            }
        }
        let repeated = move_namespace(&tx, &request).unwrap();
        assert_eq!(repeated.rows.get("vec_edge_route"), Some(&0));
        assert_eq!(repeated.ann_log_appended, 0);
        assert_eq!(read_vectors(&tx), after);
        tx.commit().unwrap();
    }
}

#[test]
fn relation_moves_through_intermediate_namespaces_preserve_residents() {
    let mut conn = migrated();
    edge(&conn, 0, "source", "depends_on");
    edge(&conn, 1, "source", "supports");
    // A resident of target-b repeats edge 0's endpoints and relation, so the unique
    // triple matches it although depends_on is routed to target-a.
    conn.execute(
        "INSERT INTO graph_edges \
         (id, namespace, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, metadata) \
         SELECT ?1, 'target-b', source_id, target_id, relation, weight, created_at, updated_at, \
         deleted_at, metadata FROM graph_edges WHERE id = ?2",
        rusqlite::params![id(200), id(0)],
    )
    .unwrap();
    let before = content(&conn);
    let staged = MoveRequest::new(
        "source",
        vec![
            route("edge:depends_on", "stage-a"),
            route("edge:supports", "stage-b"),
        ],
    );
    let tx = conn.transaction().unwrap();
    assert_eq!(
        move_namespace(&tx, &staged)
            .unwrap()
            .rows
            .get("graph_edges"),
        Some(&2)
    );
    tx.commit().unwrap();
    for (stage, target) in [("stage-a", "target-a"), ("stage-b", "target-b")] {
        let tx = conn.transaction().unwrap();
        let counts =
            move_namespace(&tx, &MoveRequest::new(stage, vec![route("edge", target)])).unwrap();
        assert_eq!(counts.subjects, BTreeMap::from([("edge".into(), 1)]));
        tx.commit().unwrap();
    }
    assert_eq!(
        places(&conn),
        BTreeMap::from([
            (id(0), "target-a".into()),
            (id(1), "target-b".into()),
            (id(200), "target-b".into()),
        ])
    );
    assert_eq!(content(&conn), before);
}

fn resident_copy(conn: &Connection, resident: usize, source: usize, namespace: &str) {
    conn.execute(
        "INSERT INTO graph_edges \
         (id, namespace, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, metadata) \
         SELECT ?1, ?2, source_id, target_id, relation, weight, created_at, updated_at, \
         deleted_at, metadata FROM graph_edges WHERE id = ?3",
        rusqlite::params![id(resident), namespace, id(source)],
    )
    .unwrap();
}

#[test]
fn edge_collision_preflight_checks_only_resolved_destinations() {
    for fallback in [false, true] {
        for fallback_first in [false, true] {
            for deleted in [false, true] {
                let mut conn = migrated();
                let second_relation = if fallback {
                    "legacy'relation"
                } else {
                    "supports"
                };
                edge(&conn, 0, "source", "depends_on");
                edge(&conn, 1, "source", second_relation);
                if deleted {
                    conn.execute(
                        "UPDATE graph_edges SET deleted_at = 9 WHERE id = ?1",
                        [id(0)],
                    )
                    .unwrap();
                }
                resident_copy(&conn, 200, 0, "target-b");
                resident_copy(&conn, 201, 0, "unused-target");
                resident_copy(&conn, 202, 0, "foreign");
                let before = content(&conn);
                let second_key = if fallback { "edge" } else { "edge:supports" };
                let mut routes = vec![
                    route("edge:depends_on", "target'a"),
                    route("edge:refutes", "unused-target"),
                ];
                if fallback_first {
                    routes.insert(0, route(second_key, "target-b"));
                } else {
                    routes.push(route(second_key, "target-b"));
                }
                let request = MoveRequest::new("source", routes);
                let tx = conn.transaction().unwrap();
                let counts = move_namespace(&tx, &request).expect(
                    "route-aware edge collision must allow an unrelated destination resident",
                );
                assert_eq!(
                    counts.subjects,
                    BTreeMap::from([
                        ("edge:depends_on".into(), 1),
                        ("edge:refutes".into(), 0),
                        (second_key.into(), 1),
                    ])
                );
                assert_eq!(counts.rows.get("graph_edges"), Some(&2));
                assert_eq!(counts.ann_log_appended, 0);
                assert_eq!(
                    places(&tx),
                    BTreeMap::from([
                        (id(0), "target'a".into()),
                        (id(1), "target-b".into()),
                        (id(200), "target-b".into()),
                        (id(201), "unused-target".into()),
                        (id(202), "foreign".into()),
                    ])
                );
                assert_eq!(content(&tx), before);
                tx.commit().unwrap();
                let tx = conn.transaction().unwrap();
                let repeated = move_namespace(&tx, &request).unwrap();
                assert_eq!(
                    repeated.subjects,
                    BTreeMap::from([
                        ("edge:depends_on".into(), 0),
                        ("edge:refutes".into(), 0),
                        (second_key.into(), 0),
                    ])
                );
                assert_eq!(repeated.rows.get("graph_edges"), Some(&0));
                assert_eq!(content(&tx), before);
                assert_eq!(places(&tx)[&id(0)], "target'a");
                assert_eq!(places(&tx)[&id(1)], "target-b");
                tx.commit().unwrap();
            }
        }
    }
}

#[test]
fn edge_collision_preflight_keeps_named_refusal_before_any_write() {
    for (fallback, shared_target, colliding_index) in
        [(false, false, 0), (true, false, 1), (true, true, 0)]
    {
        for fallback_first in [false, true] {
            for deleted in [false, true] {
                let mut conn = migrated();
                let second_relation = if fallback {
                    "legacy'relation"
                } else {
                    "supports"
                };
                edge(&conn, 0, "source", "depends_on");
                edge(&conn, 1, "source", second_relation);
                if deleted {
                    conn.execute(
                        "UPDATE graph_edges SET deleted_at = 9 WHERE id = ?1",
                        [id(colliding_index)],
                    )
                    .unwrap();
                }
                seed_note(&conn, "unrelated", "source", "observation");
                let second_target = if shared_target {
                    "target'a"
                } else {
                    "target-b"
                };
                let collision_target = if colliding_index == 0 {
                    "target'a"
                } else {
                    second_target
                };
                resident_copy(&conn, 200, colliding_index, collision_target);
                let second_key = if fallback { "edge" } else { "edge:supports" };
                let mut routes = vec![route("edge:depends_on", "target'a")];
                if fallback_first {
                    routes.insert(0, route(second_key, second_target));
                } else {
                    routes.push(route(second_key, second_target));
                }
                routes.insert(0, route("note:observation", "note-target"));
                let request = MoveRequest::new("source", routes);
                let before = (places(&conn), content(&conn), changes(&conn));
                let tx = conn.transaction().unwrap();
                let error = move_namespace(&tx, &request).unwrap_err();
                let MoveError::Collisions { collisions } = error else {
                    panic!(
                        "true destination collision must retain named preflight refusal: {error:?}"
                    );
                };
                assert_eq!(collisions.len(), 1, "one collision per distinct target");
                let collision = &collisions[0];
                assert_eq!(collision.table, "graph_edges");
                assert_eq!(collision.constraint, "idx_graph_edges_unique_triple");
                assert_eq!(collision.target, collision_target);
                let relation = if colliding_index == 0 {
                    "depends_on"
                } else {
                    second_relation
                };
                assert_eq!(collision.key, format!(
                    "aaaaaaaa-aaaa-4aaa-8aaa-{colliding_index:012}, bbbbbbbb-bbbb-4bbb-8bbb-{colliding_index:012}, {relation}"
                ));
                assert_eq!((places(&tx), content(&tx), changes(&tx)), before);
                let note_namespace: String = tx
                    .query_row(
                        "SELECT namespace FROM notes WHERE id = 'unrelated'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(note_namespace, "source");
                tx.rollback().unwrap();
                assert_eq!((places(&conn), content(&conn), changes(&conn)), before);
            }
        }
    }
}
