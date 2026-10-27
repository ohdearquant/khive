use super::*;
use khive_runtime::PackRuntime;
use khive_storage::note::{NoteStatusSelector, NoteStore};

async fn fixture() -> (KhiveRuntime, NamespaceToken, [Uuid; 4]) {
    let runtime = KhiveRuntime::memory().expect("scratch runtime");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("local token");
    let mut eligible = Vec::new();
    let mut indexed_ids = std::collections::HashSet::new();
    for (index, lifecycle, tag, group, private) in [
        (0, "active", "keep", "eligible", false),
        (1, "archived", "keep", "eligible", false),
        (2, "archived", "keep", "eligible", false),
        (3, "paused", "keep", "eligible", false),
        (4, "archived", "skip", "eligible", false),
        (5, "archived", "keep", "excluded", false),
        (6, "archived", "keep", "eligible", true),
    ] {
        let kind = if private { "message" } else { "observation" };
        let mut note = runtime
            .create_note(
                &token,
                kind,
                None,
                "archivestatusfixture beacon",
                None,
                None,
                vec![],
            )
            .await
            .expect("seed indexed note");
        note.status = lifecycle.into();
        note.properties = Some(json!({
            "tags": [tag], "group": group,
            "status": if lifecycle == "active" { "archived" } else { "active" },
        }));
        if private {
            let properties = note.properties.as_mut().unwrap().as_object_mut().unwrap();
            properties.insert("direction".into(), json!("inbound"));
            properties.insert(
                "to_actor".into(),
                json!(format!("other-{}", token.actor().id)),
            );
        }
        if index < 4 {
            eligible.push(note.id);
        }
        indexed_ids.insert(note.id);
        // Isolated fixture: establish routing/lifecycle through the actual raw backend.
        runtime
            .backend()
            .upsert_note(note)
            .await
            .expect("set lifecycle fixture");
    }
    let candidates = runtime
        .text_for_notes(&token)
        .unwrap()
        .search(khive_storage::types::TextSearchRequest {
            query: "archivestatusfixture beacon".into(),
            mode: TextQueryMode::Plain,
            filter: Some(khive_storage::types::TextFilter {
                namespaces: vec![token.namespace().as_str().to_string()],
                ..Default::default()
            }),
            top_k: 10,
            snippet_chars: 100,
        })
        .await
        .expect("read actual fixture text candidates");
    let candidate_ids = candidates
        .into_iter()
        .map(|hit| hit.subject_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        candidate_ids, indexed_ids,
        "KG_STATUS_FIXTURE_INDEXED_CANDIDATES"
    );
    (runtime, token, eligible.try_into().unwrap())
}

fn args(selector: Option<&str>) -> Value {
    let mut value = json!({
        "kind": "note", "query": "archivestatusfixture beacon", "limit": 10,
        "tags": ["keep"], "properties": {"group": "eligible"},
    });
    if let Some(selector) = selector {
        value["note_status"] = json!(selector);
    }
    value
}

#[tokio::test]
async fn typed_note_selector_defaults_and_entity_scope_are_canonical() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let runtime = KhiveRuntime::memory().unwrap();
            let mut builder = khive_runtime::VerbRegistryBuilder::new();
            builder.register(crate::KgPack::new(runtime));
            let registry = builder.build().unwrap();
            for selector in [None, Some(Value::Null), Some(json!("active")), Some(json!("archived")), Some(json!("any"))] {
                let mut value = json!({"kind": "note", "query": "beacon"});
                if let Some(selector) = &selector { value["note_status"] = selector.clone(); }
                let expected = match selector.as_ref().and_then(Value::as_str) {
                    Some("archived") => NoteStatusSelector::Archived,
                    Some("any") => NoteStatusSelector::Any,
                    _ => NoteStatusSelector::Active,
                };
                let request = ValidatedSearchRequest::from_value(value, &registry).unwrap();
                assert_eq!(request.note_status(), Some(expected), "NOTE_SEARCH_EFFECTIVE_SELECTOR");
            }
            let entity = ValidatedSearchRequest::from_value(json!({"kind": "entity", "query": "beacon"}), &registry).unwrap();
            assert_eq!(entity.note_status(), None);
            for selector in [Value::Null, json!("archived")] {
                let error = ValidatedSearchRequest::from_value(json!({
                    "kind": "entity", "query": "beacon", "note_status": selector,
                }), &registry).unwrap_err();
                assert!(matches!(error, RuntimeError::InvalidInput(ref message) if message.contains("note_status")));
            }
            for invalid in [json!("Active"), json!("paused"), json!(1), json!(true), json!([]), json!({})] {
                assert!(ValidatedSearchRequest::from_value(json!({
                    "kind": "note", "query": "beacon", "note_status": invalid,
                }), &registry).is_err());
            }
        }).await.expect("bounded selector parser regression");
}

#[tokio::test]
async fn real_hook_counts_only_eligible_lifecycle_mismatches_and_raw_stays_array() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (runtime, token, ids) = fixture().await;
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.register(crate::KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        let pack = crate::KgPack::new(runtime);
        for (selector, expected_selector, mut expected_ids, dropped) in [
            (None, NoteStatusSelector::Active, vec![ids[0]], 3),
            (
                Some("archived"),
                NoteStatusSelector::Archived,
                vec![ids[1], ids[2]],
                2,
            ),
            (Some("any"), NoteStatusSelector::Any, ids.to_vec(), 0),
        ] {
            let value = args(selector);
            let outcome = pack
                .dispatch_search_intercepted(value.clone(), &registry, &token)
                .await
                .unwrap();
            let rows = outcome.result.as_array().expect("typed hook result array");
            let mut actual_ids = rows
                .iter()
                .map(|row| Uuid::parse_str(row["id"].as_str().unwrap()).unwrap())
                .collect::<Vec<_>>();
            actual_ids.sort_unstable();
            expected_ids.sort_unstable();
            assert_eq!(
                actual_ids, expected_ids,
                "status/filter/mailbox row witness"
            );
            assert_eq!(
                outcome.metadata,
                Some((expected_selector, dropped)),
                "NOTE_SEARCH_ADMISSION_COUNT"
            );
            let raw = registry.dispatch("search", value).await.unwrap();
            assert!(raw.is_array(), "NOTE_SEARCH_RAW_ARRAY");
            assert_eq!(
                serde_json::to_vec(&raw).unwrap(),
                serde_json::to_vec(&outcome.result).unwrap(),
                "NOTE_SEARCH_ARRAY_BYTES"
            );
            assert!(rows
                .iter()
                .all(|row| row.get("note_status").is_none()
                    && row.get("note_status_dropped").is_none()));
        }
    })
    .await
    .expect("bounded actual KG search regression");
}

#[tokio::test]
async fn empty_and_all_excluded_note_results_retain_typed_zero_or_nonzero() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime
            .authorize(khive_runtime::Namespace::local())
            .unwrap();
        let mut note = runtime
            .create_note(
                &token,
                "observation",
                None,
                "archivedonlyquartz",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        note.status = "archived".into();
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.register(crate::KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        let pack = crate::KgPack::new(runtime);
        for (query, dropped) in [("missingquartz", 0), ("archivedonlyquartz", 1)] {
            let value = json!({"kind": "note", "query": query});
            let outcome = pack
                .dispatch_search_intercepted(value.clone(), &registry, &token)
                .await
                .unwrap();
            assert_eq!(outcome.result, json!([]), "empty must remain an array");
            assert_eq!(
                outcome.metadata,
                Some((NoteStatusSelector::Active, dropped)),
                "NOTE_SEARCH_EMPTY_COUNT"
            );
            let raw = registry.dispatch("search", value).await.unwrap();
            assert!(raw.is_array(), "NOTE_SEARCH_RAW_ARRAY");
            assert_eq!(raw, json!([]));
        }
        let entity = pack
            .dispatch_search_intercepted(
                json!({"kind": "entity", "query": "missingquartz"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
        assert_eq!(entity.result, json!([]));
        assert_eq!(entity.metadata, None, "entity has no note disclosure");
    })
    .await
    .expect("bounded empty note disclosure regression");
}

#[tokio::test]
async fn concurrent_hooks_keep_distinct_selectors_and_counts() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (runtime, token, _) = fixture().await;
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.register(crate::KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        let pack = crate::KgPack::new(runtime);
        let (active, archived) = tokio::join!(
            pack.dispatch_search_intercepted(args(None), &registry, &token),
            pack.dispatch_search_intercepted(args(Some("archived")), &registry, &token),
        );
        assert_eq!(
            active.unwrap().metadata,
            Some((NoteStatusSelector::Active, 3)),
            "NOTE_SEARCH_ACTIVE_ISOLATION"
        );
        assert_eq!(
            archived.unwrap().metadata,
            Some((NoteStatusSelector::Archived, 2)),
            "NOTE_SEARCH_ARCHIVED_ISOLATION"
        );
    })
    .await
    .expect("bounded concurrent KG hook regression");
}

struct NoIntercept;

#[async_trait::async_trait]
impl PackRuntime for NoIntercept {
    fn name(&self) -> &str {
        "no-intercept"
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        &[]
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(json!([]))
    }
}

#[tokio::test]
async fn default_hook_refuses_instead_of_fabricating_note_metadata() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = runtime.authorize(khive_runtime::Namespace::local()).unwrap();
            let registry = khive_runtime::VerbRegistryBuilder::new().build().unwrap();
            let error = NoIntercept.dispatch_search_intercepted(args(None), &registry, &token).await.expect_err("default hook must fail closed");
            assert!(matches!(error, RuntimeError::Internal(ref message) if message == "registered search owner does not support intercepted search"), "NOTE_SEARCH_HOOK_UNAVAILABLE");
        }).await.expect("bounded unavailable hook regression");
}
