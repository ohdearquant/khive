use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

use super::{
    create_github_record, create_github_record_with, find_by_number, ingest_prs, link_references,
    read_page_checkpoint, redacted_github_payload, secret_gate, Budget, IngestReport,
    IngestSourceState, NewRecordForRef,
};

const UPDATED: &str = "2026-10-01T00:00:00Z";
const SAFE_CONTENT: &str = "GitHub content was redacted after a secret-gate refusal.";

// Deliberately not key material: only the detector's public header/end shape.
fn synthetic_pem() -> String {
    let bar = "-".repeat(5);
    let label = ["PRIVATE", "KEY"].join(" ");
    format!("{bar}BEGIN {label}{bar}\nsynthetic-not-a-key\n{bar}END {label}{bar}")
}

fn typed_refusal() -> RuntimeError {
    secret_gate::check(&synthetic_pem()).expect_err("synthetic PEM shape must be refused")
}

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    registry: VerbRegistry,
    project: Uuid,
    dir: TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            events_split: None,
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .expect("private in-memory runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(crate::GitPack::new(runtime.clone()));
        builder
            .with_runtime_event_store(&runtime)
            .expect("runtime audit store");
        let registry = builder.build().expect("KG and Git registry");
        runtime.install_edge_rules(registry.all_edge_rules());
        registry.apply_schema_plans(runtime.backend());
        let created = crate::dispatch_from_token(&registry, &token, "create", json!({
            "kind": "entity", "entity_kind": "project", "name": "Git refusal fixture",
            "description": "This private project anchors synthetic GitHub ingest records so refusal recovery can be checked against actual stored notes annotations and durable bounded cursor acknowledgements."
        })).await.expect("create actual project anchor");
        let project =
            Uuid::parse_str(created["id"].as_str().expect("project id")).expect("project UUID");
        Self {
            runtime,
            token,
            registry,
            project,
            dir: tempfile::tempdir().expect("private fixture directory"),
        }
    }

    fn payload(&self, kind: &str, number: u64, created_at: &str) -> Value {
        json!({
            "kind": kind,
            "name": format!("#{number} original remote title"),
            "content": "Original remote content closes #999.",
            "properties": {
                "number": number, "project_id": self.project.to_string(),
                "title": "Original remote title", "author": "remote-contributor",
                "created_at": created_at, "closed_at": null, "merged_at": null,
                "base_ref": "remote-base", "head_ref": "remote-head",
                "labels": ["remote-label"], "state_reason": "completed"
            },
            "annotates": [self.project.to_string()]
        })
    }

    async fn get(&self, id: Uuid) -> Value {
        crate::dispatch_from_token(
            &self.registry,
            &self.token,
            "get",
            json!({"id": id.to_string()}),
        )
        .await
        .expect("read actual stored note")
    }

    async fn assert_annotation(&self, id: Uuid) {
        let neighbors = crate::dispatch_from_token(
            &self.registry,
            &self.token,
            "neighbors",
            json!({
                "id": id.to_string(), "direction": "out"
            }),
        )
        .await
        .expect("read actual note annotations");
        let rows = neighbors.as_array().expect("neighbors array");
        assert!(
            rows.iter()
                .any(|row| row["target_id"] == json!(self.project.to_string())
                    && row["relation"] == "annotates"),
            "stored note must annotate its real project: {neighbors}"
        );
    }

    fn install_pages(&self, rows: &[Value]) {
        for limit in 1..=rows.len().max(1) {
            let prefix = &rows[..limit.min(rows.len())];
            std::fs::write(
                self.dir.path().join(format!("page-{limit}.json")),
                serde_json::to_vec(prefix).unwrap(),
            )
            .unwrap();
        }
        // Only the exact test's child process receives this private PATH. The
        // fake executable has no network calls or grandchildren after exec.
        let program = self.dir.path().join("gh");
        let mut script = String::from("#!/bin/sh\nlimit=1000\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = --limit ]; then shift; limit=\"$1\"; fi\n  shift\ndone\ncase \"$limit\" in\n");
        for limit in 1..=rows.len().max(1) {
            script.push_str(&format!("{limit}) page={limit};;\n"));
        }
        script.push_str(&format!(
            "*) page={};;\nesac\nexec /bin/cat \"$(dirname \"$0\")/page-$page.json\"\n",
            rows.len().max(1)
        ));
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var(
            "PATH",
            format!("{}:/usr/bin:/bin", self.dir.path().display()),
        );
    }

    async fn pass(&self) -> (IngestReport, Vec<NewRecordForRef>, HashMap<u64, Uuid>, bool) {
        let mut report = IngestReport {
            done: true,
            ..IngestReport::default()
        };
        let mut records = Vec::new();
        let mut numbers = HashMap::new();
        let mut merge_shas = HashMap::new();
        let mut budget = Budget { remaining: Some(1) };
        let mut complete = false;
        ingest_prs(
            &self.runtime,
            &self.token,
            &self.registry,
            self.dir.path(),
            "fixture/repository",
            self.project,
            &mut report,
            &mut merge_shas,
            &mut numbers,
            &mut budget,
            &mut records,
            &mut complete,
        )
        .await
        .expect("real PR walker");
        (report, records, numbers, complete)
    }
}

fn remote_pr(number: u64, updated: Option<&str>, refused: bool) -> Value {
    json!({
        "number": number, "title": "Original remote title", "body": "Original remote content closes #999.",
        "author": {"login": "remote-contributor"}, "createdAt": if refused { synthetic_pem() } else { UPDATED.into() },
        "mergedAt": null, "closedAt": null, "updatedAt": updated,
        "baseRefName": "remote-base", "headRefName": "remote-head", "mergeCommit": null
    })
}

fn bounded_output(path: &Path) -> String {
    let mut text = String::new();
    File::open(path)
        .unwrap()
        .take(256 * 1024)
        .read_to_string(&mut text)
        .unwrap();
    text
}

// Re-exec only this exact test in a private HOME and cwd. No ambient store,
// daemon, credentials or process-global PATH mutation reaches the parent.
async fn ran_isolated_child() -> bool {
    let name = std::thread::current()
        .name()
        .expect("libtest name")
        .to_owned();
    if std::env::var("KHIVE_GITHUB_REFUSAL_CHILD").ok().as_deref() == Some(name.as_str()) {
        return false;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let stdout = dir.path().join("stdout");
    let stderr = dir.path().join("stderr");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name.as_str(), "--nocapture", "--test-threads=1"])
        .env_clear()
        .env("HOME", &home)
        .env("TMPDIR", dir.path())
        .env("PATH", "/usr/bin:/bin")
        .env("KHIVE_GITHUB_REFUSAL_CHILD", &name)
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&stdout)
                .unwrap(),
        ))
        .stderr(Stdio::from(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&stderr)
                .unwrap(),
        ));
    if let Ok(mode) = std::env::var("KHIVE_WRITE_QUEUE") {
        if matches!(mode.as_str(), "0" | "1") {
            command.env("KHIVE_WRITE_QUEUE", mode);
        }
    }
    let mut child = command.spawn().expect("spawn private exact test");
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            result => {
                let killed = child.kill();
                let reap_deadline = Instant::now() + Duration::from_secs(5);
                let reaped = loop {
                    let reaped = child.try_wait();
                    if matches!(&reaped, Ok(None)) && Instant::now() < reap_deadline {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        continue;
                    }
                    break reaped;
                };
                panic!("private test did not finish normally: {result:?}, kill={killed:?}, reap={reaped:?}; {} {}",
                    bounded_output(&stdout), bounded_output(&stderr));
            }
        }
    };
    let output = bounded_output(&stdout);
    assert!(
        status.success() && output.contains("1 passed;"),
        "exact child failed: {status}: {output} {}",
        bounded_output(&stderr)
    );
    true
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn pr_properties_refusal_retries_with_safe_stored_payload() {
    if ran_isolated_child().await {
        return;
    }
    let f = Fixture::new().await;
    let healthy = f.payload("pull_request", 10, UPDATED);
    let healthy_result = crate::dispatch_from_token(&f.registry, &f.token, "create", healthy)
        .await
        .expect("ordinary healthy PR create");
    let healthy_id = Uuid::parse_str(healthy_result["id"].as_str().unwrap()).unwrap();
    f.assert_annotation(healthy_id).await;
    let payload = f.payload("pull_request", 11, &synthetic_pem());
    let refusal = crate::dispatch_from_token(&f.registry, &f.token, "create", payload.clone())
        .await
        .expect_err("real post-mask property must refuse");
    match refusal.refusal_source() {
        RuntimeError::SecretDetected(matched) => {
            assert_eq!(matched.detector, "pem-private-key");
            assert!(matched
                .location
                .as_deref()
                .is_some_and(|location| location.ends_with(".properties")));
        }
        other => panic!("expected actual typed scanner refusal, got {other:?}"),
    }
    assert!(
        find_by_number(&f.runtime, &f.token, "pull_request", f.project, 11)
            .await
            .unwrap()
            .is_none()
    );
    let mut report = IngestReport::default();
    let created = create_github_record(
        &f.registry,
        &f.token,
        &mut report,
        "pull_request",
        11,
        f.project,
        payload,
    )
    .await
    .expect("safe replacement create");
    let id = Uuid::parse_str(created.response["id"].as_str().unwrap()).unwrap();
    let stored = f.get(id).await;
    assert_eq!(stored["content"], json!(SAFE_CONTENT));
    assert_eq!(created.content, SAFE_CONTENT);
    assert_eq!(stored["properties"]["created_at"], Value::Null);
    assert_eq!(stored["properties"]["author"], Value::Null);
    assert_eq!(stored["properties"]["head_ref"], Value::Null);
    assert_eq!(
        stored["properties"]["project_id"],
        json!(f.project.to_string())
    );
    f.assert_annotation(id).await;
    assert_eq!(report.writes_refused, 1);
    assert_eq!(report.write_refusals[0].redacted, Some(true));
    let report_text = serde_json::to_string(&report).unwrap();
    assert!(
        !report_text.contains("synthetic-not-a-key") && !report_text.contains("Original remote")
    );
    assert!(!stored.to_string().contains("synthetic-not-a-key"));
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn prs_budget_one_settles_refused_ties_and_undated_across_passes() {
    if ran_isolated_child().await {
        return;
    }
    for updated in [Some(UPDATED), None] {
        let f = Fixture::new().await;
        let rows: Vec<_> = (1..=3)
            .map(|number| remote_pr(number, updated, true))
            .collect();
        f.install_pages(&rows);
        let mut ids = HashSet::new();
        for pass in 1..=4 {
            let (mut report, records, numbers, complete) = f.pass().await;
            assert!(
                !report.cursor_stalled,
                "safe replacement must settle the budget-one boundary"
            );
            if pass <= 3 {
                assert_eq!(report.prs_ingested, 1);
                assert_eq!(report.writes_refused, 1);
                assert_eq!(report.write_refusals[0].redacted, Some(true));
                assert_eq!(records.len(), 1);
                assert!(
                    ids.insert(records[0].id),
                    "fresh passes must create distinct actual records"
                );
                assert_eq!(numbers.len(), pass);
                assert!(!complete && !report.done);
                let stored = f.get(records[0].id).await;
                assert_eq!(
                    records[0].text,
                    stored["content"].as_str().unwrap(),
                    "references must use actual stored replacement content"
                );
                assert_eq!(records[0].text, SAFE_CONTENT);
                f.assert_annotation(records[0].id).await;
                link_references(
                    &f.runtime,
                    &f.token,
                    &f.registry,
                    f.project,
                    &records,
                    &mut report,
                )
                .await;
                assert_eq!(
                    report.reference_edges_unresolved, 0,
                    "rejected original references must not be analyzed"
                );
            } else {
                assert_eq!(report.prs_ingested, 0);
                assert_eq!(report.writes_refused, 0);
                assert!(records.is_empty());
                assert_eq!(numbers.len(), 3);
                assert!(complete && report.done);
                assert!(matches!(
                    report.sources.pull_requests,
                    Some(IngestSourceState::Completed)
                ));
            }
            // A new walker invocation reloads the actual persisted checkpoint;
            // this is not a claim of a cold OS restart.
            let checkpoint =
                read_page_checkpoint(&f.runtime, &f.token, f.project, "prs", &mut Vec::new())
                    .await
                    .unwrap();
            let acknowledged = if updated.is_some() {
                &checkpoint.at_floor
            } else {
                &checkpoint.undated
            };
            assert_eq!(acknowledged.len(), pass.min(3));
            for (number, id) in acknowledged {
                assert_eq!(
                    find_by_number(&f.runtime, &f.token, "pull_request", f.project, *number)
                        .await
                        .unwrap(),
                    Some(*id)
                );
            }
        }
        assert_eq!(
            ids.len(),
            3,
            "the later record and refused tail must both settle"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn issue_typed_contract_refusal_uses_real_replacement_and_acknowledgement() {
    if ran_isolated_child().await {
        return;
    }
    let f = Fixture::new().await;
    let healthy = crate::dispatch_from_token(
        &f.registry,
        &f.token,
        "create",
        f.payload("issue", 20, UPDATED),
    )
    .await
    .expect("healthy ordinary issue create");
    assert!(Uuid::parse_str(healthy["id"].as_str().unwrap()).is_ok());
    let payload = f.payload("issue", 21, UPDATED);
    let fallback = redacted_github_payload("issue", 21, f.project, &payload);
    let calls = Cell::new(0);
    let mut report = IngestReport::default();
    // Explicit contract injection: no ordinary residual issue field is claimed.
    let created =
        create_github_record_with(&mut report, "issue", 21, payload, fallback, |params| {
            let attempt = calls.get();
            calls.set(attempt + 1);
            let registry = &f.registry;
            let token = &f.token;
            async move {
                if attempt == 0 {
                    Err(typed_refusal().before_requested_write())
                } else {
                    crate::dispatch_from_token(registry, token, "create", params).await
                }
            }
        })
        .await
        .expect("one real issue replacement");
    assert_eq!(calls.get(), 2);
    let id = Uuid::parse_str(created.response["id"].as_str().unwrap()).unwrap();
    assert_eq!(f.get(id).await["content"], json!(SAFE_CONTENT));
    f.assert_annotation(id).await;
    assert_eq!(report.write_refusals[0].redacted, Some(true));
    let mut checkpoint = super::PageCheckpoint::new(f.token.namespace().as_str(), None);
    assert!(checkpoint.acknowledge(21, Some(UPDATED), id));
    super::write_page_checkpoint(&f.runtime, f.project, "issues", &checkpoint)
        .await
        .unwrap();
    let loaded = read_page_checkpoint(&f.runtime, &f.token, f.project, "issues", &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(loaded.acknowledged(21, Some(UPDATED)), Some(id));
}

#[tokio::test(flavor = "current_thread")]
async fn typed_contract_failed_retries_never_claim_redaction_or_retry_a_third_time() {
    let project = Uuid::new_v4();
    for second in ["transient", "secret", "malformed"] {
        let payload = json!({"content": "original"});
        let fallback = redacted_github_payload("pull_request", 30, project, &payload);
        let mut report = IngestReport::default();
        let calls = Cell::new(0);
        let outcome =
            create_github_record_with(&mut report, "pull_request", 30, payload, fallback, |_| {
                let attempt = calls.get();
                calls.set(attempt + 1);
                async move {
                    if attempt == 0 {
                        Err(typed_refusal().before_requested_write())
                    } else {
                        match second {
                            "secret" => Err(typed_refusal()),
                            "malformed" => Ok(json!({"id": "not-a-UUID"})),
                            _ => Err(RuntimeError::InvalidInput(
                                "synthetic transport failure".into(),
                            )),
                        }
                    }
                }
            })
            .await;
        assert_eq!(
            calls.get(),
            2,
            "a second error never grants a third attempt"
        );
        assert_eq!(
            report.write_refusals[0].redacted,
            Some(false),
            "failed or malformed retries must not claim redaction"
        );
        assert_eq!(
            report.writes_refused,
            if second == "secret" { 2 } else { 1 }
        );
        if second == "malformed" {
            assert!(outcome.unwrap().response["id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .is_none());
        } else {
            assert!(outcome.is_none());
        }
    }
    let mut report = IngestReport::default();
    let calls = Cell::new(0);
    let outcome = create_github_record_with(
        &mut report,
        "issue",
        31,
        json!({"content":"original"}),
        json!({"content":"fallback"}),
        |_| {
            calls.set(calls.get() + 1);
            async {
                Err(RuntimeError::InvalidInput(
                    "synthetic first transport failure".into(),
                ))
            }
        },
    )
    .await;
    assert!(outcome.is_none());
    assert_eq!(
        calls.get(),
        1,
        "non-secret failures never invoke replacement"
    );
    assert_eq!(report.writes_refused, 0);
    assert!(report.write_refusals.is_empty());
}

#[test]
fn replacement_keeps_only_typed_identity_and_validated_metadata() {
    let project = Uuid::new_v4();
    let original = json!({"kind":"untrusted", "name":"remote", "content":"remote closes #999", "namespace":"remote", "properties":{
        "number":999,"project_id":"remote","title":"remote","author":"remote","labels":["remote"],
        "base_ref":"remote","head_ref":"remote","created_at":synthetic_pem(),
        "closed_at":"2026-10-01T01:00:00+01:00","merged_at":"invalid","state_reason":"COMPLETED","unknown":"remote"
    },"annotates":["remote"]});
    for kind in ["issue", "pull_request"] {
        let value = redacted_github_payload(kind, 40, project, &original);
        assert_eq!(value["kind"], json!(kind));
        assert_eq!(value["annotates"], json!([project.to_string()]));
        assert_eq!(value["properties"]["number"], json!(40));
        assert_eq!(
            value["properties"]["project_id"],
            json!(project.to_string())
        );
        assert_eq!(value["properties"]["created_at"], Value::Null);
        assert_eq!(value["properties"]["closed_at"], json!(UPDATED));
        assert!(value.get("namespace").is_none());
        assert!(value["properties"].get("unknown").is_none());
        assert_eq!(value["content"], json!(SAFE_CONTENT));
        assert!(!value.to_string().contains("remote"));
        if kind == "issue" {
            assert_eq!(value["properties"]["labels"], json!([]));
            assert_eq!(value["properties"]["state_reason"], json!("completed"));
        } else {
            assert_eq!(value["properties"]["merged_at"], Value::Null);
            assert_eq!(value["properties"]["head_ref"], Value::Null);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn missing_create_handler_stalls_and_restored_registry_resumes() {
    if ran_isolated_child().await {
        return;
    }
    let mut f = Fixture::new().await;
    // Exercise an actual unavailable create handler, not a manufactured error
    // or a remote integer that the Git kind hook deliberately accepts.
    let healthy_registry = std::mem::replace(
        &mut f.registry,
        VerbRegistryBuilder::new()
            .build()
            .expect("an empty registry builds without a create handler"),
    );
    assert!(!f.registry.has_verb("create"));
    let missing_create = crate::dispatch_from_token(
        &f.registry,
        &f.token,
        "create",
        f.payload("pull_request", 50, UPDATED),
    )
    .await
    .expect_err("the actual empty registry has no create handler");
    match missing_create.refusal_source() {
        RuntimeError::UnknownVerb(message) => {
            assert_eq!(message, "unknown verb \"create\"; available: ");
        }
        other => panic!("expected the actual missing-create UnknownVerb, got {other:?}"),
    }
    let missing_create_warning = format!("create pull_request #50: {missing_create}");
    f.install_pages(&[remote_pr(50, Some(UPDATED), false)]);
    let (report, records, _, complete) = f.pass().await;
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning == &missing_create_warning),
        "the real walker must report the actual missing-handler refusal: {:?}",
        report.warnings
    );
    assert!(report.cursor_stalled && !report.done && !complete);
    assert!(records.is_empty());
    assert_eq!(
        report.writes_refused, 0,
        "non-secret refusal is not relabeled"
    );
    let checkpoint = read_page_checkpoint(&f.runtime, &f.token, f.project, "prs", &mut Vec::new())
        .await
        .unwrap();
    assert!(
        checkpoint.floor.is_none()
            && checkpoint.at_floor.is_empty()
            && checkpoint.undated.is_empty()
    );
    // Restore only handler availability and retry the identical stored page.
    f.registry = healthy_registry;
    let (report, records, _, _) = f.pass().await;
    assert!(!report.cursor_stalled);
    assert_eq!(report.prs_ingested, 1);
    assert_eq!(records.len(), 1);
    let loaded = read_page_checkpoint(&f.runtime, &f.token, f.project, "prs", &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(loaded.acknowledged(50, Some(UPDATED)), Some(records[0].id));
}
