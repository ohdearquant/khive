use super::*;
use chrono::{Duration, Utc};

fn expiry_surface() -> (KhiveRuntime, VerbRegistry) {
    let rt = KhiveRuntime::memory().unwrap();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    (rt, builder.build().unwrap())
}

async fn document(registry: &VerbRegistry, key: &str, doc: Value, version: Option<i64>) -> Value {
    registry
        .dispatch(
            "stream.batch",
            json!({"ops": [{"op":"write", "kind":"head", "key":key,
        "doc":doc, "embed":false, "expected_version":version}]}),
        )
        .await
        .unwrap()["results"][0]
        .clone()
}

fn observation(key: &str, version: i64, field: Option<&str>) -> Value {
    let mut value = json!({"key":key, "kind":"head", "version":version});
    if let Some(field) = field {
        value["live_until"] = json!(field);
    }
    value
}

fn publication(observed: Value) -> Value {
    json!({"atomic":true, "observed":observed, "ops":[
        {"op":"append", "stream":"expiry-a", "record":1},
        {"op":"append", "stream":"expiry-b", "record":2}
    ]})
}

async fn refused_unchanged(
    rt: &KhiveRuntime,
    registry: &VerbRegistry,
    args: Value,
    why: &str,
    key: &str,
) -> Value {
    let counts = population(rt).await;
    let before_heads = heads(registry, &["expiry-a", "expiry-b"]).await;
    let error = reason(
        registry.dispatch("stream.batch", args).await.unwrap_err(),
        why,
    );
    assert_eq!(error["details"]["key"], key);
    assert_eq!(population(rt).await, counts);
    assert_eq!(
        heads(registry, &["expiry-a", "expiry-b"]).await,
        before_heads
    );
    error
}

#[tokio::test]
async fn expiry_arm1_expired_after_one_second_and_before_expiry_control() {
    let (rt, registry) = expiry_surface();
    let deadline = Utc::now() + Duration::seconds(1);
    let value = deadline.to_rfc3339();
    let written = document(&registry, "lease", json!({"expires_at":value}), None).await;
    let args = publication(json!([observation("lease", 1, Some("expires_at"))]));
    let control = registry
        .dispatch("stream.batch", args.clone())
        .await
        .unwrap();
    assert_eq!(control["committed"], true);
    assert_eq!(
        heads(&registry, &["expiry-a", "expiry-b"]).await,
        vec![1, 1]
    );
    let wait = (deadline - Utc::now()).to_std().unwrap_or_default();
    tokio::time::sleep(wait + std::time::Duration::from_millis(20)).await;
    let error = refused_unchanged(&rt, &registry, args, "expired", "lease").await;
    assert_eq!(error["details"]["field"], "expires_at");
    assert_eq!(error["details"]["value"], json!(value).to_string());
    assert_eq!(error["details"]["kind"], "head");
    assert_eq!(error["details"]["version"], "1");
    let now =
        chrono::DateTime::parse_from_rfc3339(error["details"]["now"].as_str().unwrap()).unwrap();
    assert!(now > deadline);
    let after = registry
        .dispatch("get", json!({"id":written["id"]}))
        .await
        .unwrap();
    assert_eq!(after["version"], 1);
}

#[tokio::test]
async fn expiry_arm2_live_and_pinned_version() {
    let (rt, registry) = expiry_surface();
    let doc = json!({"expires_at":(Utc::now()+Duration::hours(1)).to_rfc3339()});
    document(&registry, "lease", doc.clone(), None).await;
    let args = publication(json!([observation("lease", 1, Some("expires_at"))]));
    assert_eq!(
        registry
            .dispatch("stream.batch", args.clone())
            .await
            .unwrap()["committed"],
        true
    );
    document(&registry, "lease", doc, Some(1)).await;
    let error = refused_unchanged(&rt, &registry, args, "version_conflict", "lease").await;
    assert_eq!(error["details"]["current_version"], "2");
}

#[tokio::test]
async fn expiry_arm3_unreadable_fields_fail_closed() {
    for (found, value_type) in [
        (None, "absent"),
        (Some(json!("soon")), "string"),
        (Some(json!("2030-01-01T00:00:00")), "string"),
        (Some(json!(42)), "number"),
        (Some(json!({})), "object"),
        (Some(Value::Null), "null"),
    ] {
        let (rt, registry) = expiry_surface();
        let mut doc = json!({});
        if let Some(value) = &found {
            doc["expires_at"] = value.clone();
        }
        document(&registry, "lease", doc, None).await;
        let error = refused_unchanged(
            &rt,
            &registry,
            publication(json!([observation("lease", 1, Some("expires_at"))])),
            "live_until_unreadable",
            "lease",
        )
        .await;
        // The value itself is never echoed here: the path is the caller's, so the
        // refusal names the type it found and nothing of the document's contents.
        assert_eq!(error["details"]["value_type"], value_type);
        assert!(error["details"].get("value").is_none());
        assert_eq!(error["details"]["field"], "expires_at");
        assert_eq!(error["details"]["kind"], "head");
        assert_eq!(error["details"]["version"], "1");
        assert!(error["details"].get("now").is_none());
    }
}

#[tokio::test]
async fn expiry_arm4_dotted_path_and_offset_instants() {
    let (rt, registry) = expiry_surface();
    let future = (Utc::now() + Duration::hours(1))
        .with_timezone(&chrono::FixedOffset::east_opt(14 * 3600).unwrap())
        .to_rfc3339();
    document(
        &registry,
        "lease",
        json!({"lease":{"expires_at":future},"expires_at":"2000-01-01T00:00:00-12:00"}),
        None,
    )
    .await;
    assert_eq!(
        registry
            .dispatch(
                "stream.batch",
                publication(json!([observation("lease", 1, Some("lease.expires_at"))]))
            )
            .await
            .unwrap()["committed"],
        true
    );
    refused_unchanged(
        &rt,
        &registry,
        publication(json!([observation("lease", 1, Some("expires_at"))])),
        "expired",
        "lease",
    )
    .await;
}

#[tokio::test]
async fn expiry_arm5_null_version_refused_before_members() {
    let (rt, registry) = expiry_surface();
    let counts = population(&rt).await;
    let error = registry
        .dispatch(
            "stream.batch",
            publication(
                json!([{"key":"absent","kind":"head","version":null,"live_until":"expires_at"}]),
            ),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    assert!(error.to_string().contains("live_until requires"));
    assert_eq!(population(&rt).await, counts);
    assert_eq!(
        heads(&registry, &["expiry-a", "expiry-b"]).await,
        vec![0, 0]
    );
}

#[tokio::test]
async fn expiry_arm6_mixed_list_refuses_either_predicate() {
    for reverse in [false, true] {
        for invalid in [None, Some("version"), Some("time")] {
            let (rt, registry) = expiry_surface();
            document(&registry, "pinned", json!({}), None).await;
            let deadline = if invalid == Some("time") {
                "2000-01-01T00:00:00Z"
            } else {
                "9999-01-01T00:00:00Z"
            };
            document(&registry, "timed", json!({"expires_at":deadline}), None).await;
            let mut observations = vec![
                observation(
                    "pinned",
                    if invalid == Some("version") { 2 } else { 1 },
                    None,
                ),
                observation("timed", 1, Some("expires_at")),
            ];
            if reverse {
                observations.reverse();
            }
            let args = publication(json!(observations));
            match invalid {
                None => assert_eq!(
                    registry.dispatch("stream.batch", args).await.unwrap()["committed"],
                    true
                ),
                Some("version") => {
                    refused_unchanged(&rt, &registry, args, "version_conflict", "pinned").await;
                }
                _ => {
                    refused_unchanged(&rt, &registry, args, "expired", "timed").await;
                }
            }
        }
    }
}

#[tokio::test]
async fn expiry_arm8_updated_at_create_update_both_modes() {
    for atomic in [true, false] {
        let (_, registry) = expiry_surface();
        let mut previous = Value::Null;
        for version in [None, Some(1)] {
            let result=registry.dispatch("stream.batch",json!({"atomic":atomic,"ops":[{"op":"write","kind":"head","key":"time","doc":{"v":version},"expected_version":version}]})).await.unwrap()["results"][0].clone();
            let timestamp = result["updated_at"].as_str().unwrap();
            assert_eq!(
                timestamp
                    .split('.')
                    .nth(1)
                    .unwrap()
                    .trim_end_matches('Z')
                    .len(),
                6
            );
            chrono::DateTime::parse_from_rfc3339(timestamp).unwrap();
            let stored = registry
                .dispatch("get", json!({"id":result["id"]}))
                .await
                .unwrap();
            assert_eq!(result["updated_at"], stored["updated_at"]);
            assert_eq!(result["version"], stored["version"]);
            if !previous.is_null() {
                assert_ne!(result["updated_at"], previous["updated_at"]);
                assert_eq!(previous["version"], 1);
            }
            previous = result;
        }
    }
}

#[tokio::test]
async fn expiry_arm9_help_and_observed_mode() {
    let (rt, registry) = expiry_surface();
    let help = registry
        .dispatch("stream.batch", json!({"help":true}))
        .await
        .unwrap()
        .to_string();
    for expected in [
        "Requires atomic mode",
        "observed alone does not select",
        "live_until",
        "lease.expires_at",
        "atomic=true",
    ] {
        assert!(help.contains(expected), "missing {expected}: {help}");
    }
    let before = population(&rt).await;
    let mut args = publication(json!([observation("lease", 1, Some("expires_at"))]));
    args.as_object_mut().unwrap().remove("atomic");
    let error = registry.dispatch("stream.batch", args).await.unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    assert!(error.to_string().contains("requires atomic mode"));
    assert_eq!(population(&rt).await, before);
}
