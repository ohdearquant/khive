// ── ADR-046 regression: get(id=proposal_id) returns ProposalCreated payload ──

/// ADR-046:299 — get(id=<proposal_id>) must return the full ProposalCreated
/// event payload: description, changeset, reviewers, parent_id.
/// Before the fix, get returned only projection columns and omitted those fields.
#[tokio::test]
async fn get_proposal_id_returns_proposal_created_payload() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create a parent proposal so we can set parent_id on the amendment proposal.
    // BUG-6 fix: parent_id must reference an existing proposal in proposals_open,
    // not an arbitrary entity UUID.
    let parent_ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "parent proposal",
            "description": "base proposal that the amendment will reference",
            "changeset": {
                "kind": "add_entity",
                "entity": { "kind": "concept", "name": "ParentProposalEntity" }
            }
        }
    }]))
    .unwrap();
    let parent_result = call(
        &client,
        "request",
        json!({"ops": parent_ops, "presentation": "verbose"}),
    )
    .await?;
    let parent_body: Value = serde_json::from_str(&first_text(&parent_result))?;
    let parent_first = &parent_body["results"][0];
    assert_eq!(
        parent_first["ok"], true,
        "parent propose must succeed; got: {parent_first}"
    );
    let parent_id = parent_first["result"]["id"]
        .as_str()
        .expect("parent id")
        .to_string();
    assert_eq!(parent_id.len(), 36, "parent id must be full UUID");

    // Propose with all optional fields populated: description, reviewers, parent_id,
    // and a changeset that carries a named entity.
    let ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "get-payload regression",
            "description": "ADR-046:299 regression — description must survive get()",
            "changeset": {
                "kind": "add_entity",
                "entity": {
                    "kind": "concept",
                    "name": "PayloadRegressionEntity"
                }
            },
            "reviewers": ["alice", "bob"],
            "parent_id": parent_id
        }
    }]))
    .unwrap();
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], true, "propose must succeed; got: {first}");
    let proposal_id = first["result"]["id"]
        .as_str()
        .expect("propose must return id")
        .to_string();
    assert_eq!(proposal_id.len(), 36, "id from propose must be full UUID");
    assert!(
        first["result"].get("proposal_id").is_none(),
        "propose result must NOT contain old proposal_id key; got: {}",
        first["result"]
    );

    // Now get(id=<proposal_id>) — must return the ProposalCreated event payload.
    let get_result = ok_one(&client, &format!(r#"get(id="{proposal_id}")"#)).await?;

    // ADR-046:299: the four previously-missing fields must be present.
    assert_eq!(
        get_result["description"].as_str().unwrap_or(""),
        "ADR-046:299 regression — description must survive get()",
        "get(id=proposal_id) must return description from ProposalCreated payload"
    );
    let reviewers = get_result["reviewers"]
        .as_array()
        .expect("get(id=proposal_id) must return reviewers array");
    assert_eq!(
        reviewers.len(),
        2,
        "get(id=proposal_id) must return all reviewers; got: {reviewers:?}"
    );
    assert!(
        reviewers.iter().any(|r| r.as_str() == Some("alice")),
        "reviewers must include alice; got: {reviewers:?}"
    );
    assert!(
        reviewers.iter().any(|r| r.as_str() == Some("bob")),
        "reviewers must include bob; got: {reviewers:?}"
    );
    let changeset = &get_result["changeset"];
    assert!(
        !changeset.is_null(),
        "get(id=proposal_id) must return changeset; got null"
    );
    assert_eq!(
        changeset["kind"].as_str().unwrap_or(""),
        "add_entity",
        "changeset kind must be add_entity; got: {changeset}"
    );
    // parent_id is stored as Id128 (numeric); check it round-trips to a non-null value.
    assert!(
        !get_result["parent_id"].is_null(),
        "get(id=proposal_id) must return parent_id when set; got: {get_result}"
    );
    assert!(
        get_result.get("proposal_id").is_none(),
        "get(id=proposal_uuid) must NOT return old proposal_id key; got: {get_result}"
    );

    Ok(())
}

// ── ADR-046 regression: list(kind=proposal) unfiltered returns all rows ───────

/// ADR-046:277-279 — list(kind=proposal) without a status filter must return
/// ALL rows including applied/withdrawn (audit trail).
/// Before the fix, no-status defaulted to status IN ('open','changes_requested'),
/// hiding audit rows.
#[tokio::test]
async fn list_proposals_without_status_returns_all_rows() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create two proposals.
    let ops = serde_json::to_string(&json!([
        {
            "tool": "propose",
            "args": {
                "title": "audit-row-A",
                "description": "first proposal",
                "changeset": {
                    "kind": "add_entity",
                    "entity": {"kind": "concept", "name": "AuditEntityA"}
                },
                "reviewers": []
            }
        },
        {
            "tool": "propose",
            "args": {
                "title": "audit-row-B",
                "description": "second proposal",
                "changeset": {
                    "kind": "add_entity",
                    "entity": {"kind": "concept", "name": "AuditEntityB"}
                },
                "reviewers": []
            }
        }
    ]))
    .unwrap();
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["results"][0]["ok"], true, "first propose must succeed");
    assert_eq!(
        body["results"][1]["ok"], true,
        "second propose must succeed"
    );
    let pid_a = body["results"][0]["result"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        body["results"][0]["result"].get("proposal_id").is_none(),
        "propose result must NOT contain old proposal_id key; got: {}",
        body["results"][0]["result"]
    );

    // Withdraw proposal A so it moves to a terminal status.
    let ops_withdraw = serde_json::to_string(&json!([{
        "tool": "withdraw",
        "args": {
            "id": pid_a,
            "rationale": "test withdrawal for audit list"
        }
    }]))
    .unwrap();
    let wr = call(
        &client,
        "request",
        json!({"ops": ops_withdraw, "presentation": "verbose"}),
    )
    .await?;
    let wr_body: Value = serde_json::from_str(&first_text(&wr))?;
    assert_eq!(
        wr_body["results"][0]["ok"], true,
        "withdraw must succeed; got: {}",
        wr_body["results"][0]
    );
    assert!(
        wr_body["results"][0]["result"].get("proposal_id").is_none(),
        "withdraw result must NOT contain old proposal_id key; got: {}",
        wr_body["results"][0]["result"]
    );

    // list(kind=proposal) without status — must return BOTH rows (open + withdrawn).
    let list_result = ok_one(&client, r#"list(kind="proposal")"#).await?;
    let items = list_result["items"]
        .as_array()
        .expect("list(kind=proposal) must return an items envelope");
    assert!(
        items.len() >= 2,
        "list(kind=proposal) without status must include all rows (audit trail); \
         got {} items — withdrawn proposal must not be hidden",
        items.len()
    );
    // All list rows must expose `id`, not the old `proposal_id` key.
    for item in items.iter() {
        assert!(
            item.get("proposal_id").is_none(),
            "list(kind=proposal) row must NOT contain proposal_id key; got: {item}"
        );
        assert!(
            item.get("id").is_some(),
            "list(kind=proposal) row must contain id key; got: {item}"
        );
    }

    // list(kind=proposal, status=open) — must return only the open one.
    let list_open = ok_one(&client, r#"list(kind="proposal", status="open")"#).await?;
    let open_items = list_open["items"]
        .as_array()
        .expect("list(kind=proposal, status=open) must return an items envelope");
    assert!(
        open_items
            .iter()
            .all(|i| i["status"].as_str() == Some("open")),
        "list(kind=proposal, status=open) must return only open proposals; got: {open_items:?}"
    );

    Ok(())
}
