use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};
use std::sync::Arc;

fn registry(backend: Arc<khive_db::StorageBackend>, actor: &str) -> VerbRegistry {
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: None,
            actor_id: Some(actor.into()),
            ..RuntimeConfig::no_embeddings()
        },
    );
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime));
    builder.build().unwrap()
}

#[tokio::test]
async fn inbox_serde_refusals_mask_credentials_and_preserve_messages() {
    let backend = khive_db::StorageBackend::memory().unwrap();
    backend.pool().run_migrations().unwrap();
    let backend = Arc::new(backend);
    let sender = registry(Arc::clone(&backend), "actor:sender");
    let recipient = registry(backend, "actor:recipient");
    sender
        .dispatch(
            "comm.send",
            json!({"to":"actor:recipient", "content":"serde refusal control"}),
        )
        .await
        .unwrap();
    let before = recipient
        .dispatch("comm.inbox", json!({"status":"all", "limit":20}))
        .await
        .unwrap();
    assert_eq!(before["count"], 1);
    let secret = format!("sk-proj-{}", "A".repeat(80));
    for raw in ["bogus", secret.as_str()] {
        for unknown_field in [false, true] {
            let params = if unknown_field {
                let mut params = json!({});
                params
                    .as_object_mut()
                    .unwrap()
                    .insert(raw.to_string(), Value::Bool(true));
                params
            } else {
                json!({"limit":raw})
            };
            let error = recipient.dispatch("comm.inbox", params).await.unwrap_err();
            let RuntimeError::InvalidInput(message) = &error else {
                panic!("wrong refusal kind");
            };
            assert!(
                message.starts_with("bad params: "),
                "actual decoder boundary must run"
            );
            let shown = if raw == "bogus" { raw } else { "***MASKED***" };
            if raw != "bogus" {
                assert!(
                    !message.contains(raw),
                    "public inbox exposed a recognized credential"
                );
                assert!(message.contains(shown));
            }
            if unknown_field {
                assert!(message.contains(&format!("unknown field `{shown}`")));
            } else {
                assert_eq!(
                    message,
                    &format!("bad params: invalid type: string \"{shown}\", expected u32")
                );
            }
            let expected_projection = json!({"kind":"runtime_error", "message":format!("invalid input: {message}"),
                "domain_disposition":"not_committed"});
            assert_eq!(
                runtime_error_value(error, DomainDisposition::NotCommitted),
                expected_projection
            );
            let after = recipient
                .dispatch("comm.inbox", json!({"status":"all", "limit":20}))
                .await
                .unwrap();
            assert_eq!(
                after, before,
                "parameter refusal must not change the populated inbox"
            );
        }
    }
}
