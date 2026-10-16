struct Issue558Probe {
    fail: bool,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl khive_runtime::PackByIdResolver for Issue558Probe {
    async fn resolve_by_id(
        &self,
        _id: uuid::Uuid,
    ) -> Result<Option<khive_runtime::Resolved>, RuntimeError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail {
            Err(RuntimeError::Internal("issue-558 resolver failure".into()))
        } else {
            Ok(None)
        }
    }

    async fn delete_by_id(&self, _id: uuid::Uuid, _hard: bool) -> Result<Value, RuntimeError> {
        panic!("diagnostic resolver must never delete")
    }
}

#[tokio::test]
async fn issue_558_merge_resolver_error_propagates() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for fail in [true, false] {
        for note in [false, true] {
            let runtime = rt();
            let real = pack_via_registry(runtime.clone());
            let atom = issue_558_private(&runtime, &real, "atom").await;
            let domain = issue_558_private(&runtime, &real, "domain").await;
            let kind = if note { "observation" } else { "concept" };
            let ordinary = issue_558_ordinary(&real, kind, "into").await;
            let other = issue_558_ordinary(&real, kind, "from").await;
            let anchor = issue_558_ordinary(&real, "concept", "anchor").await;
            issue_558_edge(&real, &ordinary, &anchor, note).await;
            let calls = Arc::new(AtomicUsize::new(0));
            let later = Arc::new(AtomicUsize::new(0));
            let mut builder = VerbRegistryBuilder::new();
            builder.register_resolver(
                "issue-558-first",
                Box::new(Issue558Probe {
                    fail,
                    calls: calls.clone(),
                }),
            );
            builder.register_resolver(
                "issue-558-later",
                Box::new(Issue558Probe {
                    fail: false,
                    calls: later.clone(),
                }),
            );
            PackRegistry::register_packs(
                &["kg".into(), "knowledge".into()],
                runtime.clone(),
                &mut builder,
            )
            .expect("real resolver factory");
            let f = Fixture {
                registry: builder.build().expect("probe registry"),
            };
            for private in [&atom, &domain] {
                for private_into in [true, false] {
                    calls.store(0, Ordering::SeqCst);
                    later.store(0, Ordering::SeqCst);
                    let (into, from) = if private_into {
                        (private, &ordinary)
                    } else {
                        (&ordinary, private)
                    };
                    let arm = format!("fail={fail}/note={note}/{private}/into={private_into}");
                    let error = issue_558_unchanged_error(
                        &runtime,
                        &f,
                        issue_558_args(into, from, Some(kind)),
                        &arm,
                    )
                    .await;
                    if fail {
                        assert!(
                            matches!(error, RuntimeError::Internal(ref message) if message == "issue-558 resolver failure"),
                            "{arm}: {error:?}"
                        );
                    } else {
                        issue_558_directing_error(error, &arm);
                    }
                    assert_eq!(calls.load(Ordering::SeqCst), 1, "{arm}");
                    assert_eq!(
                        later.load(Ordering::SeqCst),
                        usize::from(!fail),
                        "{arm}: resolver error must short-circuit"
                    );
                }
            }
            calls.store(0, Ordering::SeqCst);
            later.store(0, Ordering::SeqCst);
            let before = issue_558_snapshot(&runtime).await;
            let mut args = issue_558_args(&ordinary, &other, Some(kind));
            args["dry_run"] = json!(true);
            f.dispatch("merge", args)
                .await
                .expect("successful substrate reads do not probe");
            assert_eq!(issue_558_snapshot(&runtime).await, before);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(later.load(Ordering::SeqCst), 0);
        }
    }
}

/// A resolver that claims every id for a pack other than knowledge.
struct OtherPackProbe {
    verbs: &'static [&'static str],
}

#[async_trait::async_trait]
impl khive_runtime::PackByIdResolver for OtherPackProbe {
    async fn resolve_by_id(
        &self,
        _id: uuid::Uuid,
    ) -> Result<Option<khive_runtime::Resolved>, RuntimeError> {
        Ok(Some(khive_runtime::Resolved::PackRecord {
            pack: "probe-pack".into(),
            kind: "probe".into(),
            data: json!({}),
        }))
    }

    async fn delete_by_id(&self, _id: uuid::Uuid, _hard: bool) -> Result<Value, RuntimeError> {
        panic!("diagnostic resolver must never delete")
    }

    fn private_record_verbs(&self) -> &'static [&'static str] {
        self.verbs
    }
}

/// The refusal for a generic verb on a pack-private record names the pack
/// whose resolver claimed the record, and offers only that pack's verbs.
#[tokio::test]
async fn private_record_refusal_names_the_pack_that_claimed_the_record() {
    let cases: [(&'static [&'static str], &str); 2] = [
        (&[], ""),
        (&["probe-pack.revise"], " (e.g. probe-pack.revise)"),
    ];
    for (verbs, examples) in cases {
        let runtime = rt();
        let real = pack_via_registry(runtime.clone());
        let ordinary = issue_558_ordinary(&real, "concept", "from").await;
        let mut builder = VerbRegistryBuilder::new();
        builder.register_resolver("probe-pack", Box::new(OtherPackProbe { verbs }));
        PackRegistry::register_packs(
            &["kg".into(), "knowledge".into()],
            runtime.clone(),
            &mut builder,
        )
        .expect("real resolver factory");
        let f = Fixture {
            registry: builder.build().expect("probe registry"),
        };
        let claimed = uuid::Uuid::new_v4().to_string();

        let merge = issue_558_unchanged_error(
            &runtime,
            &f,
            issue_558_args(&claimed, &ordinary, Some("concept")),
            "merge",
        )
        .await;
        let before = issue_558_snapshot(&runtime).await;
        let update = f
            .dispatch("update", json!({"id": claimed, "name": "New Name"}))
            .await
            .expect_err("update of a claimed record must be refused");
        assert_eq!(issue_558_snapshot(&runtime).await, before);

        for (verb, error) in [("merge", merge), ("update", update)] {
            let RuntimeError::InvalidInput(message) = error else {
                panic!("{verb}: expected InvalidInput, got {error:?}");
            };
            let expected = format!("use the probe-pack pack's own verbs{examples}");
            assert!(
                message.starts_with(verb) && message.ends_with(&expected),
                "{verb}: {message:?} does not end with {expected:?}"
            );
            assert!(
                !message.contains("knowledge"),
                "{verb}: directed to another pack's verbs: {message:?}"
            );
        }
    }
}
