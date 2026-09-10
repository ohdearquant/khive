//! ADR-174 A6: identity is optional; a version belongs to a row, not a key.
use super::*;

fn observation(key: &str, version: Value, id: Option<&Value>) -> Value {
    let mut value = json!({"key":key,"kind":"head","version":version});
    if let Some(id) = id {
        value["id"] = id.clone();
    }
    value
}

fn publication(observed: Value) -> Value {
    json!({"atomic":true,"observed":observed,"ops":[
        {"op":"append","stream":"identity/a","record":1},
        {"op":"write","key":"target","kind":"head","expected_version":1,"doc":{"published":true}},
        {"op":"append","stream":"identity/b","record":2}
    ]})
}

async fn snapshot(rt: &KhiveRuntime, reg: &VerbRegistry) -> (Vec<Value>, Vec<i64>, Value) {
    (
        population(rt).await,
        heads(reg, &["identity/a", "identity/b"]).await,
        reg.dispatch("get", json!({"kind":"head","key":"target"}))
            .await
            .unwrap(),
    )
}

async fn recreate(reg: &VerbRegistry, key: &str) -> (Value, Value) {
    lease(reg, key).await;
    let original = reg
        .dispatch("get", json!({"kind":"head","key":key}))
        .await
        .unwrap();
    reg.dispatch("delete", json!({"id":original["id"]}))
        .await
        .unwrap();
    let replacement = lease(reg, key).await;
    assert_ne!(original["id"], replacement["id"]);
    assert_eq!(original["version"], 1);
    assert_eq!(replacement["version"], 1);
    (original, replacement)
}

async fn assert_committed(reg: &VerbRegistry, args: Value) {
    let result = reg.dispatch("stream.batch", args).await.unwrap();
    assert_eq!(result["committed"], true);
    assert_eq!(result["results"].as_array().unwrap().len(), 3);
    assert_eq!(result["results"][0]["seq"], 1);
    assert_eq!(result["results"][1]["version"], 2);
    assert_eq!(result["results"][2]["seq"], 1);
    assert_eq!(heads(reg, &["identity/a", "identity/b"]).await, vec![1, 1]);
    let target = reg
        .dispatch("get", json!({"kind":"head","key":"target"}))
        .await
        .unwrap();
    assert_eq!(target["version"], 2);
    assert_eq!(
        serde_json::from_str::<Value>(target["content"].as_str().unwrap()).unwrap(),
        json!({"published":true})
    );
}

#[tokio::test]
async fn observed_id_arm1_recreation_refuses_and_original_control_commits() {
    for replaced in [false, true] {
        let (rt, reg) = surface();
        lease(&reg, "target").await;
        let (original, current) = if replaced {
            recreate(&reg, "lease").await
        } else {
            lease(&reg, "lease").await;
            let read = reg
                .dispatch("get", json!({"kind":"head","key":"lease"}))
                .await
                .unwrap();
            (read.clone(), read)
        };
        let args = publication(json!([observation(
            "lease",
            json!(1),
            Some(&original["id"])
        )]));
        if !replaced {
            assert_committed(&reg, args).await;
            continue;
        }
        let before = snapshot(&rt, &reg).await;
        let error = reason(
            reg.dispatch("stream.batch", args).await.unwrap_err(),
            "identity_conflict",
        );
        assert_eq!(
            error["details"],
            json!({"reason":"identity_conflict","key":"lease","kind":"head",
            "version":"1","id":original["id"],"current_id":current["id"],"index":"0"})
        );
        assert!(error["details"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_string));
        assert_eq!(snapshot(&rt, &reg).await, before);
    }
}

#[tokio::test]
async fn observed_id_arm2_unpinned_recreation_commits_against_commit_time_holder() {
    let (_, reg) = surface();
    lease(&reg, "target").await;
    let (original, replacement) = recreate(&reg, "lease").await;
    assert_ne!(original["id"], replacement["id"]);
    // A6.2: equality on the live holder's version is sufficient without id.
    assert_committed(
        &reg,
        publication(json!([observation("lease", json!(1), None)])),
    )
    .await;
}

#[tokio::test]
async fn observed_id_arm3_same_identity_version_move() {
    let (rt, reg) = surface();
    lease(&reg, "target").await;
    let original = lease(&reg, "lease").await;
    reg.dispatch(
        "update",
        json!({"id":original["id"],"content":"{}","expected_version":1}),
    )
    .await
    .unwrap();
    let current = reg
        .dispatch("get", json!({"kind":"head","key":"lease"}))
        .await
        .unwrap();
    assert_eq!(current["id"], original["id"]);
    assert_eq!(current["version"], 2);
    let before = snapshot(&rt, &reg).await;
    let error = reason(
        reg.dispatch(
            "stream.batch",
            publication(json!([observation(
                "lease",
                json!(1),
                Some(&original["id"])
            )])),
        )
        .await
        .unwrap_err(),
        "version_conflict",
    );
    assert_eq!(
        error["details"],
        json!({"reason":"version_conflict","key":"lease","index":"0","expected_version":"1","current_version":"2"})
    );
    assert_eq!(snapshot(&rt, &reg).await, before);
    assert_committed(
        &reg,
        publication(json!([observation(
            "lease",
            json!(2),
            Some(&original["id"])
        )])),
    )
    .await;
}

#[tokio::test]
async fn observed_id_arm4_identity_precedes_version_conflict() {
    let (rt, reg) = surface();
    lease(&reg, "target").await;
    let (original, replacement) = recreate(&reg, "lease").await;
    reg.dispatch(
        "update",
        json!({"id":replacement["id"],"content":"{}","expected_version":1}),
    )
    .await
    .unwrap();
    let before = snapshot(&rt, &reg).await;
    let error = reason(
        reg.dispatch(
            "stream.batch",
            publication(json!([observation(
                "lease",
                json!(1),
                Some(&original["id"])
            )])),
        )
        .await
        .unwrap_err(),
        "identity_conflict",
    );
    assert_eq!(error["details"]["version"], "1");
    assert_eq!(error["details"]["id"], original["id"]);
    assert_eq!(error["details"]["current_id"], replacement["id"]);
    assert_eq!(snapshot(&rt, &reg).await, before);
}

#[tokio::test]
async fn observed_id_arm5_null_version_is_invalid_before_member_writes() {
    let (rt, reg) = surface();
    lease(&reg, "target").await;
    let id = json!(uuid::Uuid::new_v4());
    // An absent key would satisfy the null version if the input guard vanished.
    for version in [Value::Null, json!(0), json!(-1)] {
        let before = snapshot(&rt, &reg).await;
        let error = reg
            .dispatch(
                "stream.batch",
                publication(json!([observation("missing", version, Some(&id))])),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
        assert_eq!(snapshot(&rt, &reg).await, before);
    }
}

#[tokio::test]
async fn observed_id_arm7_mixed_list_checks_both_directions() {
    for bad in [None, Some(0), Some(1)] {
        let (rt, reg) = surface();
        lease(&reg, "target").await;
        let pinned = lease(&reg, "pinned").await;
        let unpinned = lease(&reg, "unpinned").await;
        if bad == Some(0) {
            reg.dispatch("delete", json!({"id":pinned["id"]}))
                .await
                .unwrap();
            lease(&reg, "pinned").await;
        } else if bad == Some(1) {
            reg.dispatch(
                "update",
                json!({"id":unpinned["id"],"content":"{}","expected_version":1}),
            )
            .await
            .unwrap();
        }
        let args = publication(json!([
            observation("pinned", json!(1), Some(&pinned["id"])),
            observation("unpinned", json!(1), None)
        ]));
        if let Some(index) = bad {
            let before = snapshot(&rt, &reg).await;
            let error = reason(
                reg.dispatch("stream.batch", args).await.unwrap_err(),
                if index == 0 {
                    "identity_conflict"
                } else {
                    "version_conflict"
                },
            );
            assert_eq!(
                error["details"]["key"],
                if index == 0 { "pinned" } else { "unpinned" }
            );
            assert_eq!(error["details"]["index"], index.to_string());
            assert_eq!(snapshot(&rt, &reg).await, before);
        } else {
            assert_committed(&reg, args).await;
        }
    }
}

#[tokio::test]
async fn observed_id_arm8_help_states_identity_and_unpinned_semantics() {
    let (_, reg) = surface();
    let help = reg
        .dispatch("stream.batch", json!({"help":true}))
        .await
        .unwrap();
    let observed = help["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|param| param["name"] == "observed")
        .unwrap();
    let text = observed["description"].as_str().unwrap();
    for required in ["identity_conflict", "id", "Without id", "at commit time"] {
        assert!(text.contains(required), "missing {required}: {help}");
    }
}

#[tokio::test]
async fn observed_id_arm5b_absence_and_replacement_have_distinct_reasons() {
    let (rt, reg) = surface();
    lease(&reg, "target").await;
    lease(&reg, "lease").await;
    let original = reg
        .dispatch("get", json!({"kind":"head","key":"lease"}))
        .await
        .unwrap();
    reg.dispatch("delete", json!({"id":original["id"]}))
        .await
        .unwrap();
    let args = publication(json!([observation(
        "lease",
        json!(1),
        Some(&original["id"])
    )]));
    let before = snapshot(&rt, &reg).await;
    let absent = reason(
        reg.dispatch("stream.batch", args.clone())
            .await
            .unwrap_err(),
        "version_conflict",
    );
    assert_eq!(
        absent["details"],
        json!({"reason":"version_conflict","key":"lease","index":"0","expected_version":"1"})
    );
    assert!(absent["details"].get("current_version").is_none());
    assert!(absent["details"].get("current_id").is_none());
    assert_eq!(snapshot(&rt, &reg).await, before);
    let replacement = lease(&reg, "lease").await;
    assert_ne!(replacement["id"], original["id"]);
    let before = snapshot(&rt, &reg).await;
    let replaced = reason(
        reg.dispatch("stream.batch", args).await.unwrap_err(),
        "identity_conflict",
    );
    assert_eq!(replaced["details"]["current_id"], replacement["id"]);
    assert_eq!(replaced["details"]["id"], original["id"]);
    assert_eq!(snapshot(&rt, &reg).await, before);
}
