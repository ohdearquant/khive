use crate::CommPack;
use khive_runtime::pack::AuditSuccessRowsMode;
use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .expect("canonical result object")
        .keys()
        .map(String::as_str)
        .collect()
}
#[tokio::test]
async fn poll_handler_result_shapes_require_sampling_review() {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_audit_success_rows_mode(AuditSuccessRowsMode::All);
    builder.register_trusted(CommPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    for outcome in ["success", "failure"] {
        let value=registry.dispatch("comm.heartbeat",json!({"channel_kind":"email","channel_slug":"result-census","outcome":outcome,"error_class":"temporary"})).await.unwrap();
        assert_eq!(
            keys(&value),
            BTreeSet::from(["ok", "channel_kind", "channel_slug", "outcome"]),
            "every canonical result field requires sampling review"
        );
        assert_eq!(value["ok"], true);
        assert!(value["ok"].is_boolean());
        assert_eq!(value["channel_kind"].as_str(), Some("email"));
        assert_eq!(value["channel_slug"].as_str(), Some("result-census"));
        assert_eq!(value["outcome"].as_str(), Some(outcome));
    }
    for args in [
        json!({"channel_kind":"email","channel_slug":"result-census"}),
        json!({"channel_kind":"email","channel_slug":"","mode":"legacy_slugless"}),
    ] {
        let value = registry
            .dispatch("comm.cleanup_expired_quarantine", args)
            .await
            .unwrap();
        assert_eq!(
            keys(&value),
            BTreeSet::from(["ok", "deleted", "routed_owners_detached"]),
            "every canonical result field requires sampling review"
        );
        assert_eq!(value["ok"], true);
        assert!(value["ok"].is_boolean());
        assert_eq!(value["deleted"].as_u64(), Some(0));
        assert_eq!(value["routed_owners_detached"].as_u64(), Some(0));
    }
}
