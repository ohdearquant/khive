const EXTERNAL_THREAD: &str = "3b599414-cce0-4a8b-9c1e-2bb97108ea81";

fn external_raw_fixture(name: &str) -> &'static [u8] {
    match name {
        "owner-precedence" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/owner-precedence.eml.rs");
            }
            recorded::MIME
        }
        "alias-precedence" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/alias-precedence.eml.rs");
            }
            recorded::MIME
        }
        "benign" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/benign.eml.rs");
            }
            recorded::MIME
        }
        "reply-opt-out" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/reply-opt-out.eml.rs");
            }
            recorded::MIME
        }
        "plain-quoted-opt-out" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/plain-quoted-opt-out.eml.rs");
            }
            recorded::MIME
        }
        "action-withheld" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/action-withheld.eml.rs");
            }
            recorded::MIME
        }
        "agent-withheld" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/agent-withheld.eml.rs");
            }
            recorded::MIME
        }
        "hidden-cf-withheld" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/hidden-cf-withheld.eml.rs");
            }
            recorded::MIME
        }
        "two-family-screened" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/two-family-screened.eml.rs");
            }
            recorded::MIME
        }
        "html-opt-out" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/html-opt-out.eml.rs");
            }
            recorded::MIME
        }
        "html-benign" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/html-benign.eml.rs");
            }
            recorded::MIME
        }
        "html-blockquote-opt-out" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/html-blockquote-opt-out.eml.rs");
            }
            recorded::MIME
        }
        "auth-failed-opt-out" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/auth-failed-opt-out.eml.rs");
            }
            recorded::MIME
        }
        "maintainer-unscreened" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/maintainer-unscreened.eml.rs");
            }
            recorded::MIME
        }
        "maintainer-invalid-sender" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/maintainer-invalid-sender.eml.rs");
            }
            recorded::MIME
        }
        "hidden-html-withheld" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/hidden-html-withheld.eml.rs");
            }
            recorded::MIME
        }
        "executable-screened" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/executable-screened.eml.rs");
            }
            recorded::MIME
        }
        "ordinary-attachment" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/ordinary-attachment.eml.rs");
            }
            recorded::MIME
        }
        "agent-role" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/agent-role.eml.rs");
            }
            recorded::MIME
        }
        "agent-reveal" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/agent-reveal.eml.rs");
            }
            recorded::MIME
        }
        "action-pipe" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/action-pipe.eml.rs");
            }
            recorded::MIME
        }
        "action-install" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/action-install.eml.rs");
            }
            recorded::MIME
        }
        "action-credential" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/action-credential.eml.rs");
            }
            recorded::MIME
        }
        "display-none-positive" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/display-none-positive.eml.rs");
            }
            recorded::MIME
        }
        "display-none-negative" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/display-none-negative.eml.rs");
            }
            recorded::MIME
        }
        "visibility-hidden-positive" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/visibility-hidden-positive.eml.rs");
            }
            recorded::MIME
        }
        "visibility-hidden-negative" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/visibility-hidden-negative.eml.rs");
            }
            recorded::MIME
        }
        "zero-font-positive" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/zero-font-positive.eml.rs");
            }
            recorded::MIME
        }
        "zero-font-negative" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/zero-font-negative.eml.rs");
            }
            recorded::MIME
        }
        "same-colour-positive" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/same-colour-positive.eml.rs");
            }
            recorded::MIME
        }
        "same-colour-negative" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/same-colour-negative.eml.rs");
            }
            recorded::MIME
        }
        _ => panic!("unknown recorded raw fixture {name}"),
    }
}

#[derive(Default)]
struct RoutingHost {
    enabled: bool,
    blocked: bool,
    fail_lookup: bool,
    owners: HashMap<String, crate::ThreadOwner>,
    looked_up: Mutex<Vec<String>>,
}

#[async_trait]
impl ExternalEmailHost for RoutingHost {
    fn external_senders_configured(&self) -> bool {
        self.enabled
    }
    fn verify_unsubscribe_token(&self, _: &str) -> Option<String> {
        None
    }
    async fn inbound_suppression(&self) -> Result<InboundSuppression, ChannelError> {
        let blocked = self.blocked;
        Ok(Arc::new(move |_| blocked))
    }
    async fn lookup_thread_owner(
        &self,
        candidate: &str,
    ) -> Result<Option<crate::ThreadOwner>, ChannelError> {
        self.looked_up.lock().unwrap().push(candidate.to_string());
        if self.fail_lookup {
            return Err(ChannelError::Transport("PRIVATE_LOOKUP_FAILURE".into()));
        }
        Ok(self.owners.get(candidate).cloned())
    }
    async fn record_mailto_unsubscribe(&self, _: &str) -> Result<bool, ChannelError> {
        Ok(false)
    }
}

struct RawFixtureImap {
    raw: &'static [u8],
    uid: u32,
}

#[async_trait]
impl ImapConnector for RawFixtureImap {
    async fn fetch_page(
        &self,
        _: DateTime<Utc>,
        _: usize,
        _: ImapProgress,
    ) -> Result<ImapFetchPage, ChannelError> {
        let email = parse_raw_bytes(self.uid, self.raw, "imap.example.com", 7).unwrap();
        Ok(ImapFetchPage {
            emails: vec![SelectedMessage::Email(Box::new(email))],
            next_progress: ImapProgress {
                uid_validity: NonZeroU32::new(7),
                last_seen_uid: NonZeroU32::new(self.uid),
            },
        })
    }
}

fn external_raw_channel(name: &str, host: Arc<RoutingHost>, uid: u32) -> EmailChannel {
    let smtp = SmtpSender::with_connector(RecordingSmtp {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let imap = ImapFetcher::with_connector(RawFixtureImap {
        raw: external_raw_fixture(name),
        uid,
    });
    EmailChannel::with_connectors(make_config("maintainer@example.com"), smtp, imap)
        .with_external_host(host)
        .with_external_inbound_config(ExternalInboundConfig {
            routes: crate::InboundRoutes::parse("first@example.com=lambda:first,second@example.com=lambda:second,cc@example.com=lambda:cc").unwrap(),
            actor: "lambda:recipient".into(),
        })
}

async fn external_raw_page(name: &str, host: Arc<RoutingHost>) -> ChannelPollPage {
    external_raw_channel(name, host, 42)
        .poll_page(Utc::now(), None)
        .await
        .unwrap()
}

#[tokio::test]
async fn a13_raw_poll_verifies_candidate_order_then_routes_alias_or_fallback() {
    let host = Arc::new(RoutingHost {
        enabled: true,
        owners: HashMap::from([
            (
                "owned-first@khive.ai".into(),
                crate::ThreadOwner {
                    actor: "lambda:owner".into(),
                    thread_id: EXTERNAL_THREAD.into(),
                },
            ),
            (
                "owned-last@khive.ai".into(),
                crate::ThreadOwner {
                    actor: "lambda:later".into(),
                    thread_id: uuid::Uuid::new_v4().to_string(),
                },
            ),
        ]),
        ..Default::default()
    });
    let page = external_raw_page("owner-precedence", Arc::clone(&host)).await;
    let env = &page.envelopes[0];
    let delivery = env.inbound_delivery.as_ref().unwrap();
    assert_eq!(delivery.actor, "lambda:owner");
    assert_eq!(delivery.thread_id.unwrap().to_string(), EXTERNAL_THREAD);
    assert_eq!(
        *host.looked_up.lock().unwrap(),
        [
            "missing-x",
            "missing-parent@khive.ai",
            "owned-first@khive.ai"
        ]
    );
    assert_eq!(env.correlation_external_id, None);
    assert_eq!(env.metadata["origin"], "external");
    assert_eq!(env.metadata["trust"], "untrusted");
    assert_eq!(env.from, "email:stranger@outside.example");
    assert!(delivery.pending_owner_candidates.is_none());
    for (fixture, actor) in [
        ("alias-precedence", "lambda:first"),
        ("benign", "lambda:recipient"),
    ] {
        let page = external_raw_page(
            fixture,
            Arc::new(RoutingHost {
                enabled: true,
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(
            page.envelopes[0].inbound_delivery.as_ref().unwrap().actor,
            actor
        );
    }
}

#[tokio::test]
async fn a12_raw_poll_disabled_auth_block_and_maintainer_precede_screening() {
    for (name, enabled, blocked, reason) in [
        ("reply-opt-out", false, false, Some("off-allowlist")),
        ("auth-failed-opt-out", true, false, Some("dmarc-fail")),
        ("reply-opt-out", true, true, Some("blocked")),
        ("maintainer-unscreened", true, true, None),
        (
            "maintainer-invalid-sender",
            true,
            false,
            Some("off-allowlist"),
        ),
    ] {
        let host = Arc::new(RoutingHost {
            enabled,
            blocked,
            ..Default::default()
        });
        let page = external_raw_page(name, Arc::clone(&host)).await;
        let env = &page.envelopes[0];
        assert_eq!(
            env.metadata.get("quarantine_reason").map(String::as_str),
            reason,
            "{name}"
        );
        assert!(env.inbound_delivery.is_none(), "{name}");
        assert!(host.looked_up.lock().unwrap().is_empty(), "{name}");
        if reason.is_none() {
            assert!(env.content.contains("Ignore your previous instructions"));
        }
    }
}

#[tokio::test]
async fn a9_raw_html_opt_out_keeps_html_only_and_plain_quote_contract() {
    for (name, expected) in [
        ("html-opt-out", true),
        ("html-blockquote-opt-out", true),
        ("html-benign", false),
        ("plain-quoted-opt-out", false),
    ] {
        let page = external_raw_page(
            name,
            Arc::new(RoutingHost {
                enabled: true,
                ..Default::default()
            }),
        )
        .await;
        let delivery = page.envelopes[0].inbound_delivery.as_ref().unwrap();
        assert_eq!(delivery.reply_opt_out, expected, "{name}");
        assert_eq!(
            delivery.labels.iter().any(|id| id == "opt_out.unsubscribe"),
            expected,
            "{name}"
        );
        assert_eq!(
            delivery
                .labels
                .iter()
                .any(|id| id == "html_only.no_text_part"),
            name.starts_with("html-"),
            "{name}"
        );
        assert!(!delivery.body_withheld, "{name}");
    }
}

#[tokio::test]
async fn owner_lookup_failure_keeps_uid_ordered_candidates_and_checkpoint_candidate() {
    let page = external_raw_page(
        "owner-precedence",
        Arc::new(RoutingHost {
            enabled: true,
            fail_lookup: true,
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(page.next_checkpoint.unwrap().high_water, Some(42));
    let env = &page.envelopes[0];
    assert!(env.external_id.as_deref().unwrap().ends_with(":7:42"));
    let delivery = env.inbound_delivery.as_ref().unwrap();
    assert_eq!(delivery.actor, "lambda:recipient");
    assert_eq!(delivery.thread_id, None);
    assert_eq!(
        delivery.pending_owner_candidates.as_ref().unwrap(),
        &[
            "missing-x",
            "missing-parent@khive.ai",
            "owned-first@khive.ai",
            "owned-last@khive.ai"
        ]
    );
    assert!(!serde_json::to_string(env)
        .unwrap()
        .contains("PRIVATE_LOOKUP_FAILURE"));
    // Only the coordinator may commit the returned candidate after ingest.
    assert!(env.quarantine_replay.is_some());
}

#[derive(Debug, Default)]
struct ReplayFixtureBlobs(Mutex<HashMap<khive_storage::ContentRef, Vec<u8>>>);

#[async_trait]
impl khive_storage::BlobStore for ReplayFixtureBlobs {
    async fn put(&self, bytes: Vec<u8>) -> khive_storage::StorageResult<khive_storage::ContentRef> {
        let reference = khive_storage::ContentRef::from_digest_bytes(
            khive_types::Hash32::from_blake3(&bytes).as_bytes(),
        );
        self.0.lock().unwrap().insert(reference.clone(), bytes);
        Ok(reference)
    }
    async fn get_bounded_verified(
        &self,
        reference: &khive_storage::ContentRef,
        max_bytes: u64,
    ) -> khive_storage::StorageResult<Vec<u8>> {
        let bytes = self
            .0
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .ok_or_else(|| khive_storage::StorageError::Internal("fixture blob missing".into()))?;
        assert_eq!(
            reference,
            &khive_storage::ContentRef::from_digest_bytes(
                khive_types::Hash32::from_blake3(&bytes).as_bytes()
            )
        );
        if bytes.len() as u64 > max_bytes {
            return Err(khive_storage::StorageError::BlobTooLarge {
                content_ref: reference.clone(),
                max_bytes,
                observed_at_least: bytes.len() as u64,
            });
        }
        Ok(bytes)
    }
    async fn exists(
        &self,
        reference: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<bool> {
        Ok(self.0.lock().unwrap().contains_key(reference))
    }
    async fn size(
        &self,
        reference: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<Option<u64>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(reference)
            .map(|bytes| bytes.len() as u64))
    }
    async fn delete(
        &self,
        reference: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<bool> {
        Ok(self.0.lock().unwrap().remove(reference).is_some())
    }
}

fn external_ingest_registry() -> (khive_runtime::VerbRegistry, KhiveRuntime) {
    use khive_pack_blob as _;
    use khive_pack_comm as _;
    use khive_pack_kg as _;
    let runtime = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: None,
        actor_id: Some("lambda:recipient".into()),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .unwrap();
    runtime
        .install_blob_store(Arc::new(ReplayFixtureBlobs::default()))
        .unwrap();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    khive_runtime::PackRegistry::register_packs(
        &["kg".into(), "comm".into(), "blob".into()],
        runtime.clone(),
        &mut builder,
    )
    .unwrap();
    builder.with_default_namespace("local");
    builder.with_actor_id(Some("lambda:recipient".into()));
    (builder.build().unwrap(), runtime)
}

async fn emitted_external_ingest_params(
    env: &ChannelEnvelope,
    registry: &khive_runtime::VerbRegistry,
) -> serde_json::Value {
    use base64::Engine as _;
    let mut metadata: serde_json::Map<String, serde_json::Value> = env
        .metadata
        .iter()
        .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
        .collect();
    let delivery = env.inbound_delivery.as_ref();
    if let Some(labels) = env.metadata.get("labels") {
        metadata.insert(
            "labels".into(),
            serde_json::json!(serde_json::from_str::<Vec<String>>(labels).unwrap()),
        );
    }
    if let Some(delivery) = delivery {
        metadata.insert("labels".into(), serde_json::json!(delivery.labels));
        metadata.insert(
            "screening_body_withheld".into(),
            serde_json::json!(delivery.body_withheld),
        );
    }
    let withheld = delivery.is_some_and(|delivery| delivery.body_withheld);
    if withheld || env.metadata.get("quarantined").map(String::as_str) == Some("true") {
        let raw = env.quarantine_replay.as_ref().unwrap();
        let result = registry
            .dispatch(
                "blob.put",
                serde_json::json!({
                    "bytes": base64::engine::general_purpose::STANDARD.encode(&raw.bytes),
                }),
            )
            .await
            .unwrap();
        metadata.insert(
            "quarantine_content_ref".into(),
            result["content_ref"].clone(),
        );
    }
    serde_json::json!({
        "namespace": "local",
        "from": env.from, "to": env.to, "content": env.content, "subject": env.subject,
        "external_id": env.external_id, "legacy_external_id": env.legacy_external_id,
        "channel_kind": "email", "channel_slug": "user@example.com",
        "sent_at": env.sent_at.as_ref().map(|ts| ts.to_rfc3339()),
        "correlation_external_id": if delivery.is_some() { None } else { env.correlation_external_id.as_deref() },
        "default_inbound_actor": delivery.map(|delivery| delivery.actor.as_str()).unwrap_or("lambda:recipient"),
        "thread_id": delivery.and_then(|delivery| delivery.thread_id).map(|id| id.to_string()),
        "pending_owner_candidates": delivery.and_then(|delivery| delivery.pending_owner_candidates.as_ref()),
        "reply_opt_out": delivery.is_some_and(|delivery| delivery.reply_opt_out),
        "wire_message_id": env.wire_message_id,
        "wire_references": env.wire_references,
        "metadata": metadata,
    })
}

#[tokio::test]
async fn a15_fourteen_recorded_rule_pairs_poll_then_real_comm_sinks() {
    let pairs = [
        ("hidden_text.cf_character", "hidden-cf-withheld", "benign"),
        (
            "hidden_text.html_display_none",
            "display-none-positive",
            "display-none-negative",
        ),
        (
            "hidden_text.html_visibility_hidden",
            "visibility-hidden-positive",
            "visibility-hidden-negative",
        ),
        (
            "hidden_text.html_zero_font",
            "zero-font-positive",
            "zero-font-negative",
        ),
        (
            "hidden_text.html_same_colour",
            "same-colour-positive",
            "same-colour-negative",
        ),
        ("agent_addressed.override", "agent-withheld", "benign"),
        ("agent_addressed.role_claim", "agent-role", "benign"),
        ("agent_addressed.reveal_or_change", "agent-reveal", "benign"),
        ("action_request.pipe_to_shell", "action-pipe", "benign"),
        (
            "action_request.install_or_execute",
            "action-install",
            "benign",
        ),
        ("action_request.credential", "action-credential", "benign"),
        (
            "executable_attachment.declared_or_sniffed",
            "executable-screened",
            "ordinary-attachment",
        ),
        ("html_only.no_text_part", "html-benign", "benign"),
        (
            "opt_out.unsubscribe",
            "reply-opt-out",
            "plain-quoted-opt-out",
        ),
    ];
    for (rule, positive, negative) in pairs {
        for (name, expected) in [(positive, true), (negative, false)] {
            let page = external_raw_page(
                name,
                Arc::new(RoutingHost {
                    enabled: true,
                    ..Default::default()
                }),
            )
            .await;
            let env = &page.envelopes[0];
            let (registry, runtime) = external_ingest_registry();
            let params = emitted_external_ingest_params(env, &registry).await;
            assert_eq!(
                params["metadata"]["labels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|label| label == rule),
                expected,
                "{rule}: {name}"
            );
            let screened = expected && rule.starts_with("executable_attachment.");
            assert_eq!(
                env.metadata.get("quarantine_reason").map(String::as_str),
                screened.then_some("screened"),
                "{rule}: {name} disposition"
            );
            if !screened {
                let delivery = env.inbound_delivery.as_ref().unwrap();
                let withholds = rule.starts_with("hidden_text.")
                    || rule.starts_with("agent_addressed.")
                    || rule.starts_with("action_request.");
                assert_eq!(
                    delivery.body_withheld,
                    expected && withholds,
                    "{rule}: {name}"
                );
            }
            let first = registry
                .dispatch("comm.ingest", params.clone())
                .await
                .unwrap();
            let duplicate = registry.dispatch("comm.ingest", params).await.unwrap();
            assert_eq!(duplicate["deduplicated"], true, "{rule}: {name}");
            assert_eq!(first["thread_id"], duplicate["thread_id"]);
            if let Some(delivery) = &env.inbound_delivery {
                for (verb, args) in [
                    ("comm.inbox", serde_json::json!({})),
                    ("comm.thread", serde_json::json!({"id": first["full_id"]})),
                    ("comm.read", serde_json::json!({"id": first["full_id"]})),
                ] {
                    let read = registry.dispatch(verb, args).await.unwrap();
                    let text = read.to_string();
                    assert!(
                        text.contains("\"origin\":\"external\""),
                        "{verb}: origin lost"
                    );
                    assert!(
                        text.contains("\"trust\":\"untrusted\""),
                        "{verb}: trust lost"
                    );
                    assert!(
                        text.contains("email:stranger@outside.example"),
                        "{verb}: sender lost"
                    );
                    assert!(
                        text.contains(rule) || !expected,
                        "{verb}: {rule} labels lost"
                    );
                    if delivery.body_withheld {
                        assert!(
                            !text.contains("HIDDEN_PAIR_TEXT"),
                            "{verb}: raw hidden body leaked"
                        );
                    }
                }
            }
            if env
                .inbound_delivery
                .as_ref()
                .is_some_and(|delivery| delivery.reply_opt_out)
            {
                let entries = khive_pack_comm::handlers::load_outbound_email_suppressions(&runtime)
                    .await
                    .unwrap();
                assert!(entries.iter().any(|entry| {
                    entry.kind == khive_runtime::EmailSuppressionKind::Address
                        && entry.value == "stranger@outside.example"
                }));
            }
        }
    }
}

#[tokio::test]
async fn a14_raw_external_marking_survives_real_comm_read_surfaces() {
    let page = external_raw_page(
        "benign",
        Arc::new(RoutingHost {
            enabled: true,
            ..Default::default()
        }),
    )
    .await;
    let env = &page.envelopes[0];
    assert_eq!(env.from, "email:stranger@outside.example");
    assert_ne!(env.from, "email:maintainer@example.com");
    let (registry, _) = external_ingest_registry();
    let params = emitted_external_ingest_params(env, &registry).await;
    let ack = registry.dispatch("comm.ingest", params).await.unwrap();
    for (verb, args) in [
        ("comm.inbox", serde_json::json!({})),
        ("comm.thread", serde_json::json!({"id": ack["full_id"]})),
        ("comm.read", serde_json::json!({"id": ack["full_id"]})),
    ] {
        let text = registry.dispatch(verb, args).await.unwrap().to_string();
        assert!(text.contains("\"origin\":\"external\""), "{verb}");
        assert!(text.contains("\"trust\":\"untrusted\""), "{verb}");
        assert!(text.contains("\"labels\":[]"), "{verb}");
        assert!(text.contains("email:stranger@outside.example"), "{verb}");
        assert!(!text.contains("email:maintainer@example.com"), "{verb}");
    }
}

#[tokio::test]
async fn a15_withheld_original_operator_blob_read_is_byte_exact_and_comm_reads_are_body_free() {
    use base64::Engine as _;
    let page = external_raw_page(
        "action-withheld",
        Arc::new(RoutingHost {
            enabled: true,
            ..Default::default()
        }),
    )
    .await;
    let env = &page.envelopes[0];
    assert!(!serde_json::to_string(env)
        .unwrap()
        .contains("ADR200_WITHHELD_ORIGINAL_SECRET_7f42"));
    let (registry, runtime) = external_ingest_registry();
    let params = emitted_external_ingest_params(env, &registry).await;
    let reference = params["metadata"]["quarantine_content_ref"].clone();
    let ack = registry.dispatch("comm.ingest", params).await.unwrap();
    for (verb, args) in [
        ("comm.inbox", serde_json::json!({})),
        ("comm.thread", serde_json::json!({"id":ack["full_id"]})),
        ("comm.read", serde_json::json!({"id":ack["full_id"]})),
    ] {
        let result = registry.dispatch(verb, args).await.unwrap();
        assert!(
            !result
                .to_string()
                .contains("ADR200_WITHHELD_ORIGINAL_SECRET_7f42"),
            "{verb}"
        );
    }
    let result = registry
        .dispatch("blob.get", serde_json::json!({"content_ref":reference}))
        .await
        .unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(result["bytes"].as_str().unwrap())
            .unwrap(),
        external_raw_fixture("action-withheld")
    );
    let id = ack["full_id"].as_str().unwrap().parse().unwrap();
    let owner = runtime
        .core()
        .attachments()
        .unwrap()
        .get_attachment(id, "quarantine-original")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.content_ref.as_str(), reference.as_str().unwrap());
}

#[tokio::test]
async fn a15_raw_two_withholding_families_quarantine_as_screened() {
    let page = external_raw_page(
        "two-family-screened",
        Arc::new(RoutingHost {
            enabled: true,
            ..Default::default()
        }),
    )
    .await;
    let env = &page.envelopes[0];
    assert!(env.inbound_delivery.is_none());
    assert_eq!(env.metadata["quarantine_reason"], "screened");
    assert!(!env.content.contains("ADR200_QUARANTINE_BODY"));
    let labels: Vec<String> = serde_json::from_str(&env.metadata["labels"]).unwrap();
    assert_eq!(
        labels,
        [
            "agent_addressed.override",
            "action_request.install_or_execute"
        ]
    );
    let (registry, _) = external_ingest_registry();
    let params = emitted_external_ingest_params(env, &registry).await;
    let first = registry
        .dispatch("comm.ingest", params.clone())
        .await
        .unwrap();
    let duplicate = registry.dispatch("comm.ingest", params).await.unwrap();
    assert_eq!(duplicate["deduplicated"], true);
    assert_eq!(first["thread_id"], duplicate["thread_id"]);
}
