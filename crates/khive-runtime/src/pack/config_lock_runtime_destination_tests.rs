/// A real registered handler, reached only after the registry's pre-handler
/// config drain. It observes on its actual retained A runtime.
struct ObserveConfigPack(KhiveRuntime);

impl Pack for ObserveConfigPack {
    const NAME: &'static str = "config_observer";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "observe_config",
        description: "observe a test config value during the handler",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for ObserveConfigPack {
    fn name(&self) -> &str {
        ObserveConfigPack::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        ObserveConfigPack::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        ObserveConfigPack::ENTITY_KINDS
    }
    fn brain_consumer_kinds(&self) -> &'static [&'static str] {
        ObserveConfigPack::BRAIN_CONSUMER_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        ObserveConfigPack::HANDLERS
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.0
            .record_config_locked("destination.from_handler", || "resolved-in-a".into());
        Ok(serde_json::json!({"observed": true}))
    }
}

#[tokio::test]
#[serial(config_ledger)]
async fn handler_observation_after_a_drain_stays_pending_while_b_dispatches() {
    let runtime_a = KhiveRuntime::memory().unwrap();
    let handler_runtime = runtime_a.clone();
    let a = Fixture::configured(
        runtime_a,
        "handler-owner-a",
        "handler-a",
        audit_config(),
        move |builder| {
            builder.register(ObserveConfigPack(handler_runtime));
        },
    );
    let b = Fixture::from_runtime(
        KhiveRuntime::memory().unwrap(),
        "handler-owner-b",
        "handler-b",
    );
    let token = NamespaceToken::mint_authorized(
        khive_types::Namespace::parse("handler-owner-a").unwrap(),
        ActorRef::new("agent", "handler-a"),
    );
    let result = a
        .registry
        .dispatch_with_identity(
            "observe_config",
            serde_json::json!({"namespace": "handler-owner-a"}),
            Some(RequestIdentity::from_token(&token)),
        )
        .await
        .expect("actual A handler succeeds");
    assert_eq!(result, serde_json::json!({"observed": true}));
    assert!(
        a.rows().await.is_empty(),
        "A observed after its first pre-handler drain"
    );
    b.dispatch().await;
    assert!(
        b.rows().await.is_empty(),
        "B must not persist A's handler-time observation"
    );
    a.dispatch().await;
    a.assert_rows(&[("destination.from_handler", "resolved-in-a")])
        .await;
    let rows = a.rows().await;
    assert_eq!(rows[0].namespace, "handler-owner-a");
    assert_eq!(rows[0].actor, "agent:handler-a");
    assert!(
        b.rows().await.is_empty(),
        "B stays empty after A's later drain"
    );
    a.finish().await;
    b.finish().await;
}
