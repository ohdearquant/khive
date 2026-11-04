use super::*;

/// Reach the production post-check write and serializer with a holder inserted
/// after an actual preflight miss. The fixture uses the generated plan rather
/// than rebuilding its key or digest.
async fn assert_checked_replay(
    pack: &MemoryPack,
    token: &NamespaceToken,
    batch: &str,
    different_batch_row: bool,
) {
    let write_token = token.with_namespace(actor_namespace(token).expect("actor namespace"));
    let old = pack
        .remember_permitting(
            token,
            json!({"content": "reed warbler census original"}),
            &[],
        )
        .await
        .expect("seed the old memory");
    let old_id = old["id"]
        .as_str()
        .expect("old memory id")
        .parse::<Uuid>()
        .expect("old memory UUID");
    let raw = json!({
        "id": old_id.to_string(),
        "action": "supersede",
        "content": "reed warbler census revised",
        "properties": {
            "who": "batch-tester",
            "source_ids": ["reed-warbler-field-sheet"],
        },
    });
    let parsed = Parsed {
        echo: echo_of(&raw),
        row: decode_params::<RowInput>(raw).map_err(|message| refusal("invalid-row", message)),
    };
    let checked = pack
        .check_row(token, &write_token, batch, parsed, &HashMap::new())
        .await
        .expect("check the row before a key holder exists");
    let mut holder_params = match &checked.plan {
        Plan::New(planned) => planned.params.clone(),
        _ => panic!("preflight must produce a new write before the fixture inserts a holder"),
    };
    if different_batch_row {
        let digest = holder_params["properties"]["batch_row"]
            .as_str()
            .expect("the plan supplies a batch_row");
        let mut chars = digest.chars();
        let first = chars.next().expect("the generated digest is not empty");
        let changed: String = std::iter::once(if first == '0' { '1' } else { '0' })
            .chain(chars)
            .collect();
        holder_params["properties"]["batch_row"] = json!(changed);
    }
    let holder = pack
        .remember_permitting(token, holder_params, BATCH_WRITTEN_PROPERTY_KEYS)
        .await
        .expect("insert the same-content holder after preflight");
    assert!(
        holder.get("replayed").is_none(),
        "the fixture must insert a fresh holder after the proven preflight miss"
    );
    let holder_id = holder["id"]
        .as_str()
        .expect("holder id")
        .parse::<Uuid>()
        .expect("holder UUID");

    let result = pack
        .apply_checked_rows(token, &write_token, batch, false, vec![checked])
        .await
        .expect("the production checked-row stage reports an entry");
    let entry = &result["entries"][0];
    if different_batch_row {
        assert_eq!(entry["outcome"], "refused");
        assert_eq!(entry["reason"]["code"], "dedupe-name-taken");
        assert!(entry["new_id"].is_null());
        assert_eq!(result["counts"]["refused"], 1);
        assert_eq!(result["counts"]["deduped"], 0);
        assert_eq!(result["counts"]["written"], 0);
    } else {
        assert_eq!(entry["outcome"], "deduped");
        assert_eq!(entry["new_id"], holder_id.to_string());
        assert_eq!(result["counts"]["deduped"], 1);
        assert_eq!(result["counts"]["refused"], 0);
    }

    let edges = pack
        .runtime
        .neighbors_with_query(
            &write_token,
            holder_id,
            khive_storage::types::NeighborQuery {
                direction: khive_storage::types::Direction::Out,
                relations: Some(vec![EdgeRelation::Supersedes]),
                limit: None,
                min_weight: None,
            },
        )
        .await
        .expect("query the holder's supersedes edge");
    assert_eq!(
        edges.len(),
        if different_batch_row { 0 } else { 1 },
        "only a matching replay may assert the supersedes edge"
    );
    if !different_batch_row {
        assert_eq!(edges[0].node_id, old_id);
    }
}

#[tokio::test]
async fn test_apply_batch_replayed_row_with_different_batch_row_is_refused() {
    let rt = crate::test_support::with_receipt_credentials(
        khive_runtime::KhiveRuntime::memory().expect("in-memory runtime"),
    );
    let token = rt
        .authorize(khive_runtime::Namespace::parse("local").expect("local namespace"))
        .expect("authorize local");
    let pack = MemoryPack::new(rt);

    // Both cases use the prepared production stage. The shared-row fixture
    // names its source above, just as check_row requires for a local old row.
    // Keep the successful replay first; separate Tokio tests have no ordering.
    assert_checked_replay(&pack, &token, "replay-matching", false).await;
    assert_checked_replay(&pack, &token, "replay-mismatching", true).await;
}
