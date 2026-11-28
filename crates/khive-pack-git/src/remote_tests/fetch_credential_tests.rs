// git.fetch over an HTTPS mapping carries the calling actor's credential, and only theirs.

#[tokio::test]
async fn https_fetch_uses_the_actor_credential_and_refuses_an_unmapped_actor() {
    let f = Fixture::new(true, None).await;
    f.policy(&f.actor, "git.fetch", "allow").await;
    let reply = f
        .call(&f.actor, "git.fetch", json!({"ref":"work"}))
        .await
        .unwrap();
    assert_eq!(reply["ref"], "refs/remotes/origin/work");
    assert_eq!(reply["sha"], f.base);
    assert_eq!(reply["remote"], REMOTE);
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Committed);
    assert_eq!(receipt.credential["source"], "actor");
    assert_eq!(receipt.credential["ref"], "author-ref");
    let fetches: Vec<Value> = f
        .remote
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|call| call["op"] == "fetch")
        .cloned()
        .collect();
    assert_eq!(
        fetches,
        vec![json!({"op":"fetch","source":"refs/heads/work",
            "token_hash":blake3::hash(SECRET.as_bytes()).to_hex().to_string()})]
    );
    // A fetch reads the credential and nothing at the platform's API.
    assert!(f
        .remote
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .all(|call| call.get("method").is_none()));

    // A caller with no actor row is refused before any transfer.
    let nobody = format!("{}:nobody", f.actor);
    f.policy(&nobody, "git.fetch", "allow").await;
    let calls = f.remote.state.lock().unwrap().calls.len();
    let receipt = f
        .refusal(&nobody, "git.fetch", json!({"ref":"work"}), "actor_unmapped")
        .await;
    assert_eq!(
        receipt.result["refusal"],
        json!({"table":"git_write.actors","key":nobody,"cause":"absent"})
    );
    assert_eq!(f.remote.state.lock().unwrap().calls.len(), calls);
}
