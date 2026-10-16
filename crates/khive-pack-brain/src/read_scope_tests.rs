use super::*;
use khive_types::EventKind;

async fn seed_actor_events(rt: &KhiveRuntime, token: &NamespaceToken) {
    for actor in ["caller-a", "actor:caller-a", "caller-b"] {
        super::event_counts_tests::seed_event(
            rt,
            token,
            "search",
            EventKind::SearchExecuted,
            actor,
            1_000_000,
            json!({}),
        )
        .await;
    }
}

async fn seed_actor_bindings(pack: &BrainPack, token: &NamespaceToken) {
    let registry = empty_registry();
    for (actor, profile) in [("caller-a", "profile-a"), ("caller-b", "profile-b")] {
        pack.dispatch(
            "brain.create_profile",
            json!({"name": profile, "consumer_kind": "recall"}),
            &registry,
            token,
        )
        .await
        .expect("create actor profile");
        pack.dispatch(
            "brain.activate",
            json!({"profile_id": profile}),
            &registry,
            token,
        )
        .await
        .expect("activate actor profile");
        pack.dispatch(
            "brain.bind",
            json!({
                "actor": actor,
                "profile_id": profile,
                "consumer_kind": "recall",
            }),
            &registry,
            token,
        )
        .await
        .expect("bind actor profile");
    }
}

fn event_params() -> Value {
    json!({"since": "1970-01-01T00:00:00Z"})
}

fn assert_actor_refused(err: RuntimeError, actor: &str) {
    let RuntimeError::InvalidInput(message) = err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert!(
        message.contains(actor),
        "actor missing from refusal: {message}"
    );
}

#[tokio::test]
async fn omitted_actor_counts_only_caller_and_collapses_event_aliases() {
    let (pack, rt) = make_pack_with_read_scope(Some("serving-actor"), &[], &[]);
    let (_, caller_rt) = make_pack_with_read_scope(Some("caller-a"), &["caller-b"], &[]);
    let token = caller_rt
        .authorize_with_visibility(Namespace::local(), caller_rt.visible_namespaces().to_vec())
        .expect("caller token");
    let registry = empty_registry();
    seed_actor_events(&rt, &token).await;

    let own = pack
        .dispatch("brain.event_counts", event_params(), &registry, &token)
        .await
        .expect("default caller counts");
    assert_eq!(own["counts_by_actor"], json!({"caller-a": 2}));
    assert_eq!(own["window_event_total"], json!(2));
    assert_eq!(own["total"], json!(2));

    let mut params = event_params();
    params["actor"] = json!("caller-a");
    let explicit = pack
        .dispatch("brain.event_counts", params.clone(), &registry, &token)
        .await
        .expect("explicit actor preserves stored grouping");
    assert_eq!(
        explicit["counts_by_actor"],
        json!({"caller-a": 1, "actor:caller-a": 1})
    );
    assert_eq!(explicit["window_event_total"], json!(2));
    params["actor"] = json!("caller-b");
    let other = pack
        .dispatch("brain.event_counts", params, &registry, &token)
        .await
        .expect("visible actor control");
    assert_eq!(other["counts_by_actor"], json!({"caller-b": 1}));
    assert_eq!(other["window_event_total"], json!(1));
}

#[tokio::test]
async fn omitted_actor_reads_only_caller_bindings_and_resolution() {
    let (pack, _) = make_pack_with_read_scope(Some("serving-actor"), &[], &[]);
    let (_, caller_rt) = make_pack_with_read_scope(Some("caller-a"), &["caller-b"], &[]);
    let token = caller_rt
        .authorize_with_visibility(Namespace::local(), caller_rt.visible_namespaces().to_vec())
        .expect("caller token");
    let registry = empty_registry();
    seed_actor_bindings(&pack, &token).await;

    let own_bindings = pack
        .dispatch("brain.bindings", json!({}), &registry, &token)
        .await
        .expect("default caller bindings");
    assert_eq!(own_bindings["count"], json!(1));
    assert_eq!(own_bindings["bindings"][0]["actor"], json!("caller-a"));
    let own_profile = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .expect("default caller profile");
    assert_eq!(own_profile["resolved_profile_id"], json!("profile-a"));

    let other = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall", "actor": "caller-b"}),
            &registry,
            &token,
        )
        .await
        .expect("visible actor profile control");
    assert_eq!(other["resolved_profile_id"], json!("profile-b"));
}

#[tokio::test]
async fn foreign_actor_requires_visibility_for_each_read_verb() {
    let (pack, rt) = make_pack_with_read_scope(Some("caller-a"), &[], &[]);
    let hidden_token = rt.authorize(Namespace::local()).expect("caller token");
    let (_, visible_rt) = make_pack_with_read_scope(Some("caller-a"), &["caller-b"], &[]);
    let visible_token = visible_rt
        .authorize_with_visibility(Namespace::local(), visible_rt.visible_namespaces().to_vec())
        .expect("caller token with configured visibility");
    let registry = empty_registry();
    seed_actor_events(&rt, &hidden_token).await;
    seed_actor_bindings(&pack, &hidden_token).await;

    for (verb, mut params) in [
        ("brain.event_counts", event_params()),
        ("brain.resolve", json!({"consumer_kind": "recall"})),
        ("brain.bindings", json!({})),
    ] {
        params["actor"] = json!("caller-b");
        let denied = pack
            .dispatch(verb, params.clone(), &registry, &hidden_token)
            .await
            .expect_err("foreign actor without visibility must be refused");
        assert_actor_refused(denied, "caller-b");

        let allowed = pack
            .dispatch(verb, params, &registry, &visible_token)
            .await
            .expect("the same foreign actor is readable with visibility");
        match verb {
            "brain.event_counts" => {
                assert_eq!(allowed["counts_by_actor"], json!({"caller-b": 1}));
                assert_eq!(allowed["window_event_total"], json!(1));
            }
            "brain.resolve" => {
                assert_eq!(allowed["resolved_profile_id"], json!("profile-b"));
            }
            "brain.bindings" => {
                assert_eq!(allowed["count"], json!(1));
                assert_eq!(allowed["bindings"][0]["actor"], json!("caller-b"));
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn prefixed_foreign_event_actor_requires_visibility() {
    let (pack, rt) = make_pack_with_read_scope(Some("caller-a"), &[], &[]);
    let hidden_token = rt.authorize(Namespace::local()).expect("caller token");
    let (_, visible_rt) = make_pack_with_read_scope(Some("caller-a"), &["caller-b"], &[]);
    let visible_token = visible_rt
        .authorize_with_visibility(Namespace::local(), visible_rt.visible_namespaces().to_vec())
        .expect("caller token with configured visibility");
    let registry = empty_registry();
    seed_actor_events(&rt, &hidden_token).await;
    super::event_counts_tests::seed_event(
        &rt,
        &hidden_token,
        "search",
        EventKind::SearchExecuted,
        "actor:caller-b",
        1_000_000,
        json!({}),
    )
    .await;
    let mut params = event_params();
    params["actor"] = json!("actor:caller-b");
    let denied = pack
        .dispatch(
            "brain.event_counts",
            params.clone(),
            &registry,
            &hidden_token,
        )
        .await
        .expect_err("a stored actor prefix cannot bypass visibility");
    assert_actor_refused(denied, "caller-b");
    let prefixed = pack
        .dispatch(
            "brain.event_counts",
            params.clone(),
            &registry,
            &visible_token,
        )
        .await
        .expect("visible prefixed actor keeps exact stored matching");
    assert_eq!(prefixed["counts_by_actor"], json!({"actor:caller-b": 1}));
    assert_eq!(prefixed["window_event_total"], json!(1));
    params["actor"] = json!("caller-b");
    let bare = pack
        .dispatch("brain.event_counts", params, &registry, &visible_token)
        .await
        .expect("visible bare actor matches both stored spellings");
    assert_eq!(
        bare["counts_by_actor"],
        json!({"caller-b": 1, "actor:caller-b": 1})
    );
    assert_eq!(bare["window_event_total"], json!(2));
}

#[tokio::test]
async fn attributed_prefixed_actor_ids_keep_event_counts_principal_scoped() {
    let (pack, rt) = make_pack_with_read_scope(Some("serving-actor"), &[], &[]);
    let (_, caller_a_rt) = make_pack_with_read_scope(Some("caller-a"), &[], &[]);
    let (_, caller_b_rt) = make_pack_with_read_scope(Some("actor:caller-a"), &[], &[]);
    let token_a = caller_a_rt
        .authorize(Namespace::local())
        .expect("caller A token");
    let token_b = caller_b_rt
        .authorize(Namespace::local())
        .expect("caller B token");
    let (_, visible_a_rt) = make_pack_with_read_scope(Some("caller-a"), &["actor:caller-a"], &[]);
    let (_, visible_b_rt) = make_pack_with_read_scope(Some("actor:caller-a"), &["caller-a"], &[]);
    let visible_a = visible_a_rt
        .authorize_with_visibility(
            Namespace::local(),
            visible_a_rt.visible_namespaces().to_vec(),
        )
        .expect("caller A token with caller B visible");
    let visible_b = visible_b_rt
        .authorize_with_visibility(
            Namespace::local(),
            visible_b_rt.visible_namespaces().to_vec(),
        )
        .expect("caller B token with caller A visible");
    let registry = empty_registry();
    for (token, canonical_actor, count) in [
        (&token_a, "actor:caller-a", 1),
        (&token_b, "actor:actor:caller-a", 2),
    ] {
        let events = rt.events(token).expect("attributed event store");
        for _ in 0..count {
            let mut event = khive_storage::event::Event::new(
                token.namespace().as_str(),
                "search",
                EventKind::SearchExecuted,
                khive_types::SubstrateKind::Note,
                "caller-supplied",
            );
            event.created_at = 1_000_000;
            event.payload = json!({"result_kind": "note"});
            let id = event.id;
            events
                .append_event(event)
                .await
                .expect("append attributed event");
            let stored = events.get_event(id).await.unwrap().unwrap();
            assert_eq!(stored.actor, canonical_actor);
        }
    }

    for exhaustive in [false, true] {
        let mut params = event_params();
        params["exhaustive"] = json!(exhaustive);
        for (token, caller, canonical_actor, count) in [
            (&token_a, "caller-a", "actor:caller-a", 1),
            (&token_b, "actor:caller-a", "actor:actor:caller-a", 2),
        ] {
            let own = pack
                .dispatch("brain.event_counts", params.clone(), &registry, token)
                .await
                .expect("default scope reads only the attributed principal");
            let mut expected = json!({});
            expected[caller] = json!(count);
            assert_eq!(own["counts_by_actor"], expected);
            assert_eq!(own["total"], json!(count));
            assert_eq!(own["window_event_total"], json!(count));

            let mut explicit_params = params.clone();
            explicit_params["actor"] = json!(canonical_actor);
            let explicit = pack
                .dispatch("brain.event_counts", explicit_params, &registry, token)
                .await
                .expect("canonical self scope reads only the attributed principal");
            let mut expected = json!({});
            expected[canonical_actor] = json!(count);
            assert_eq!(explicit["counts_by_actor"], expected);
            assert_eq!(explicit["total"], json!(count));
            assert_eq!(explicit["window_event_total"], json!(count));
        }

        let mut bare_params = params.clone();
        bare_params["actor"] = json!("caller-a");
        let bare = pack
            .dispatch("brain.event_counts", bare_params, &registry, &token_a)
            .await
            .expect("ordinary bare self filter still matches canonical events");
        assert_eq!(bare["counts_by_actor"], json!({"actor:caller-a": 1}));
        assert_eq!(bare["total"], json!(1));
        assert_eq!(bare["window_event_total"], json!(1));

        for (hidden, visible, other, canonical_actor, count) in [
            (
                &token_a,
                &visible_a,
                "actor:caller-a",
                "actor:actor:caller-a",
                2,
            ),
            (&token_b, &visible_b, "caller-a", "actor:caller-a", 1),
        ] {
            let mut foreign_params = params.clone();
            foreign_params["actor"] = json!(canonical_actor);
            let denied = pack
                .dispatch(
                    "brain.event_counts",
                    foreign_params.clone(),
                    &registry,
                    hidden,
                )
                .await
                .expect_err("canonical foreign scope requires the other principal to be visible");
            assert_actor_refused(denied, other);
            let allowed = pack
                .dispatch("brain.event_counts", foreign_params, &registry, visible)
                .await
                .expect("explicit foreign scope is allowed with configured visibility");
            let mut expected = json!({});
            expected[canonical_actor] = json!(count);
            assert_eq!(allowed["counts_by_actor"], expected);
            assert_eq!(allowed["total"], json!(count));
            assert_eq!(allowed["window_event_total"], json!(count));
        }
    }
}

async fn append_attributed_actor_events(rt: &KhiveRuntime, token: &NamespaceToken, count: usize) {
    let events = rt.events(token).expect("attributed event store");
    let canonical_actor = format!("{}:{}", token.actor().kind, token.actor().id);
    for _ in 0..count {
        let mut event = khive_storage::event::Event::new(
            token.namespace().as_str(),
            "search",
            EventKind::SearchExecuted,
            khive_types::SubstrateKind::Note,
            "caller-supplied",
        );
        event.created_at = 1_000_000;
        event.payload = json!({"result_kind": "note"});
        let id = event.id;
        events
            .append_event(event)
            .await
            .expect("append attributed event");
        let stored = events.get_event(id).await.unwrap().unwrap();
        assert_eq!(stored.actor, canonical_actor);
    }
}

async fn assert_stamped_kind_raw_id_scope(kind: &str, raw_id: &str) {
    let (pack, rt) = make_pack_with_read_scope(Some("serving-actor"), &[], &[]);
    let (_, caller_rt) = make_pack_with_read_scope(Some(raw_id), &[], &[]);
    let token = caller_rt
        .authorize(Namespace::local())
        .expect("named caller token");
    assert_eq!(token.actor().kind, "actor");
    assert_eq!(token.actor().id, raw_id);
    append_attributed_actor_events(&rt, &token, 2).await;

    let (_, unrelated_rt) = make_pack_with_read_scope(Some("unrelated-caller"), &[], &[]);
    let unrelated = unrelated_rt
        .authorize(Namespace::local())
        .expect("unrelated token");
    append_attributed_actor_events(&rt, &unrelated, 3).await;

    let visible_identity = match kind {
        "actor" => {
            let (_, other_rt) = make_pack_with_read_scope(Some("x"), &[], &[]);
            let other = other_rt
                .authorize(Namespace::local())
                .expect("other actor token");
            append_attributed_actor_events(&rt, &other, 1).await;
            "x"
        }
        "anonymous" => {
            let (_, other_rt) = make_pack_with_read_scope(None, &[], &[]);
            let other = other_rt
                .authorize(Namespace::local())
                .expect("anonymous token");
            append_attributed_actor_events(&rt, &other, 1).await;
            for exhaustive in [false, true] {
                let mut params = event_params();
                params["actor"] = json!(raw_id);
                params["exhaustive"] = json!(exhaustive);
                let own = pack
                    .dispatch("brain.event_counts", params, &empty_registry(), &other)
                    .await
                    .expect("anonymous canonical self filter uses structural identity");
                assert_eq!(own["counts_by_actor"], json!({"anonymous:local": 1}));
                assert_eq!(own["total"], json!(1));
                assert_eq!(own["window_event_total"], json!(1));
            }
            raw_id
        }
        "agent" => {
            // Agent-kind tokens have no public runtime minting path; use the
            // trusted fixture for this reserved canonical stamp.
            super::event_counts_tests::seed_event(
                &rt,
                &token,
                "search",
                EventKind::SearchExecuted,
                raw_id,
                1_000_000,
                json!({"result_kind": "note"}),
            )
            .await;
            raw_id
        }
        _ => panic!("unexpected stamped actor kind {kind}"),
    };
    let (_, visible_rt) = make_pack_with_read_scope(Some(raw_id), &[visible_identity], &[]);
    let visible = visible_rt
        .authorize_with_visibility(Namespace::local(), visible_rt.visible_namespaces().to_vec())
        .expect("named caller token with the other principal visible");
    let canonical_self = format!("actor:{raw_id}");
    let registry = empty_registry();
    for exhaustive in [false, true] {
        let mut params = event_params();
        params["exhaustive"] = json!(exhaustive);
        let own = pack
            .dispatch("brain.event_counts", params.clone(), &registry, &token)
            .await
            .expect("default scope excludes the ambiguous raw alias");
        let mut expected = json!({});
        expected[raw_id] = json!(2);
        assert_eq!(own["counts_by_actor"], expected);
        assert_eq!(own["total"], json!(2));
        assert_eq!(own["window_event_total"], json!(2));

        params["actor"] = json!(canonical_self);
        let explicit_self = pack
            .dispatch("brain.event_counts", params.clone(), &registry, &token)
            .await
            .expect("canonical self filter identifies the named principal");
        let mut expected = json!({});
        expected[canonical_self.as_str()] = json!(2);
        assert_eq!(explicit_self["counts_by_actor"], expected);
        assert_eq!(explicit_self["total"], json!(2));
        assert_eq!(explicit_self["window_event_total"], json!(2));

        params["actor"] = json!(raw_id);
        let denied = pack
            .dispatch("brain.event_counts", params.clone(), &registry, &token)
            .await
            .expect_err("a matching raw id cannot authorize another principal's canonical stamp");
        assert_actor_refused(denied, visible_identity);
        let other = pack
            .dispatch("brain.event_counts", params, &registry, &visible)
            .await
            .expect("visible stamped-kind filter reads exactly the other principal");
        let mut expected = json!({});
        expected[raw_id] = json!(1);
        assert_eq!(other["counts_by_actor"], expected);
        assert_eq!(other["total"], json!(1));
        assert_eq!(other["window_event_total"], json!(1));
    }
}

#[test]
fn stamped_actor_kind_cases_cover_runtime_kinds() {
    let mut kinds = khive_runtime::RUNTIME_STAMPED_ACTOR_KINDS.to_vec();
    kinds.sort_unstable();
    assert_eq!(kinds, ["actor", "agent", "anonymous"]);
}

#[tokio::test]
async fn default_actor_prefixed_raw_id_excludes_colliding_principal_events() {
    assert_stamped_kind_raw_id_scope("actor", "actor:x").await;
}

#[tokio::test]
async fn default_anonymous_prefixed_raw_id_excludes_colliding_principal_events() {
    assert_stamped_kind_raw_id_scope("anonymous", "anonymous:local").await;
}

#[tokio::test]
async fn default_agent_prefixed_raw_id_excludes_colliding_principal_events() {
    assert_stamped_kind_raw_id_scope("agent", "agent:x").await;
}

#[tokio::test]
async fn default_ordinary_actor_preserves_bare_historical_alias() {
    let (pack, rt) = make_pack_with_read_scope(Some("caller-a"), &[], &[]);
    let token = rt
        .authorize(Namespace::local())
        .expect("ordinary actor token");
    append_attributed_actor_events(&rt, &token, 2).await;
    super::event_counts_tests::seed_event(
        &rt,
        &token,
        "search",
        EventKind::SearchExecuted,
        "caller-a",
        1_000_000,
        json!({"result_kind": "note"}),
    )
    .await;
    let (_, other_rt) = make_pack_with_read_scope(Some("unrelated-caller"), &[], &[]);
    let other = other_rt
        .authorize(Namespace::local())
        .expect("unrelated token");
    append_attributed_actor_events(&rt, &other, 3).await;
    let registry = empty_registry();
    for exhaustive in [false, true] {
        let mut params = event_params();
        params["exhaustive"] = json!(exhaustive);
        let own = pack
            .dispatch("brain.event_counts", params.clone(), &registry, &token)
            .await
            .expect("ordinary default scope includes its historical alias");
        assert_eq!(own["counts_by_actor"], json!({"caller-a": 3}));
        assert_eq!(own["total"], json!(3));
        assert_eq!(own["window_event_total"], json!(3));

        params["actor"] = json!("caller-a");
        let bare = pack
            .dispatch("brain.event_counts", params.clone(), &registry, &token)
            .await
            .expect("ordinary bare filter preserves both stored spellings");
        assert_eq!(
            bare["counts_by_actor"],
            json!({"caller-a": 1, "actor:caller-a": 2})
        );
        assert_eq!(bare["total"], json!(3));
        assert_eq!(bare["window_event_total"], json!(3));

        params["actor"] = json!("actor:caller-a");
        let canonical = pack
            .dispatch("brain.event_counts", params, &registry, &token)
            .await
            .expect("ordinary canonical filter remains exact");
        assert_eq!(canonical["counts_by_actor"], json!({"actor:caller-a": 2}));
        assert_eq!(canonical["total"], json!(2));
        assert_eq!(canonical["window_event_total"], json!(2));
    }
}

#[tokio::test]
async fn all_actor_event_counts_require_serving_runtime_fleet_reader() {
    let (pack, rt) = make_pack_with_read_scope(Some("caller-a"), &[], &["caller-a"]);
    let token = rt
        .authorize(Namespace::local())
        .expect("listed caller token");
    let registry = empty_registry();
    seed_actor_events(&rt, &token).await;

    let mut params = event_params();
    params["all_actors"] = json!(true);
    let fleet = pack
        .dispatch("brain.event_counts", params.clone(), &registry, &token)
        .await
        .expect("listed caller can read all actors");
    assert_eq!(fleet["total"], json!(3));
    assert_eq!(fleet["window_event_total"], json!(3));
    assert_eq!(
        fleet["counts_by_actor"],
        json!({"caller-a": 1, "actor:caller-a": 1, "caller-b": 1})
    );

    let own = pack
        .dispatch("brain.event_counts", event_params(), &registry, &token)
        .await
        .expect("listed readers still default to their own actor");
    assert_eq!(own["counts_by_actor"], json!({"caller-a": 2}));

    let (_, other_rt) = make_pack_with_read_scope(Some("caller-b"), &["caller-a"], &["caller-b"]);
    let other_token = other_rt.authorize(Namespace::local()).expect("other token");
    let denied = pack
        .dispatch(
            "brain.event_counts",
            params.clone(),
            &registry,
            &other_token,
        )
        .await
        .expect_err("client configuration cannot grant fleet read access");
    assert_actor_refused(denied, "caller-b");

    let (unlisted_pack, _) = make_pack_with_read_scope(Some("caller-a"), &[], &[]);
    let denied = unlisted_pack
        .dispatch("brain.event_counts", params, &registry, &token)
        .await
        .expect_err("an empty serving allowlist must deny even a listed client");
    assert_actor_refused(denied, "caller-a");
}

#[tokio::test]
async fn all_actors_conflicts_with_explicit_actor() {
    let (pack, rt) = make_pack_with_read_scope(Some("caller-a"), &["caller-b"], &["caller-a"]);
    let token = rt
        .authorize_with_visibility(Namespace::local(), rt.visible_namespaces().to_vec())
        .expect("listed caller token");
    let registry = empty_registry();
    seed_actor_events(&rt, &token).await;
    let mut params = event_params();
    params["actor"] = json!("caller-b");
    params["all_actors"] = json!(false);
    let scoped = pack
        .dispatch("brain.event_counts", params.clone(), &registry, &token)
        .await
        .expect("false all_actors permits an explicit visible actor");
    assert_eq!(scoped["counts_by_actor"], json!({"caller-b": 1}));
    params["all_actors"] = json!(true);
    let denied = pack
        .dispatch("brain.event_counts", params, &registry, &token)
        .await
        .expect_err("fleet scope and an actor filter conflict");
    let RuntimeError::InvalidInput(message) = denied else {
        panic!("expected InvalidInput, got {denied:?}");
    };
    assert!(message.contains("all_actors"), "{message}");
    assert!(message.contains("actor"), "{message}");
}

#[tokio::test]
async fn anonymous_reads_scope_events_and_bindings_and_resolve_wildcards() {
    let (pack, rt) = make_pack_with_read_scope(None, &["caller-b"], &[]);
    let token = rt
        .authorize_with_visibility(Namespace::local(), rt.visible_namespaces().to_vec())
        .expect("anonymous token");
    let registry = empty_registry();
    assert_eq!(token.actor().kind, "anonymous");
    pack.dispatch("brain.mark_turn", json!({}), &registry, &token)
        .await
        .expect("anonymous actor event");
    for actor in ["caller-b", "local", "actor:local", "actor:anonymous:local"] {
        super::event_counts_tests::seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            actor,
            1_000_000,
            json!({}),
        )
        .await;
    }
    let own = pack
        .dispatch("brain.event_counts", event_params(), &registry, &token)
        .await
        .expect("anonymous caller counts");
    assert_eq!(own["counts_by_actor"], json!({"anonymous:local": 1}));
    assert_eq!(own["window_event_total"], json!(1));
    let mut params = event_params();
    params["actor"] = json!("caller-b");
    let other = pack
        .dispatch("brain.event_counts", params, &registry, &token)
        .await
        .expect("visible actor control for anonymous caller");
    assert_eq!(other["counts_by_actor"], json!({"caller-b": 1}));

    seed_actor_bindings(&pack, &token).await;
    pack.dispatch(
        "brain.bind",
        json!({
            "actor": "anonymous:local",
            "profile_id": "profile-b",
            "consumer_kind": "recall",
        }),
        &registry,
        &token,
    )
    .await
    .expect("anonymous actor binding");
    pack.dispatch(
        "brain.bind",
        json!({
            "actor": "local",
            "profile_id": "profile-b",
            "consumer_kind": "recall",
        }),
        &registry,
        &token,
    )
    .await
    .expect("explicit local actor binding");
    pack.dispatch(
        "brain.bind",
        json!({
            "actor": "*",
            "profile_id": "profile-a",
            "consumer_kind": "recall",
        }),
        &registry,
        &token,
    )
    .await
    .expect("wildcard profile binding");
    let bindings = pack
        .dispatch("brain.bindings", json!({}), &registry, &token)
        .await
        .expect("anonymous caller bindings");
    assert_eq!(bindings["count"], json!(1));
    assert_eq!(bindings["bindings"][0]["actor"], json!("anonymous:local"));
    let resolved = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall"}),
            &registry,
            &token,
        )
        .await
        .expect("anonymous caller profile");
    assert_eq!(resolved["resolved_profile_id"], json!("profile-a"));
    let explicit = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall", "actor": "anonymous:local"}),
            &registry,
            &token,
        )
        .await
        .expect("explicit anonymous actor binding control");
    assert_eq!(explicit["resolved_profile_id"], json!("profile-b"));
    let local = pack
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind": "recall", "actor": "local"}),
            &registry,
            &token,
        )
        .await
        .expect("explicit local actor binding control");
    assert_eq!(local["resolved_profile_id"], json!("profile-b"));
    let local_bindings = pack
        .dispatch(
            "brain.bindings",
            json!({"actor": "local"}),
            &registry,
            &token,
        )
        .await
        .expect("explicit local actor bindings control");
    assert_eq!(local_bindings["count"], json!(1));
    assert_eq!(local_bindings["bindings"][0]["actor"], json!("local"));
}

#[tokio::test]
async fn actor_read_params_reject_unknown_fields_and_wrong_types() {
    let (pack, rt) = make_pack_with_read_scope(Some("caller-a"), &[], &[]);
    let token = rt.authorize(Namespace::local()).expect("caller token");
    let registry = empty_registry();
    for (verb, params) in [
        ("brain.event_counts", event_params()),
        ("brain.resolve", json!({"consumer_kind": "recall"})),
        ("brain.bindings", json!({})),
    ] {
        pack.dispatch(verb, params.clone(), &registry, &token)
            .await
            .expect("valid params control");
        for (field, value) in [
            ("unknown_scope", json!(true)),
            ("actor", json!(42)),
            ("all_actors", json!("true")),
        ] {
            if verb == "brain.bindings" && field != "actor" {
                continue;
            }
            let mut invalid = params.clone();
            invalid[field] = value;
            let err = pack
                .dispatch(verb, invalid, &registry, &token)
                .await
                .expect_err("invalid actor read params must fail");
            assert!(matches!(err, RuntimeError::InvalidInput(_)), "{err:?}");
        }
    }
}
