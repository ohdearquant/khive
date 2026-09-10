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

#[derive(Debug)]
struct ObservedDisclosurePolicy {
    allow_holder: bool,
    seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

impl khive_gate::Gate for ObservedDisclosurePolicy {
    fn check(
        &self,
        request: &khive_gate::GateRequest,
    ) -> Result<khive_gate::GateDecision, khive_gate::GateError> {
        self.seen
            .lock()
            .unwrap()
            .push((request.verb.clone(), request.args.clone()));
        let holder_listing = request.verb == "list"
            && request.args["note_kind"] == "head"
            && request.args["key_prefix"] == "private/key";
        Ok(
            if request.verb == "get" || (holder_listing && !self.allow_holder) {
                khive_gate::GateDecision::deny("holder reads denied by test policy")
            } else {
                // A lookup of the unrelated write target would wrongly allow disclosure.
                khive_gate::GateDecision::allow()
            },
        )
    }
}

#[tokio::test]
async fn observed_id_arm10b_per_member_mode_refuses_observed_before_any_holder_read() {
    use std::sync::{Arc, Mutex};

    // Arm 10's second half. The disclosure filter has two sites, the transactional error rewrite
    // and the per-member result strip, and only the first can ever carry an identity_conflict:
    // `observed` is refused outside atomic mode before any check runs, so the per-member path
    // cannot produce the reason at all. Filtering at one site is sound because of that, not
    // because the other site happens to be untested, and this arm is what says so. If a later
    // amendment admits `observed` in per-member mode, this goes red and names the site that then
    // needs the filter.
    let (rt, setup) = surface();
    lease(&setup, "target").await;
    let (original, _holder) = recreate(&setup, "private/key").await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(Arc::new(ObservedDisclosurePolicy {
        allow_holder: false,
        seen: seen.clone(),
    }));
    builder.register(crate::KgPack::new(rt.clone()));
    let reg = builder.build().unwrap();
    let mut args = publication(json!([observation(
        "private/key",
        json!(1),
        Some(&original["id"])
    )]));
    args["atomic"] = json!(false);
    let before = snapshot(&rt, &setup).await;
    seen.lock().unwrap().clear();
    let error = reg
        .dispatch("stream.batch", args.clone())
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
    // The refusal is the mode guard, so nothing read the key's holder on the way to it.
    assert_eq!(
        *seen.lock().unwrap(),
        vec![("stream.batch".to_owned(), args)]
    );
    assert_eq!(snapshot(&rt, &setup).await, before);
}

#[tokio::test]
async fn observed_id_arm10_disclosure_uses_observation_key_and_existing_policy() {
    use std::sync::{Arc, Mutex};

    // Index 1 aliases a different write; index 4 is beyond the three members.
    for observed_index in [1, 4] {
        let (rt, setup) = surface();
        lease(&setup, "target").await;
        let (original, holder) = recreate(&setup, "private/key").await;
        for allow_holder in [true, false] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut builder = VerbRegistryBuilder::new();
            builder.with_gate(Arc::new(ObservedDisclosurePolicy {
                allow_holder,
                seen: seen.clone(),
            }));
            builder.register(crate::KgPack::new(rt.clone()));
            let reg = builder.build().unwrap();
            let guessed = json!(uuid::Uuid::nil().to_string());
            assert_ne!(guessed, holder["id"]);
            let asserted = if allow_holder {
                original["id"].clone()
            } else {
                guessed
            };
            let mut entries: Vec<Value> = (0..observed_index)
                .map(|index| observation(&format!("absent-{index}"), Value::Null, None))
                .collect();
            entries.push(observation("private/key", json!(1), Some(&asserted)));
            let args = publication(json!(entries));
            let before = snapshot(&rt, &setup).await;
            seen.lock().unwrap().clear();
            let error = reason(
                reg.dispatch("stream.batch", args.clone())
                    .await
                    .unwrap_err(),
                "identity_conflict",
            );
            let mut expected = json!({"reason":"identity_conflict", "key":"private/key", "kind":"head", "version":"1", "id":asserted, "index":observed_index.to_string()});
            if allow_holder {
                expected["current_id"] = holder["id"].clone();
            }
            assert_eq!(
                error["details"], expected,
                "holder disclosure must follow scoped authorization"
            );
            assert_eq!(
                *seen.lock().unwrap(),
                vec![
                    ("stream.batch".to_owned(), args),
                    (
                        "list".to_owned(),
                        json!({"kind":"note","note_kind":"head","key_prefix":"private/key"})
                    ),
                ]
            );
            assert_eq!(snapshot(&rt, &setup).await, before);

            // The same caller's existing keyed-create refusal is the disclosure control.
            let create_args = json!({"atomic":true,"ops":[
                {"op":"append","stream":"identity/a","record":1},
                {"op":"write","key":"private/key","kind":"head","doc":{}},
                {"op":"append","stream":"identity/b","record":2}
            ]});
            seen.lock().unwrap().clear();
            let control = reason(
                reg.dispatch("stream.batch", create_args.clone())
                    .await
                    .unwrap_err(),
                "key_conflict",
            );
            assert_eq!(
                control["details"].get("existing_id"),
                allow_holder.then_some(&holder["id"])
            );
            assert_eq!(control["details"]["key"], "private/key");
            assert_eq!(
                *seen.lock().unwrap(),
                vec![
                    ("stream.batch".to_owned(), create_args),
                    (
                        "list".to_owned(),
                        json!({"kind":"note","note_kind":"head","key_prefix":"private/key"})
                    ),
                ]
            );
            assert_eq!(snapshot(&rt, &setup).await, before);

            if !allow_holder {
                // No new admission rule: a legitimate pin still commits for this caller.
                let args = publication(json!([observation(
                    "private/key",
                    json!(1),
                    Some(&holder["id"])
                )]));
                seen.lock().unwrap().clear();
                let committed = reg.dispatch("stream.batch", args.clone()).await.unwrap();
                assert_eq!(committed["committed"], true);
                assert_eq!(committed["results"][0]["seq"], 1);
                assert_eq!(committed["results"][1]["version"], 2);
                assert_eq!(committed["results"][2]["seq"], 1);
                assert_eq!(
                    *seen.lock().unwrap(),
                    vec![("stream.batch".to_owned(), args)]
                );
                assert_eq!(
                    heads(&setup, &["identity/a", "identity/b"]).await,
                    vec![1, 1]
                );
                let target = setup
                    .dispatch("get", json!({"key":"target","kind":"head"}))
                    .await
                    .unwrap();
                assert_eq!(target["version"], 2);
                assert_eq!(
                    serde_json::from_str::<Value>(target["content"].as_str().unwrap()).unwrap(),
                    json!({"published":true})
                );
            }
        }
    }
}
