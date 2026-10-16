// ── parse_chatgpt_export: direct unit tests ─────────────────────────────
//
// Unlike the end-to-end mirror_chatgpt_export_file tests in ingest.rs
// (which go through file I/O + SQL), these exercise the pure JSON-tree
// parsing seam directly with no runtime or DB setup.

#[test]
fn test_chatgpt_export_minimal_valid_export() {
    let export = serde_json::json!([{
        "id": "conv-min",
        "title": "Minimal Export",
        "current_node": "msg-assistant",
        "create_time": 1_700_000_000.0,
        "mapping": {
            "root": {
                "id": "root",
                "message": null,
                "parent": null,
                "children": ["msg-user"]
            },
            "msg-user": {
                "id": "msg-user",
                "parent": "root",
                "children": ["msg-assistant"],
                "message": {
                    "id": "msg-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Hello"]}
                }
            },
            "msg-assistant": {
                "id": "msg-assistant",
                "parent": "msg-user",
                "children": [],
                "message": {
                    "id": "msg-assistant",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Hi there"]}
                }
            }
        }
    }]);
    let content = serde_json::to_string(&export).unwrap();

    let events = parse_chatgpt_export(&content).expect("valid export must parse");
    assert_eq!(events.len(), 2, "root has no message, 2 nodes carry one");

    assert_eq!(events[0].uuid, "msg-user");
    assert_eq!(events[0].session_id, "conv-min");
    assert_eq!(events[0].role.as_deref(), Some("user"));
    assert_eq!(events[0].text.as_deref(), Some("Hello"));
    assert_eq!(events[0].parent_uuid, None, "root carries no message");
    assert!(!events[0].is_sidechain);
    assert_eq!(events[0].slug.as_deref(), Some("Minimal Export"));

    assert_eq!(events[1].uuid, "msg-assistant");
    assert_eq!(events[1].role.as_deref(), Some("assistant"));
    assert_eq!(events[1].text.as_deref(), Some("Hi there"));
    assert_eq!(events[1].parent_uuid.as_deref(), Some("msg-user"));
    assert!(!events[1].is_sidechain);
}

#[test]
fn test_chatgpt_export_masks_secret_bearing_title() {
    let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
    let export = serde_json::json!([{
        "id": "conv-secret-title",
        "title": format!("prod creds {secret}"),
        "current_node": "msg-user",
        "create_time": 1_700_000_000.0,
        "mapping": {
            "root": {
                "id": "root",
                "message": null,
                "parent": null,
                "children": ["msg-user"]
            },
            "msg-user": {
                "id": "msg-user",
                "parent": "root",
                "children": [],
                "message": {
                    "id": "msg-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Hello"]}
                }
            }
        }
    }]);
    let content = serde_json::to_string(&export).unwrap();

    let events = parse_chatgpt_export(&content).expect("valid export must parse");
    assert_eq!(events.len(), 1);
    let slug = events[0].slug.as_deref().expect("event slug");
    assert!(!slug.contains(&secret));
    assert!(slug.contains("***MASKED***"));
}

#[test]
fn test_chatgpt_export_multi_branch_tree_dfs_order_and_sidechain() {
    // root -> user -> {main (current path), alt (regenerated/abandoned)}
    let export = serde_json::json!([{
        "id": "conv-branch",
        "title": "Branching Conversation",
        "current_node": "msg-main",
        "mapping": {
            "root": {
                "id": "root",
                "message": null,
                "parent": null,
                "children": ["msg-user"]
            },
            "msg-user": {
                "id": "msg-user",
                "parent": "root",
                "children": ["msg-main", "msg-alt"],
                "message": {
                    "id": "msg-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Question"]}
                }
            },
            "msg-main": {
                "id": "msg-main",
                "parent": "msg-user",
                "children": [],
                "message": {
                    "id": "msg-main",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Main answer"]}
                }
            },
            "msg-alt": {
                "id": "msg-alt",
                "parent": "msg-user",
                "children": [],
                "message": {
                    "id": "msg-alt",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Alternate answer"]}
                }
            }
        }
    }]);
    let content = serde_json::to_string(&export).unwrap();

    let events = parse_chatgpt_export(&content).expect("branch export must parse");
    assert_eq!(events.len(), 3);

    // DFS preorder must follow the mapping's `children` array order, not
    // JSON object key order: user, then main, then alt.
    let uuids: Vec<&str> = events.iter().map(|e| e.uuid.as_str()).collect();
    assert_eq!(uuids, vec!["msg-user", "msg-main", "msg-alt"]);

    let by_uuid = |id: &str| events.iter().find(|e| e.uuid == id).unwrap();
    assert!(!by_uuid("msg-user").is_sidechain);
    assert!(
        !by_uuid("msg-main").is_sidechain,
        "current_node's own path must not be flagged"
    );
    assert!(
        by_uuid("msg-alt").is_sidechain,
        "branch off current_node path must be flagged sidechain"
    );
    assert_eq!(by_uuid("msg-alt").text.as_deref(), Some("Alternate answer"));
}

#[test]
fn test_chatgpt_export_malformed_inputs() {
    // Top level not valid JSON at all -> None (caller must not advance cursor).
    assert!(parse_chatgpt_export("not json").is_none());

    // Valid JSON but not an array -> None.
    assert!(parse_chatgpt_export(r#"{"oops": "not an array"}"#).is_none());

    // Valid array, but individual malformed conversations (missing
    // mapping, missing id, non-object entry) must be skipped
    // individually rather than sinking the whole file.
    let export = serde_json::json!([
        {"id": "no-mapping"},
        {"mapping": {}},
        "not-an-object",
        {
            "id": "conv-good",
            "current_node": "msg-good",
            "mapping": {
                "root": {"id": "root", "message": null, "parent": null, "children": ["msg-good"]},
                "msg-good": {
                    "id": "msg-good",
                    "parent": "root",
                    "children": [],
                    "message": {
                        "id": "msg-good",
                        "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": ["Still works"]}
                    }
                }
            }
        }
    ]);
    let content = serde_json::to_string(&export).unwrap();

    let events = parse_chatgpt_export(&content)
        .expect("array with some malformed conversations still parses");
    assert_eq!(
        events.len(),
        1,
        "malformed conversations skipped, valid one still yields its event"
    );
    assert_eq!(events[0].uuid, "msg-good");
    assert_eq!(events[0].session_id, "conv-good");
}
