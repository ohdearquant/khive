use khive_graph_diff::{diff, DiffInputError, GraphState, Id128, Substrate};
use serde_json::{json, Value};

const A: &str = "00000000-0000-0000-0000-000000000001";
const B: &str = "00000000-0000-0000-0000-000000000002";
const C: &str = "00000000-0000-0000-0000-000000000003";

fn lines(records: &[Value]) -> String {
    records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn state(entities: &[Value], edges: &[Value], notes: &[Value]) -> GraphState {
    GraphState::from_ndjson(&lines(entities), &lines(edges), &lines(notes)).unwrap()
}

#[test]
fn all_substrates_report_complete_additions_removals_and_field_changes() {
    let before = state(
        &[
            json!({"id": B, "name": "gone", "properties": {"x": 1}}),
            json!({"id": A, "name": "old"}),
        ],
        &[
            json!({"edge_id": B, "source_id": A, "target_id": C, "relation": "supports"}),
            json!({"edge_id": A, "weight": 0.5}),
        ],
        &[
            json!({"id": B, "content": "gone", "kind": "observation"}),
            json!({"id": A, "content": "old"}),
        ],
    );
    let after = state(
        &[
            json!({"id": C, "kind": "concept", "name": "added"}),
            json!({"id": A, "name": "new"}),
        ],
        &[
            json!({"edge_id": C, "source_id": B, "target_id": A, "relation": "derived_from"}),
            json!({"edge_id": A, "weight": 0.75}),
        ],
        &[
            json!({"id": C, "content": "added", "kind": "observation"}),
            json!({"id": A, "content": "new"}),
        ],
    );
    assert_eq!(
        serde_json::to_value(diff(&before, &after)).unwrap(),
        json!({
            "entities": {
                "added": [{"id": C, "fields": {"kind": "concept", "name": "added"}}],
                "removed": [{"id": B, "fields": {"name": "gone", "properties": {"x": 1}}}],
                "modified": [{"id": A, "changes": [{"field": "name", "before": "old", "after": "new"}]}]
            },
            "edges": {
                "added": [{"id": C, "fields": {"source_id": B, "target_id": A, "relation": "derived_from"}}],
                "removed": [{"id": B, "fields": {"source_id": A, "target_id": C, "relation": "supports"}}],
                "modified": [{"id": A, "changes": [{"field": "weight", "before": 0.5, "after": 0.75}]}]
            },
            "notes": {
                "added": [{"id": C, "fields": {"content": "added", "kind": "observation"}}],
                "removed": [{"id": B, "fields": {"content": "gone", "kind": "observation"}}],
                "modified": [{"id": A, "changes": [{"field": "content", "before": "old", "after": "new"}]}]
            }
        })
    );
}

#[test]
fn missing_and_null_have_distinct_serialized_changes_in_both_directions() {
    let absent = state(&[json!({"id": A})], &[], &[]);
    let null = state(&[json!({"id": A, "properties": null})], &[], &[]);
    let added = diff(&absent, &null);
    let removed = diff(&null, &absent);
    assert_eq!(added.entities.modified[0].changes[0].before, None);
    assert_eq!(
        added.entities.modified[0].changes[0].after,
        Some(Value::Null)
    );
    assert_eq!(
        serde_json::to_string(&added.entities.modified[0].changes).unwrap(),
        r#"[{"field":"properties","after":null}]"#
    );
    assert_eq!(
        serde_json::to_string(&removed.entities.modified[0].changes).unwrap(),
        r#"[{"field":"properties","before":null}]"#
    );
}

#[test]
fn nested_values_arrays_and_json_types_change_without_path_flattening() {
    let before = state(
        &[json!({"id": A, "nested": {"a.b": {"value": null}}, "array": [1, 2], "number": 1})],
        &[],
        &[],
    );
    let after = state(
        &[json!({"id": A, "nested": {"a.b": {}}, "array": [2, 1], "number": "1"})],
        &[],
        &[],
    );
    assert_eq!(
        serde_json::to_value(&diff(&before, &after).entities.modified[0].changes).unwrap(),
        json!([
            {"field": "array", "before": [1, 2], "after": [2, 1]},
            {"field": "nested", "before": {"a.b": {"value": null}}, "after": {"a.b": {}}},
            {"field": "number", "before": 1, "after": "1"}
        ])
    );
}

#[test]
fn line_and_recursive_object_key_permutations_have_identical_diff_bytes() {
    let first = format!(
        r#"{{"id":"{B}","z":{{"b":2,"a":[{{"z":3,"a":1}}]}}}}
{{"id":"{A}","name":"first"}}"#
    );
    let second = format!(
        r#"{{"name":"first","id":"{A}"}}
{{"z":{{"a":[{{"a":1,"z":3}}],"b":2}},"id":"{B}"}}"#
    );
    let a = GraphState::from_ndjson(&first, "", "").unwrap();
    let b = GraphState::from_ndjson(&second, "", "").unwrap();
    assert_eq!(a, b);
    assert!(diff(&a, &b).is_empty());
    let empty = GraphState::default();
    let bytes = serde_json::to_string(&diff(&empty, &a)).unwrap();
    assert_eq!(bytes, serde_json::to_string(&diff(&empty, &b)).unwrap());
    assert!(bytes.contains(r#""z":{"a":[{"a":1,"z":3}],"b":2}"#));
    assert_eq!(
        diff(&empty, &a)
            .entities
            .added
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        vec![Id128::from_u128(1), Id128::from_u128(2)]
    );
}

#[test]
fn removals_and_modifications_are_uuid_sorted_independently() {
    let before = state(
        &[
            json!({"id": C, "z": 0}),
            json!({"id": B}),
            json!({"id": A, "z": 0}),
        ],
        &[],
        &[],
    );
    let after = state(
        &[
            json!({"id": C, "a": 2, "z": 1}),
            json!({"id": A, "a": 2, "z": 1}),
        ],
        &[],
        &[],
    );
    let delta = diff(&before, &after);
    assert_eq!(
        delta
            .entities
            .modified
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        vec![Id128::from_u128(1), Id128::from_u128(3)]
    );
    assert_eq!(delta.entities.removed[0].id, Id128::from_u128(2));
    assert_eq!(
        delta.entities.modified[0]
            .changes
            .iter()
            .map(|c| c.field.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "z"]
    );
    assert_eq!(
        diff(&before, &GraphState::default())
            .entities
            .removed
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        vec![
            Id128::from_u128(1),
            Id128::from_u128(2),
            Id128::from_u128(3)
        ]
    );
}

#[test]
fn identity_aliases_normalize_and_never_survive_in_record_fields() {
    let canonical = "abcdef00-1234-5678-9abc-def012345678";
    let alias = "ABCDEF00123456789ABCDEF012345678";
    let first = state(&[json!({"id": canonical, "name": "same"})], &[], &[]);
    let second = state(&[json!({"id": alias, "name": "same"})], &[], &[]);
    assert!(diff(&first, &second).is_empty());
    assert_eq!(
        serde_json::to_value(&diff(&GraphState::default(), &second).entities.added).unwrap(),
        json!([{"id": canonical, "fields": {"name": "same"}}])
    );
    let duplicate = lines(&[json!({"id": canonical}), json!({"id": alias})]);
    assert!(
        matches!(GraphState::from_ndjson(&duplicate, "", ""), Err(DiffInputError::DuplicateIdentity { substrate: Substrate::Entity, line: 2, id }) if id.to_string() == canonical)
    );
}

#[test]
fn equal_ids_across_substrates_remain_separate() {
    let graph = state(
        &[json!({"id": A, "kind": "future_entity"})],
        &[json!({"edge_id": A, "relation": "future_relation"})],
        &[json!({"id": A, "kind": "future_note"})],
    );
    let delta = diff(&GraphState::default(), &graph);
    assert_eq!(delta.entities.added.len(), 1);
    assert_eq!(delta.edges.added.len(), 1);
    assert_eq!(delta.notes.added.len(), 1);
    assert_eq!(delta.edges.added[0].fields["relation"], "future_relation");
    assert!(!delta.edges.added[0].fields.contains_key("edge_id"));
}

#[test]
fn vcs_ndjson_shapes_and_note_fields_are_preserved_without_defaults() {
    // Entity/edge fixtures follow khive-vcs/src/sync.rs NdjsonEntity (37-54),
    // NdjsonEdge (57-71), and their serializers write_sorted_* (953-991).
    // VCS does not export notes; that third stream uses the note id/kind/content
    // contract and remains an independently supplied collection.
    let graph = state(
        &[
            json!({"id": A, "kind": "concept", "name": "a", "description": null, "properties": {}, "tags": [], "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-02-02T00:00:00Z"}),
        ],
        &[
            json!({"edge_id": B, "source": A, "target": C, "relation": "supports", "weight": 0.75, "properties": null, "created_at": "2026-03-03T00:00:00Z", "updated_at": "2026-04-04T00:00:00Z"}),
        ],
        &[
            json!({"id": C, "namespace": "local", "kind": "observation", "content": "text", "properties": null, "created_at": 123}),
        ],
    );
    let delta = diff(&GraphState::default(), &graph);
    assert_eq!(delta.entities.added[0].fields.len(), 7);
    assert_eq!(delta.edges.added[0].fields.len(), 7);
    assert_eq!(delta.notes.added[0].fields.len(), 5);
    assert_eq!(
        delta.entities.added[0].fields["updated_at"],
        "2026-02-02T00:00:00Z"
    );
    assert_eq!(delta.edges.added[0].fields["source"], A);
    assert_eq!(
        delta.edges.added[0].fields["created_at"],
        "2026-03-03T00:00:00Z"
    );
    assert_eq!(delta.notes.added[0].fields["content"], "text");
    let minimal = diff(
        &GraphState::default(),
        &state(&[json!({"id": A})], &[], &[]),
    );
    assert!(minimal.entities.added[0].fields.is_empty());
}

#[test]
fn valid_empty_streams_and_whitespace_are_empty_states() {
    let empty = GraphState::from_ndjson(" \n\r\n\t", "", "\n").unwrap();
    assert_eq!(empty, GraphState::default());
    assert!(diff(&empty, &empty).is_empty());
}

#[test]
fn malformed_json_retains_source_and_physical_line_in_each_substrate() {
    for substrate in [Substrate::Entity, Substrate::Edge, Substrate::Note] {
        let valid = json!({substrate.identity_field(): A}).to_string();
        let input = format!("\n{valid}\n{{bad\n{valid}");
        let error = match substrate {
            Substrate::Entity => GraphState::from_ndjson(&input, "", ""),
            Substrate::Edge => GraphState::from_ndjson("", &input, ""),
            Substrate::Note => GraphState::from_ndjson("", "", &input),
        }
        .unwrap_err();
        assert!(std::error::Error::source(&error).is_some());
        assert!(
            matches!(error, DiffInputError::InvalidJson { substrate: actual, line: 3, .. } if actual == substrate)
        );
    }
}

#[test]
fn object_and_identity_refusals_are_distinct() {
    for input in ["null", "[]", "1", "\"text\""] {
        assert!(matches!(
            GraphState::from_ndjson(input, "", ""),
            Err(DiffInputError::NotObject {
                substrate: Substrate::Entity,
                line: 1
            })
        ));
    }
    assert!(matches!(
        GraphState::from_ndjson("{}", "", ""),
        Err(DiffInputError::MissingIdentity { field: "id", .. })
    ));
    assert!(matches!(
        GraphState::from_ndjson("", &json!({"id": A}).to_string(), ""),
        Err(DiffInputError::MissingIdentity {
            substrate: Substrate::Edge,
            field: "edge_id",
            ..
        })
    ));
    for identity in [Value::Null, json!(1), json!([]), json!({}), json!(true)] {
        assert!(matches!(
            GraphState::from_ndjson("", "", &json!({"id": identity}).to_string()),
            Err(DiffInputError::NonStringIdentity {
                substrate: Substrate::Note,
                field: "id",
                ..
            })
        ));
    }
    for identity in ["", "not-a-uuid", "00000000-0000-0000-0000-00000000000x"] {
        let error =
            GraphState::from_ndjson(&json!({"id": identity}).to_string(), "", "").unwrap_err();
        assert!(std::error::Error::source(&error).is_some());
        assert!(matches!(
            error,
            DiffInputError::InvalidIdentity { line: 1, .. }
        ));
    }
}

#[test]
fn identical_duplicates_refuse_in_all_three_collections() {
    for substrate in [Substrate::Entity, Substrate::Edge, Substrate::Note] {
        let record = json!({substrate.identity_field(): A});
        let input = lines(&[record.clone(), record]);
        let result = match substrate {
            Substrate::Entity => GraphState::from_ndjson(&input, "", ""),
            Substrate::Edge => GraphState::from_ndjson("", &input, ""),
            Substrate::Note => GraphState::from_ndjson("", "", &input),
        };
        assert!(
            matches!(result, Err(DiffInputError::DuplicateIdentity { substrate: actual, line: 2, id }) if actual == substrate && id == Id128::from_u128(1))
        );
    }
}

#[test]
fn diff_is_repeatable_and_does_not_mutate_its_inputs() {
    let before = state(&[json!({"id": A, "name": "before"})], &[], &[]);
    let after = state(&[json!({"id": A, "name": "after"})], &[], &[]);
    let saved = (before.clone(), after.clone());
    let first = serde_json::to_vec(&diff(&before, &after)).unwrap();
    assert_eq!(first, serde_json::to_vec(&diff(&before, &after)).unwrap());
    assert_eq!((before, after), saved);
}

#[test]
fn parity_golden() {
    let before = state(
        &[json!({"id": A, "gone": null})],
        &[],
        &[json!({"id": C, "content": "old"})],
    );
    let after = state(
        &[json!({"id": A, "added": null})],
        &[json!({"edge_id": B, "properties": {"z": [1, {"b": 2, "a": 1}], "a": true}})],
        &[],
    );
    let forward = serde_json::to_string(&diff(&before, &after)).unwrap();
    assert_eq!(
        forward,
        r#"{"entities":{"added":[],"removed":[],"modified":[{"id":"00000000-0000-0000-0000-000000000001","changes":[{"field":"added","after":null},{"field":"gone","before":null}]}]},"edges":{"added":[{"id":"00000000-0000-0000-0000-000000000002","fields":{"properties":{"a":true,"z":[1,{"a":1,"b":2}]}}}],"removed":[],"modified":[]},"notes":{"added":[],"removed":[{"id":"00000000-0000-0000-0000-000000000003","fields":{"content":"old"}}],"modified":[]}}"#
    );
    let empty = serde_json::to_string(&diff(&before, &before)).unwrap();
    assert_eq!(
        empty,
        r#"{"entities":{"added":[],"removed":[],"modified":[]},"edges":{"added":[],"removed":[],"modified":[]},"notes":{"added":[],"removed":[],"modified":[]}}"#
    );
    // The leading newline separates data from libtest's partial status line.
    // CI compares these actual public-API result bytes, not the golden literals.
    println!("\nGRAPH_DIFF_PARITY:forward:{forward}");
    println!("GRAPH_DIFF_PARITY:empty:{empty}");
}
