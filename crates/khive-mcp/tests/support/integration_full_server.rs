use super::*;

pub(super) fn make_full_server() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        actor_id: Some("brain-feedback-test".to_string()),
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec![
            "kg".to_string(),
            "gtd".to_string(),
            "memory".to_string(),
            "brain".to_string(),
            "session".to_string(),
        ],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime with all packs");
    let runtime = receipt_credentials::with_receipt_credentials(runtime);
    KhiveMcpServer::new(runtime).expect("server builds with kg+gtd+memory+brain+session")
}
