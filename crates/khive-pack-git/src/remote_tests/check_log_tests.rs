fn log_run(id: u64, app: &str, status: &str) -> Value {
    json!({"id":id,"name":"CI / test (linux)","head_sha":"a".repeat(40),"status":status,
        "conclusion":if status == "completed" { json!("failure") } else { Value::Null },
        "app":{"slug":app},"started_at":"2026-11-21T15:00:00Z",
        "completed_at":"2026-11-21T15:09:00Z",
        "output":{"title":"1 test failed","summary":"first line\r\nsecond line"}})
}

fn log_job(id: u64, run: u64) -> Value {
    json!({"id":id,"check_run_url":format!("https://api.github.com/repos/{SLUG}/check-runs/{run}")})
}

fn log_store(f: &Fixture) -> Arc<dyn khive_storage::BlobStore> {
    let store: Arc<dyn khive_storage::BlobStore> = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(f.dir.path().join("blobs"), 0).unwrap(),
    );
    f.rt.install_blob_store(store.clone()).unwrap();
    store
}

fn log_calls(f: &Fixture) -> Vec<Value> {
    f.remote.state.lock().unwrap().calls.clone()
}

fn log_ready(f: &Fixture, log: &[u8]) {
    let mut s = f.remote.state.lock().unwrap();
    s.check_runs = vec![log_run(7, "github-actions", "completed")];
    s.jobs.insert(7, log_job(7, 7));
    s.logs.insert(7, log.to_vec());
    s.log_failure = None;
    s.mismatch = None;
    s.rate_limited = None;
}

async fn log_stored(store: &Arc<dyn khive_storage::BlobStore>, reply: &Value) -> String {
    let content = khive_storage::ContentRef::from_hex(reply["log"].as_str().unwrap()).unwrap();
    let bytes = store
        .get_bounded_verified(&content, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

fn log_reads(f: &Fixture) -> Option<String> {
    std::fs::read_to_string(f.dir.path().join("reads")).ok()
}

/// The bound on the kept bytes, removed again when the test ends. The fixture holds the
/// environment lock for the test's whole length.
struct LogLimit;

impl LogLimit {
    fn set(bytes: u64) -> Self {
        std::env::set_var("KHIVE_GIT_CHECK_LOG_MAX_BYTES", bytes.to_string());
        Self
    }
}

impl Drop for LogLimit {
    fn drop(&mut self) {
        std::env::remove_var("KHIVE_GIT_CHECK_LOG_MAX_BYTES");
    }
}

#[tokio::test]
async fn remote_check_log_stores_the_log_reports_its_end_and_writes_nothing_else() {
    let f = Fixture::new(true, None).await;
    let store = log_store(&f);
    let mut log = String::from("\u{feff}");
    for i in 0..300 {
        log.push_str(&format!("step {i}\r\n"));
    }
    log.push_str(&format!("\u{1b}[31merror\u{1b}[0m: sent {SECRET} upstream\n"));
    log_ready(&f, log.as_bytes());
    f.policy(&f.actor, "git.check_log", "allow").await;
    let writes = f.remote.writes();
    let receipts = state_receipt_count(&f, &f.actor).await;
    let refs = git(&f.repo, &["show-ref"]);

    let reply = f
        .call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap();

    assert_eq!(reply["slug"], SLUG);
    assert_eq!(reply["gate"]["decision"], "allow");
    assert_eq!(
        reply["check_run"],
        json!({"id":7,"name":"CI / test (linux)","head_sha":"a".repeat(40),
            "status":"completed","conclusion":"failure","started_at":"2026-11-21T15:00:00Z",
            "completed_at":"2026-11-21T15:09:00Z","title":"1 test failed",
            "summary":"first line\nsecond line"})
    );
    let expected = (0..300)
        .map(|i| format!("step {i}\n"))
        .collect::<String>()
        + "error: sent [redacted] upstream\n";
    assert_eq!(log_stored(&store, &reply).await, expected);
    assert_eq!(reply["bytes"], expected.len());
    assert_eq!(reply["kept_bytes"], log.len());
    assert_eq!(reply["total_bytes"], log.len());
    assert_eq!(reply["complete"], true);
    // The colour codes were removed; the mark and the line-end returns alone would not say so.
    assert_eq!(reply["text_altered"], true);
    assert_eq!(reply["trust"], "untrusted");
    let tail = reply["tail"].as_str().unwrap();
    assert_eq!(tail.lines().count(), 200);
    assert!(tail.starts_with("step 101\n"), "{tail}");
    assert!(expected.ends_with(tail));
    assert!(!reply.to_string().contains(SECRET));

    // Nothing written but the blob: no platform write, no receipt, no ref.
    assert_eq!(f.remote.writes(), writes);
    assert_eq!(state_receipt_count(&f, &f.actor).await, receipts);
    assert_eq!(git(&f.repo, &["show-ref"]), refs);
    // Four reads under the caller's own credential: the repository, the check run, its job and
    // the job's log, kept to the default bound.
    let calls = log_calls(&f);
    let token = blake3::hash(SECRET.as_bytes()).to_hex().to_string();
    assert_eq!(calls.len(), 4, "{calls:?}");
    for (call, path) in calls.iter().zip([
        format!("repos/{SLUG}"),
        format!("repos/{SLUG}/check-runs/7"),
        format!("repos/{SLUG}/actions/jobs/7"),
    ]) {
        assert_eq!(call["method"], "GET", "{call}");
        assert_eq!(call["path"], path);
        assert_eq!(call["token_hash"], token);
    }
    assert_eq!(
        calls[3],
        json!({"op":"download","path":format!("repos/{SLUG}/actions/jobs/7/logs"),
            "limit":8 * 1024 * 1024,"token_hash":token})
    );
}

#[tokio::test]
async fn remote_check_log_keeps_the_end_of_a_long_log_and_says_it_is_partial() {
    let f = Fixture::new(true, None).await;
    let store = log_store(&f);
    f.policy(&f.actor, "git.check_log", "allow").await;
    // The window starts inside the credential: the six bytes of it the window holds are
    // replaced as well.
    let log = format!("head {SECRET}\r\nend\n");
    log_ready(&f, log.as_bytes());
    let _limit = LogLimit::set(12);

    let reply = f
        .call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap();

    assert_eq!(log_stored(&store, &reply).await, "[redacted]\nend\n");
    assert_eq!(reply["tail"], "[redacted]\nend\n");
    assert_eq!(reply["kept_bytes"], 12);
    assert_eq!(reply["total_bytes"], log.len());
    assert_eq!(reply["complete"], false);
    assert_eq!(reply["text_altered"], false);
    assert_eq!(log_calls(&f)[3]["limit"], 12);

    // A log exactly at the bound is whole; one byte under it, the first byte is dropped.
    let log = "0123456789\n";
    log_ready(&f, log.as_bytes());
    drop(_limit);
    let _limit = LogLimit::set(log.len() as u64);
    let reply = f
        .call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap();
    assert_eq!(log_stored(&store, &reply).await, log);
    assert_eq!(
        (&reply["kept_bytes"], &reply["total_bytes"], &reply["complete"]),
        (&json!(log.len()), &json!(log.len()), &json!(true))
    );
    drop(_limit);
    let _limit = LogLimit::set(log.len() as u64 - 1);
    let reply = f
        .call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap();
    assert_eq!(log_stored(&store, &reply).await, &log[1..]);
    assert_eq!(
        (&reply["kept_bytes"], &reply["total_bytes"], &reply["complete"]),
        (&json!(log.len() - 1), &json!(log.len()), &json!(false))
    );

    // A bound past the blob store's own is held to it; zero is held to one byte.
    drop(_limit);
    let _limit = LogLimit::set(u64::MAX);
    f.call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap();
    assert_eq!(
        log_calls(&f).last().unwrap()["limit"],
        khive_storage::MAX_BLOB_WHOLE_BYTES
    );
    drop(_limit);
    let _limit = LogLimit::set(0);
    let reply = f
        .call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap();
    assert_eq!(log_calls(&f).last().unwrap()["limit"], 1);
    assert_eq!(reply["kept_bytes"], 1);
    assert_eq!(reply["complete"], false);
}

#[tokio::test]
async fn remote_check_log_refuses_before_any_platform_read() {
    let f = Fixture::new(true, None).await;
    log_ready(&f, b"log\n");
    let untouched = |f: &Fixture, arm: &str| {
        assert!(log_calls(f).is_empty(), "{arm} reached the platform");
        assert_eq!(log_reads(f), None, "{arm} resolved a credential");
    };

    // No policy row for this verb yet.
    let error = f
        .call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("policy_denied"), "{error}");
    untouched(&f, "policy");

    f.policy(&f.actor, "git.check_log", "allow").await;
    for (arm, params, reason) in [
        ("zero", json!({"check_run":0}), "invalid_params"),
        ("text", json!({"check_run":"7"}), "invalid_params"),
        ("negative", json!({"check_run":-7}), "invalid_params"),
        ("absent", json!({}), "invalid_params"),
        (
            "unknown",
            json!({"check_run":7,"number":1}),
            "unknown field `number`",
        ),
    ] {
        let error = f
            .call(&f.actor, "git.check_log", params)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(reason), "{arm}: {error}");
        untouched(&f, arm);
    }

    // An installation with nowhere to keep the log spends no platform request on it.
    let error = f
        .call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await;
    assert!(error.is_err(), "{error:?}");
    untouched(&f, "blob store");

    // A caller with an allow decision and no actor row has no credential to read with.
    log_store(&f);
    let stranger = format!("{}:stranger", f.actor);
    f.policy(&stranger, "git.check_log", "allow").await;
    let error = f
        .call(&stranger, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("actor_unmapped"), "{error}");
    untouched(&f, "stranger");

    // Control: the mapped caller is answered.
    f.call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap();
    assert_eq!(log_calls(&f).len(), 4);
}

#[tokio::test]
async fn remote_check_log_refuses_an_unlisted_repository_before_policy() {
    let f = Fixture::new(false, None).await;
    log_store(&f);
    f.policy(&f.actor, "git.check_log", "allow").await;
    let checks = crate::local_handlers::policy_check_count(&f.actor);
    let error = f
        .call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not_configured"), "{error}");
    assert_eq!(crate::local_handlers::policy_check_count(&f.actor), checks);
    assert!(log_calls(&f).is_empty());
    assert_eq!(log_reads(&f), None);
}

#[tokio::test]
async fn remote_check_log_reads_only_a_finished_job_of_the_runner() {
    let f = Fixture::new(true, None).await;
    log_store(&f);
    f.policy(&f.actor, "git.check_log", "allow").await;
    type Arm = (&'static str, fn(&mut State), &'static str, usize);
    let arms: [Arm; 12] = [
        (
            "repository under another name",
            |s| s.mismatch = Some("slug"),
            "repository_identity_mismatch",
            1,
        ),
        (
            "repository under another visibility",
            |s| s.mismatch = Some("visibility"),
            "repository_identity_mismatch",
            1,
        ),
        (
            "allowance spent before the first read",
            |s| s.rate_limited = Some(1_795_346_246),
            "remote_rate_limited: the allowance returns at epoch second 1795346246",
            1,
        ),
        (
            "another app",
            |s| s.check_runs = vec![log_run(7, "other-app", "completed")],
            "check_log_unavailable",
            2,
        ),
        (
            "still running",
            |s| s.check_runs = vec![log_run(7, "github-actions", "in_progress")],
            "check_run_incomplete",
            2,
        ),
        (
            "no status",
            |s| {
                let mut run = log_run(7, "github-actions", "completed");
                run.as_object_mut().unwrap().remove("status");
                s.check_runs = vec![run];
            },
            "remote_response",
            2,
        ),
        (
            "unknown check run",
            |s| s.check_runs = vec![],
            "remote_refused",
            2,
        ),
        (
            "job of another check run",
            |s| {
                s.jobs.insert(7, log_job(7, 8));
            },
            "remote_response",
            3,
        ),
        (
            "job under another id",
            |s| {
                s.jobs.insert(7, log_job(9, 7));
            },
            "remote_response",
            3,
        ),
        (
            "log no longer kept",
            |s| s.log_failure = Some(RemoteError::Refused),
            "remote_refused",
            4,
        ),
        (
            "log unreachable",
            |s| s.log_failure = Some(RemoteError::Unavailable),
            "remote_unavailable",
            4,
        ),
        (
            "allowance spent",
            |s| {
                s.log_failure = Some(RemoteError::RateLimited {
                    reset: Some(1_795_346_246),
                })
            },
            "remote_rate_limited: the allowance returns at epoch second 1795346246",
            4,
        ),
    ];
    for (arm, change, reason, reads) in arms {
        log_ready(&f, b"log\n");
        change(&mut f.remote.state.lock().unwrap());
        let before = log_calls(&f).len();
        let error = f
            .call(&f.actor, "git.check_log", json!({"check_run":7}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(reason), "{arm}: {error}");
        let calls = log_calls(&f)[before..].to_vec();
        assert_eq!(calls.len(), reads, "{arm}: {calls:?}");
        assert!(!error.contains(SECRET), "{arm}: {error}");
    }
    // Control: the same call answers once the run and the job are the runner's and finished.
    log_ready(&f, b"log\n");
    let reply = f
        .call(&f.actor, "git.check_log", json!({"check_run":7}))
        .await
        .unwrap();
    assert_eq!(reply["tail"], "log\n");
}
