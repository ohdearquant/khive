use super::*;

#[tokio::test]
async fn entity_list_wrappers_skip_count_but_exact_consumers_retain_it() {
    use crate::reference_resolution::{resolve_reference, ReferenceResolution};
    use crate::reference_ring::ReferenceRing;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let runtime = rt();
    let token = runtime
        .authorize_with_visibility(
            Namespace::local(),
            vec![Namespace::parse("shared").unwrap()],
        )
        .unwrap();
    let store = runtime.entities(&token).unwrap();
    for index in 0..14 {
        let namespace = match index {
            12 => "shared",
            13 => "hidden",
            _ => "local",
        };
        let mut row = Entity::new(namespace, "document", "counted-name")
            .with_entity_type((index % 2 == 0).then_some("paper"))
            .with_properties(serde_json::json!({"type": "paper", "ordinal": index}))
            .with_description(format!("row-{index}"))
            .with_tags(vec![if index % 2 == 0 { "wanted" } else { "other" }.into()]);
        row.created_at = index;
        row.updated_at = index;
        store.upsert_entity(row).await.unwrap();
    }
    let filter = EntityFilter {
        kinds: vec!["document".into()],
        entity_types: vec!["paper".into()],
        legacy_entity_type_fallback: true,
        namespaces: vec!["local".into(), "shared".into()],
        ..Default::default()
    };
    let tagged_filter = EntityFilter {
        kinds: vec!["document".into()],
        tags_any: vec!["wanted".into()],
        namespaces: filter.namespaces.clone(),
        ..Default::default()
    };
    let page = PageRequest {
        limit: 2,
        offset: 1,
    };
    let expected = store
        .query_entities("local", filter.clone(), page.clone())
        .await
        .unwrap();
    assert_eq!(expected.total, Some(13));
    assert_eq!(expected.items.len(), 2);
    let expected_tagged = store
        .query_entities("local", tagged_filter, page.clone())
        .await
        .unwrap();
    assert_eq!(expected_tagged.total, Some(7));

    // In-memory reads share the writer connection, so every call encounters this authorizer.
    assert_eq!(runtime.backend().pool().max_readers(), 0);
    let denied = Arc::new(AtomicUsize::new(0));
    {
        let attempts = Arc::clone(&denied);
        let writer = runtime.backend().pool().writer().unwrap();
        writer
            .conn()
            .authorizer(Some(move |context: AuthContext<'_>| match context.action {
                AuthAction::Function { function_name }
                    if function_name.eq_ignore_ascii_case("count") =>
                {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            }))
            .unwrap();
    }
    assert!(store
        .query_entities("local", filter.clone(), page.clone())
        .await
        .is_err());
    let control_attempts = denied.load(Ordering::SeqCst);
    assert!(
        control_attempts > 0,
        "exact page must encounter the count authorizer"
    );

    let scalar = runtime
        .list_entities(&token, Some("document"), Some("paper"), 2, 1)
        .await
        .unwrap();
    let composed = runtime
        .list_entities_filtered(
            &token,
            EntityFilter {
                entity_types_by_kind: [("document".into(), vec!["paper".into()])]
                    .into_iter()
                    .collect(),
                namespaces: vec!["hidden".into()],
                ..filter.clone()
            },
            2,
            1,
        )
        .await
        .unwrap();
    let tagged = runtime
        .list_entities_tagged(&token, Some("document"), Some("wanted"), 2, 1)
        .await
        .unwrap();
    let expected_rows = serde_json::to_value(&expected.items).unwrap();
    assert_eq!(serde_json::to_value(scalar).unwrap(), expected_rows);
    assert_eq!(serde_json::to_value(composed).unwrap(), expected_rows);
    assert_eq!(
        serde_json::to_value(tagged).unwrap(),
        serde_json::to_value(&expected_tagged.items).unwrap()
    );
    assert_eq!(denied.load(Ordering::SeqCst), control_attempts);

    assert!(runtime
        .count_entities_tagged(&token, Some("document"), Some("wanted"))
        .await
        .is_err());
    let count_attempts = denied.load(Ordering::SeqCst);
    assert!(count_attempts > control_attempts);
    let ring = ReferenceRing::new();
    assert!(
        resolve_reference(&runtime, &ring, &token, "counted-name", 1, Some("document"))
            .await
            .is_err()
    );
    assert!(denied.load(Ordering::SeqCst) > count_attempts);
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert_eq!(
        store
            .query_entities("local", filter, page)
            .await
            .unwrap()
            .total,
        Some(13)
    );
    assert_eq!(
        runtime
            .count_entities_tagged(&token, Some("document"), Some("wanted"))
            .await
            .unwrap(),
        7
    );
    let ReferenceResolution::Ambiguous { candidates } =
        resolve_reference(&runtime, &ring, &token, "counted-name", 1, Some("document"))
            .await
            .unwrap()
    else {
        panic!("exact-name resolution must retain ambiguity beyond its bounded page");
    };
    assert_eq!(candidates.len(), 10);
}

#[tokio::test]
async fn list_composed_type_filters_apply_alias_and_disjoint_sets_before_pagination() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let store = runtime.entities(&token).unwrap();
    let mut expected = Vec::new();
    for (kind, column, property, matches) in [
        ("document", Some("paper"), "ignored", true),
        ("document", None, "preprint", true),
        ("document", Some("preprint"), "ignored", true),
        ("document", Some("report"), "preprint", false),
        ("concept", None, "preprint", false),
    ] {
        let row = Entity::new("local", kind, "type predicate")
            .with_entity_type(column)
            .with_properties(serde_json::json!({"type": property}));
        if matches {
            expected.push(row.id);
        }
        store.upsert_entity(row).await.unwrap();
    }
    store
        .upsert_entity(
            Entity::new("foreign", "document", "foreign alias")
                .with_properties(serde_json::json!({"type":"preprint"})),
        )
        .await
        .unwrap();
    for (values, should_match) in [
        (vec!["paper".to_string(), "preprint".to_string()], true),
        (vec!["absent".to_string()], false),
        (Vec::new(), false),
    ] {
        let filter = EntityFilter {
            entity_types_by_kind: [("document".to_string(), values)].into_iter().collect(),
            legacy_entity_type_fallback: true,
            namespaces: vec!["foreign".to_string()], // Runtime supplies token visibility.
            ..Default::default()
        };
        let mut offset_ids = Vec::new();
        for offset in 0..=expected.len() {
            let page = runtime
                .list_entities_filtered(&token, filter.clone(), 1, offset as u32)
                .await
                .unwrap();
            offset_ids.extend(page.into_iter().map(|row| row.id));
        }
        let mut cursor_ids = Vec::new();
        let mut after = None;
        for _ in 0..=expected.len() {
            let (page, next) = runtime
                .list_entities_after_filtered(&token, filter.clone(), after, 1)
                .await
                .unwrap();
            cursor_ids.extend(page.into_iter().map(|row| row.id));
            after = next;
            if after.is_none() {
                break;
            }
        }
        let mut wanted = if should_match {
            expected.clone()
        } else {
            Vec::new()
        };
        wanted.sort_unstable();
        offset_ids.sort_unstable();
        cursor_ids.sort_unstable();
        assert_eq!(offset_ids, wanted);
        assert_eq!(cursor_ids, wanted);
    }
    // Existing scalar callers retain literal matching, including legacy fallback.
    assert_eq!(
        runtime
            .list_entities(&token, None, Some("paper"), 20, 0)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        runtime
            .list_entities_after(&token, None, Some("paper"), &[], None, 20)
            .await
            .unwrap()
            .0
            .len(),
        1
    );
}
