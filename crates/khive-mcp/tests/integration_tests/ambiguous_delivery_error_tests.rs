// ── comm.send ambiguous-delivery structured error ────────────────────────────

/// A minimal mock pack whose single verb always fails the same way
/// `comm.send`/`comm.reply` do when the dual-write's writer-task reply is
/// lost after the request was accepted (`side_effects_unknown`): a
/// `RuntimeError::Khive(KhiveError)` with `kind=conflict` and a
/// `details.outbound_id` field carrying a freshly generated, non-`'static`
/// UUID string (via `Details::new_owned`). Verifies that this dynamic detail
/// value — not just static string-literal pairs, as `ErrorInjectPack` above
/// covers — survives the MCP per-op boundary as a structured JSON field
/// instead of being flattened into the error's prose `message`.
struct AmbiguousDeliveryPack;

impl khive_types::Pack for AmbiguousDeliveryPack {
    const NAME: &'static str = "ambiguous-delivery-inject";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "always_ambiguous",
        description: "always returns the comm dual-write ambiguous-delivery error shape",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for AmbiguousDeliveryPack {
    fn name(&self) -> &str {
        "ambiguous-delivery-inject"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        AmbiguousDeliveryPack::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        _params: serde_json::Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<serde_json::Value, RuntimeError> {
        let outbound_id = uuid::Uuid::new_v4();
        let err = KhiveError::conflict(format!(
            "dual_write delivery outcome is uncertain (side_effects_unknown); \
             call comm.delivered(id=\"{outbound_id}\") before retrying"
        ))
        .with_details(Details::new_owned([(
            "outbound_id",
            outbound_id.to_string(),
        )]));
        Err(RuntimeError::Khive(err))
    }
}

fn make_ambiguous_delivery_server() -> KhiveMcpServer {
    disable_daemon();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AmbiguousDeliveryPack);
    let registry = builder.build().expect("ambiguous-delivery registry builds");
    KhiveMcpServer::from_registry(registry)
}

async fn connect_ambiguous_delivery(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_ambiguous_delivery_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// The ambiguous-delivery error's `outbound_id` must be readable as a
/// structured `error.details.outbound_id` string field through the MCP
/// per-op boundary — not only recoverable by regexing `error.message` prose.
#[tokio::test]
async fn comm_ambiguous_delivery_error_carries_structured_outbound_id() -> anyhow::Result<()> {
    let client = connect_ambiguous_delivery().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "always_ambiguous()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];

    assert_eq!(first["ok"], false, "expected op failure: {first}");

    let error = &first["error"];
    assert!(
        error.is_object(),
        "ambiguous-delivery error must be a JSON object, not a string; got: {error}"
    );
    assert_eq!(
        error["kind"].as_str(),
        Some("conflict"),
        "ambiguous-delivery error must classify as kind='conflict'; got: {error}"
    );

    let outbound_id = error["details"]["outbound_id"]
        .as_str()
        .expect("error.details.outbound_id must be a structured string field");
    assert!(
        uuid::Uuid::parse_str(outbound_id).is_ok(),
        "error.details.outbound_id must be a valid UUID string; got: {outbound_id:?}"
    );

    Ok(())
}
