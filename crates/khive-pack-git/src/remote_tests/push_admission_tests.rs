#[cfg(feature = "contract-faults")]
#[tokio::test]
async fn remote_push_marker_preserves_pusher_evidence_after_reply_loss() {
    let f = Fixture::new(true, Some("git.push:reply-lost-after-effect")).await;
    assert!(f.call(&f.reviewer, "git.push", f.push()).await.is_err());
    let push = f.last(&f.reviewer).await;
    assert_eq!(push.disposition, Disposition::Unknown);
    let n = f.open().await;
    assert!(f
        .review(&f.reviewer, n)
        .await
        .unwrap_err()
        .to_string()
        .contains("last_pusher"));
    assert_eq!(
        f.last(&f.reviewer).await.result["last_pusher"]["push_receipt_id"],
        push.id
    );
}

#[tokio::test]
async fn remote_push_gate_policy_and_positive_control() {
    for (gate, decision) in [(false, "allow"), (true, "deny"), (true, "allow")] {
        let f = Fixture::new(gate, None).await;
        let id = f.policy(&f.actor, "git.push", decision).await;
        let before = crate::local_handlers::policy_check_count(&f.actor);
        if !gate || decision == "deny" {
            f.refusal(
                &f.actor,
                "git.push",
                f.push(),
                if gate {
                    "policy_denied"
                } else {
                    "not_configured"
                },
            )
            .await;
            assert_eq!(f.remote.state.lock().unwrap().push_calls, 0);
            assert!(f.remote.state.lock().unwrap().calls.is_empty());
        } else {
            let result = f.call(&f.actor, "git.push", f.push()).await.unwrap();
            assert_eq!(result["sha"], f.head);
            assert_eq!(remote_head(&f.remote.bare), Some(f.head.clone()));
        }
        assert_eq!(
            crate::local_handlers::policy_check_count(&f.actor) - before,
            usize::from(gate)
        );
        let receipt = f.last(&f.actor).await;
        assert_eq!(
            receipt.gate["decision"],
            if gate { "allow" } else { "deny" }
        );
        if gate {
            assert_eq!(receipt.policy["id"], id);
            assert_eq!(receipt.policy["decision"], decision);
        } else {
            assert!(receipt.policy.is_null());
        }
    }
}

#[tokio::test]
async fn remote_push_compares_at_local_remote_and_native_effect_seams() {
    for seam in ["local", "remote", "effect"] {
        let f = Fixture::new(true, None).await;
        if seam == "local" {
            git(&f.repo, &["update-ref", "refs/heads/work", &f.middle]);
        }
        if seam == "remote" {
            git(
                &f.remote.bare,
                &["update-ref", "refs/heads/work", &f.middle],
            );
        }
        if seam == "effect" {
            f.remote.state.lock().unwrap().remote_race = Some(f.middle.clone());
        }
        let result = f.call(&f.actor, "git.push", f.push()).await;
        assert!(result.is_err(), "{seam}: rival remote was overwritten");
        assert_eq!(
            f.last(&f.actor).await.disposition,
            Disposition::NotCommitted
        );
        assert_eq!(
            remote_head(&f.remote.bare),
            Some(if seam == "local" {
                f.base.clone()
            } else {
                f.middle.clone()
            })
        );
        assert_eq!(
            git(&f.repo, &["rev-parse", "work"]),
            if seam == "local" {
                f.middle.as_str()
            } else {
                f.head.as_str()
            }
        );
    }
}

#[tokio::test]
async fn remote_push_force_true_is_unconditionally_denied_before_transport() {
    // This fixture has both the git-write Gate and an allow policy for this actor.
    let f = Fixture::new(true, None).await;
    let mut p = f.push();
    p["force"] = json!(true);
    let receipt = f.refusal(&f.actor, "git.push", p, "force_denied").await;
    assert_eq!(receipt.reason.as_deref(), Some("force_denied"));
    assert_eq!(receipt.inputs["force"], true);
    assert!(f.remote.state.lock().unwrap().calls.is_empty());
    assert_eq!(f.remote.state.lock().unwrap().push_calls, 0);
    assert!(
        !f.dir.path().join("reads").exists(),
        "force denial must happen before resolving a platform credential"
    );
}

#[tokio::test]
async fn remote_push_false_and_omitted_force_preserve_the_ordinary_push() {
    for force in [Some(false), None] {
        // Each successful push advances its bare remote, so controls need fresh fixtures.
        let f = Fixture::new(true, None).await;
        let local_refs = git(&f.repo, &["show-ref"]);
        let mut p = f.push();
        if let Some(force) = force {
            p["force"] = json!(force);
        }
        let result = f.call(&f.actor, "git.push", p).await.unwrap();
        assert_eq!(result["sha"], f.head);
        assert_eq!(remote_head(&f.remote.bare), Some(f.head.clone()));
        assert_eq!(git(&f.repo, &["show-ref"]), local_refs);
        let receipt = f.last(&f.actor).await;
        assert_eq!(receipt.disposition, Disposition::Committed);
        assert_eq!(receipt.reason, None);
        assert_eq!(
            receipt.inputs.get("force"),
            force.map(|_| json!(false)).as_ref()
        );
        let state = f.remote.state.lock().unwrap();
        assert_eq!(state.push_calls, 1);
        assert_eq!(state.writes, 1);
        assert!(state.calls.iter().any(|call| call["op"] == "remote_ref"));
        assert!(state.calls.iter().any(|call| call["op"] == "push"
            && call["sha"] == f.head
            && call["expected_remote"] == f.base));
    }
}

#[tokio::test]
async fn remote_push_rejects_non_boolean_force_before_transport() {
    let f = Fixture::new(true, None).await;
    for force in [
        Value::Null,
        json!(0),
        json!(1),
        json!(-1),
        json!(1.5),
        json!("true"),
        json!("false"),
        json!([]),
        json!({}),
    ] {
        let mut p = f.push();
        p["force"] = force;
        let receipt = f
            .refusal(&f.actor, "git.push", p, "force must be a boolean")
            .await;
        assert_eq!(receipt.reason.as_deref(), Some("invalid_params"));
        assert!(f.remote.state.lock().unwrap().calls.is_empty());
    }
    assert_eq!(f.remote.state.lock().unwrap().push_calls, 0);
    assert!(!f.dir.path().join("reads").exists());
}

#[tokio::test]
async fn remote_push_rejects_unknown_overrides_and_omission_before_network() {
    let f = Fixture::new(true, None).await;
    for extra in [
        json!({"force_with_lease":true}),
        json!({"force_with_lease":false}),
        json!({"refspec":"+HEAD:refs/heads/main"}),
        json!({"argv":["--force"]}),
        json!({"remote":"elsewhere"}),
        json!({"actor":"spoof"}),
        json!({"credential":SECRET}),
    ] {
        let mut p = f.push();
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let unknown = extra.as_object().unwrap().keys().next().unwrap();
        let receipt = f
            .refusal(
                &f.actor,
                "git.push",
                p,
                &format!("unknown field `{unknown}`"),
            )
            .await;
        assert_eq!(receipt.reason.as_deref(), Some("invalid_params"));
        assert!(!receipt.to_value().to_string().contains(SECRET));
    }
    let mut p = f.push();
    p.as_object_mut().unwrap().remove("expected_remote");
    f.refusal(&f.actor, "git.push", p, "expected_remote is required")
        .await;
    assert!(f.remote.state.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn remote_push_null_is_create_only_and_equal_sha_is_not_an_effect() {
    let f = Fixture::new(true, None).await;
    let mut p = f.push();
    p["expected_remote"] = Value::Null;
    f.refusal(&f.actor, "git.push", p.clone(), "expected_remote_mismatch")
        .await;
    git(&f.remote.bare, &["update-ref", "-d", "refs/heads/work"]);
    let refs = git(&f.repo, &["show-ref"]);
    f.call(&f.actor, "git.push", p.clone()).await.unwrap();
    assert_eq!(remote_head(&f.remote.bare), Some(f.head.clone()));
    assert_eq!(git(&f.repo, &["show-ref"]), refs);
    f.refusal(&f.actor, "git.push", p, "expected_remote_mismatch")
        .await;
    let mut p = f.push();
    p["expected_remote"] = json!(f.head);
    f.refusal(&f.actor, "git.push", p, "already_at_target")
        .await;
}
