use khive_pack_kg::KgPack;
use khive_pack_session::SessionPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};

struct StoredSession {
    registry: VerbRegistry,
    record: Value,
}

async fn stored_session() -> StoredSession {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(SessionPack::new(runtime.clone()));
    let registry = builder.build().expect("registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    let stored = registry
        .dispatch(
            "session.store",
            json!({
                "content":"session refusal witness", "title":"Refusal fixture",
                "provider":"codex", "provider_session_id":"fixture-session",
                "tags":["refusal-witness"]
            }),
        )
        .await
        .expect("store actual session");
    assert_eq!(stored["ok"], true);
    assert_eq!(stored["session"]["content"], "session refusal witness");
    StoredSession {
        registry,
        record: stored["session"].clone(),
    }
}

fn refused_values() -> [(String, &'static str); 2] {
    [
        ("bogus".into(), "bogus"),
        (format!("sk-proj-{}", "A".repeat(80)), "***MASKED***"),
    ]
}

async fn assert_refusal(
    registry: &VerbRegistry,
    verb: &str,
    args: Value,
    raw: &str,
    expected: String,
) {
    let error = registry
        .dispatch(verb, args)
        .await
        .expect_err("invalid parameter must refuse");
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("{verb} changed its refusal variant");
    };
    if raw != "bogus" {
        assert!(
            !message.contains(raw),
            "{verb} exposed the synthetic credential"
        );
        assert!(message.contains("***MASKED***"), "{verb} omitted the mask");
    }
    assert_eq!(message, &expected);
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({
            "kind":"runtime_error", "message":format!("invalid input: {expected}"),
            "domain_disposition":"not_committed"
        })
    );
}

async fn assert_stored_record_unchanged(fixture: &StoredSession) {
    let resumed = fixture
        .registry
        .dispatch("session.resume", json!({"id":fixture.record["id"]}))
        .await
        .expect("resume stored fixture");
    assert_eq!(resumed["session"], fixture.record);
}

#[tokio::test]
async fn export_masks_refused_formats_and_preserves_json_and_markdown() {
    let f = stored_session().await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &f.registry,
            "session.export",
            json!({"id":f.record["id"], "format":raw}),
            &raw,
            format!("session.export: format must be one of [\"json\", \"markdown\"]; got {shown}"),
        )
        .await;
    }
    for format in [None, Some("json"), Some("markdown")] {
        let exported = f
            .registry
            .dispatch(
                "session.export",
                json!({"id":f.record["id"], "format":format}),
            )
            .await
            .expect("valid export format");
        assert_eq!(exported["ok"], true);
        assert_eq!(exported["format"], format.unwrap_or("json"));
        if format == Some("markdown") {
            let body = exported["content"].as_str().expect("markdown body");
            assert!(body.starts_with("# Refusal fixture\n"));
            assert!(body.contains("- provider: codex\n"));
            assert!(body.contains("- provider_session_id: fixture-session\n"));
            assert!(body.ends_with("## Content\n\nsession refusal witness"));
        } else {
            assert_eq!(exported["session"], f.record);
        }
    }
    assert_stored_record_unchanged(&f).await;
}

#[tokio::test]
async fn resume_and_export_mask_malformed_ids_and_preserve_uuid_and_prefix_lookup() {
    let f = stored_session().await;
    let full = f.record["id"].as_str().expect("stored UUID");
    let compact = full.replace('-', "");
    for verb in ["session.resume", "session.export"] {
        for (raw, shown) in refused_values() {
            assert_refusal(
                &f.registry,
                verb,
                json!({"id":raw}),
                &raw,
                format!(
                    "{verb}: id must be a full UUID or 8+ hex prefix; \
                     valid values: full UUID or 8+ hex prefix; got {shown}"
                ),
            )
            .await;
        }
        for id in [full, &compact[..8]] {
            let result = f.registry.dispatch(verb, json!({"id":id})).await.unwrap();
            assert_eq!(result["session"], f.record);
        }
    }
}

#[tokio::test]
async fn list_masks_refused_since_and_preserves_the_inclusive_storage_filter() {
    let f = stored_session().await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &f.registry,
            "session.list",
            json!({"since":raw}),
            &raw,
            format!(
                "session.list: since must be an RFC 3339 timestamp; valid example: \
                 2026-01-01T00:00:00Z; got {shown:?}"
            ),
        )
        .await;
    }
    let since = f.record["created_at"].as_str().expect("stored timestamp");
    for params in [json!({}), json!({"since":since})] {
        let listed = f.registry.dispatch("session.list", params).await.unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["total"], 1);
        assert_eq!(listed["sessions"][0]["id"], f.record["id"]);
        assert_eq!(listed["sessions"][0]["provider"], "codex");
        assert!(listed["sessions"][0].get("content").is_none());
    }
    let future = f
        .registry
        .dispatch("session.list", json!({"since":"2100-01-01T00:00:00Z"}))
        .await
        .unwrap();
    assert_eq!(future["count"], 0);
    assert_eq!(future["total"], 0);
    assert_stored_record_unchanged(&f).await;
}

async fn assert_search_still_gated(registry: &VerbRegistry, params: Value) {
    let error = registry
        .dispatch("session.search", params)
        .await
        .expect_err("valid search parameters retain the readiness gate");
    let RuntimeError::Unconfigured(message) = error else {
        panic!("session.search changed its readiness refusal");
    };
    assert_eq!(
        message,
        "session.search requires transcript deletion and resume/export continuity support"
    );
}

#[tokio::test]
async fn search_masks_refused_sources_and_preserves_the_readiness_gate() {
    let f = stored_session().await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &f.registry,
            "session.search",
            json!({"query":"session", "source":raw}),
            &raw,
            format!(
                "session.search: source must be one of claude_code, codex, chatgpt_export, \
                 claude_ai_export, unknown; got {shown:?}"
            ),
        )
        .await;
    }
    for source in [
        "claude_code",
        "codex",
        "chatgpt_export",
        "claude_ai_export",
        "unknown",
    ] {
        assert_search_still_gated(&f.registry, json!({"query":"session", "source":source})).await;
    }
    assert_stored_record_unchanged(&f).await;
}

#[tokio::test]
async fn search_masks_refused_since_and_preserves_valid_timestamp_admission() {
    let f = stored_session().await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &f.registry,
            "session.search",
            json!({"query":"session", "since":raw}),
            &raw,
            format!("session.search: since must be an RFC 3339 timestamp; got {shown:?}"),
        )
        .await;
    }
    for since in [
        None,
        Some("2026-01-01T00:00:00Z"),
        f.record["created_at"].as_str(),
    ] {
        assert_search_still_gated(
            &f.registry,
            json!({"query":"session", "source":"codex", "since":since}),
        )
        .await;
    }
    assert_stored_record_unchanged(&f).await;
}
