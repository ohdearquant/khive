#[tokio::test]
async fn compose_member_omissions_explicit_errors_alias_dedup_and_first_error_order() {
    let dir = TempDir::new().expect("directory");
    let rt = runtime(&dir.path().join("fold.db"));
    let id = "abcdef00-1111-2222-3333-444444444444";
    seed_atom(&rt, id, "local", "named", "draft", false).await;
    seed_atom(&rt, "badc0ffe-one", "local", "bad-one", "reviewed", false).await;
    seed_atom(&rt, "badc0ffe-two", "local", "bad-two", "reviewed", false).await;
    seed_domain(
        &rt,
        "omissions",
        &[
            " missing ".into(),
            "named".into(),
            " missing ".into(),
            id.into(),
        ],
    )
    .await;
    let response=compose(&rt,json!({"query":"code", "domain_ids":["omissions"], "atom_ids":[id,"named"],"blend_kg":false})).await.expect("members tolerate missing; explicit draft opts in");
    assert_eq!(
        response["data"]["omissions"],
        json!([" missing ", " missing "])
    );
    assert_eq!(
        response["data"]["atoms"].as_array().expect("atoms").len(),
        1
    );
    let error = compose(
        &rt,
        json!({"query":"code","atom_ids":["missing","badc0ffe"],"blend_kg":false}),
    )
    .await
    .expect_err("first explicit error");
    assert_eq!(
        error.to_string(),
        load_atom_by_id_or_slug(&rt, "local", "missing")
            .await
            .expect_err("original missing")
            .to_string()
    );
    seed_domain(
        &rt,
        "ambiguous",
        &["missing".into(), "badc0ffe".into(), "named".into()],
    )
    .await;
    let error = compose(
        &rt,
        json!({"query":"code","domain_ids":["ambiguous"],"blend_kg":false}),
    )
    .await
    .expect_err("member prefix remains fatal");
    assert_eq!(
        error.to_string(),
        load_atom_by_id_or_slug(&rt, "local", "badc0ffe")
            .await
            .expect_err("original prefix")
            .to_string()
    );
}

#[test]
fn compose_slug_map_duplicate_slug_and_uuid_members_preserve_linear_first_oracle() {
    let mk = |n, slug: &str| Atom {
        id: Uuid::from_u128(n),
        namespace: "local".into(),
        slug: slug.into(),
        name: "name".into(),
        content: "content".into(),
        tags: "[]".into(),
        properties: None,
        status: None,
        source_uri: None,
        source_type: None,
        finalized: true,
        created_at: 0,
        updated_at: 0,
        deleted_at: None,
    };
    let atoms = vec![mk(1, "same"), mk(2, "other"), mk(3, "same")];
    let members = vec![
        "same".into(),
        "same".into(),
        atoms[1].id.to_string(),
        "absent".into(),
    ];
    let expected: HashSet<_> = members
        .iter()
        .filter_map(|slug| {
            atoms
                .iter()
                .find(|atom| atom.slug == *slug)
                .map(|atom| atom.id.to_string())
        })
        .collect();
    assert_eq!(
        KnowledgeHandlers::compose_domain_member_ids(&atoms, &members),
        expected
    );
    assert_eq!(expected, [atoms[0].id.to_string()].into_iter().collect());
}
