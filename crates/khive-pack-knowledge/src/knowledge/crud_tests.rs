// Gate wiring tests: confirm that the secret check integrated into
// upsert_atoms fires on credential-shaped atom content and passes on
// allowlisted content (sha256 hex, UUIDs).  Tests call
// `khive_runtime::secret_gate::check` directly with the same inputs
// that the handler would pass — this proves the gate is reachable without
// requiring a live DB connection.

use khive_runtime::secret_gate::check;
use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
use khive_storage::SqlValue;

#[test]
fn key_only_list_projection_does_not_select_atom_content() {
    let fields = vec!["id".to_string(), "slug".to_string()];
    let columns = super::list_select_columns(Some(&fields));
    assert_eq!(columns, "id, slug, created_at");
    assert!(!columns.contains("content"));
}

#[tokio::test]
async fn get_prefix_query_plan_uses_primary_key_range_seeks() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::KnowledgePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(runtime.backend());

    let mut reader = runtime.sql().reader().await.expect("prefix plan reader");
    let rows = reader
        .explain(
            super::knowledge_get_prefix_statement("0b6cf134").expect("valid compact UUID prefix"),
        )
        .await
        .expect("explain knowledge prefix query");
    let details: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("detail") {
            Some(SqlValue::Text(detail)) => Some(detail.clone()),
            _ => None,
        })
        .collect();

    assert!(
        ["knowledge_domains", "knowledge_atoms"]
            .iter()
            .all(|table| {
                details.iter().any(|detail| {
                    detail.contains(&format!("SEARCH {table}"))
                        && detail.contains("id>?")
                        && detail.contains("id<?")
                }) && !details
                    .iter()
                    .any(|detail| detail.contains(&format!("SCAN {table}")))
            }),
        "knowledge.get short-id tables must use primary-key range seeks: {details:?}"
    );
}

/// `delete_atoms` matches ids and slugs with one statement per column. A
/// single `id IN (..) OR slug IN (..)` predicate makes the no-statistics
/// planner scan the namespace, so a full chunk must still seek an index.
#[tokio::test]
async fn delete_atoms_key_statements_seek_an_index_at_a_full_chunk() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let keys: Vec<String> = (0..super::HYDRATION_ID_CHUNK)
        .map(|i| format!("key-{i}"))
        .collect();
    let mut reader = runtime.sql().reader().await.expect("plan reader");

    for table in ["knowledge_atoms", "knowledge_domains"] {
        let head = format!("SELECT id, slug FROM {table}");
        let lead = vec![SqlValue::Text("local".into())];
        let [by_id, by_slug] = super::key_match_statements(&head, lead, &keys);
        assert_eq!(by_id.params.len(), super::HYDRATION_ID_CHUNK + 1);
        assert_eq!(by_slug.params.len(), super::HYDRATION_ID_CHUNK + 1);

        for (statement, index) in [
            (by_id, format!("sqlite_autoindex_{table}_1")),
            (by_slug, format!("idx_{table}_ns_slug")),
        ] {
            let rows = reader.explain(statement).await.expect("explain statement");
            let details: Vec<String> = rows
                .iter()
                .filter_map(|row| match row.get("detail") {
                    Some(SqlValue::Text(detail)) => Some(detail.clone()),
                    _ => None,
                })
                .collect();
            assert!(
                details
                    .iter()
                    .any(|detail| detail.contains(&format!("USING INDEX {index}"))),
                "{table} key match must seek {index}: {details:?}"
            );
            assert!(
                !details.iter().any(|detail| detail.contains("SCAN")),
                "{table} key match must not scan: {details:?}"
            );
        }
    }
}

#[test]
fn atom_body_with_fake_aws_key_is_blocked() {
    // Fake AWS access key ID in an atom body — must be blocked.
    let body = "provider: aws\naccess_key_id: AKIAFAKE000000000000\nregion: us-east-1";
    assert!(
        check(body).is_err(),
        "atom body containing fake AWS key must be blocked"
    );
}

#[test]
fn atom_body_with_sha256_hash_passes() {
    // A manifest-style line containing a sha256 digest — must pass the allowlist.
    let body = "checksum = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\"";
    assert!(
        check(body).is_ok(),
        "atom body with sha256 manifest hash must pass; fired: {:?}",
        check(body).err()
    );
}

#[test]
fn atom_name_with_fake_openai_key_is_blocked() {
    // A credential accidentally used as an atom name — must be blocked.
    let name = format!("sk-proj-{}", "A".repeat(80));
    assert!(
        check(&name).is_err(),
        "atom name containing fake OpenAI key must be blocked"
    );
}

#[test]
fn normal_atom_name_passes() {
    let name = "FlashAttention-2: efficient transformer self-attention";
    assert!(
        check(name).is_ok(),
        "normal atom name must pass; fired: {:?}",
        check(name).err()
    );
}

// Ignored-param warning coverage: verify that chunk_size and cascade are still
// accepted by the param structs (no deserialization error) and that the fields
// are Some when supplied, confirming the warning branch precondition is satisfiable.

#[test]
fn upsert_atoms_chunk_size_accepted_and_detectable() {
    use crate::knowledge::schema::UpsertAtomsParams;
    let p: UpsertAtomsParams = serde_json::from_value(serde_json::json!({
        "atoms": [{"slug": "s", "name": "n", "content": "placeholder content for test"}],
        "chunk_size": 100,
    }))
    .expect("upsert_atoms params with chunk_size must deserialize without error");
    assert!(
        p.chunk_size.is_some(),
        "chunk_size must be Some when supplied so the warning branch precondition is satisfiable"
    );
}

#[test]
fn delete_atoms_cascade_accepted_and_detectable() {
    use crate::knowledge::schema::DeleteAtomsParams;
    let p: DeleteAtomsParams = serde_json::from_value(serde_json::json!({
        "ids": ["some-atom-id"],
        "cascade": true,
    }))
    .expect("delete_atoms params with cascade must deserialize without error");
    assert!(
        p.cascade.is_some(),
        "cascade must be Some when supplied so the warning branch precondition is satisfiable"
    );
}
