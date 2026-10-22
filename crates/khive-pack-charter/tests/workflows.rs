use serde_json::{json, Value};

use khive_pack_charter::CharterPack;
use khive_runtime::{
    KhiveRuntime, PackRegistry, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::Pack;

const PR_REVIEW: &str = include_str!("fixtures/pr_review.charter");
const PR_FIX: &str = include_str!("fixtures/pr_fix.charter");
const PR_SPLIT: &str = include_str!("fixtures/pr_split.charter");
const INBOX_TRIAGE: &str = include_str!("fixtures/inbox_triage.charter");
const PR_MERGE: &str = include_str!("fixtures/pr_merge.charter");

const PR: &str = "acme/widgets#7";

struct Fixture {
    _runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: vec![CharterPack::NAME.to_owned()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("isolated runtime without embedding services");
        assert!(!runtime.backend().is_file_backed());
        let mut builder = VerbRegistryBuilder::new();
        PackRegistry::register_packs(
            &[CharterPack::NAME.to_owned()],
            runtime.clone(),
            &mut builder,
        )
        .expect("charter is discovered through its inventory factory");
        let registry = builder.build().expect("charter registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("pack schema installation");
        Self {
            _runtime: runtime,
            registry,
        }
    }

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry
            .dispatch(verb, params.clone())
            .await
            .unwrap_or_else(|e| panic!("{verb} {params}: {e}"))
    }

    async fn refused(&self, verb: &str, params: Value) -> String {
        match self.registry.dispatch(verb, params.clone()).await {
            Ok(v) => panic!("{verb} {params} was accepted: {v}"),
            Err(e) => error_text(&e),
        }
    }

    async fn publish_review_set(&self) {
        for source in [PR_FIX, PR_SPLIT, PR_REVIEW] {
            let out = self
                .call("charter.publish", json!({"source": source}))
                .await;
            assert_eq!(out["published"], true, "{out}");
        }
    }

    async fn start_review(&self) -> String {
        let out = self
            .call(
                "charter.start",
                json!({
                    "charter": "pr_review",
                    "subject_kind": "pull_request",
                    "subject_ref": PR,
                    "bindings": {"author": "alice", "reviewer": "rita", "approver": "ann", "maintainer": "max"},
                }),
            )
            .await;
        assert_eq!(out["started"], true, "{out}");
        out["run_id"].as_str().expect("run id").to_owned()
    }

    async fn observe(&self, kind: &str, outcome: Option<&str>, args: Value) -> Value {
        let mut params =
            json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": kind, "args": args});
        if let Some(outcome) = outcome {
            params["outcome"] = json!(outcome);
        }
        self.call("charter.observe", params).await
    }

    async fn run(&self, run_id: &str) -> Value {
        self.call("charter.get", json!({"run_id": run_id})).await
    }

    /// (run state, current phase, that phase's state, that phase's round)
    async fn at(&self, run_id: &str) -> (String, String, String, i64) {
        let run = self.run(run_id).await;
        let current = run["current_phase"].as_str().unwrap_or_default().to_owned();
        let phase = run["phases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["phase"] == current.as_str())
            .cloned()
            .unwrap_or(Value::Null);
        (
            run["state"].as_str().unwrap().to_owned(),
            current,
            phase["state"].as_str().unwrap_or_default().to_owned(),
            phase["round"].as_i64().unwrap_or(-1),
        )
    }

    async fn child_of(&self, parent: &str, charter: &str) -> Vec<Value> {
        let runs = self
            .call(
                "charter.runs",
                json!({"charter": charter, "subject_ref": PR}),
            )
            .await;
        runs["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["parent_run_id"] == parent)
            .cloned()
            .collect()
    }

    /// Drive the review phase to the given decision with every holding requirement passing.
    async fn decide(&self, decision: &str) {
        self.observe("review.request", Some("done"), json!({}))
            .await;
        self.observe("review.posted", None, json!({})).await;
        self.observe("review.verdict_present", Some("pass"), json!({}))
            .await;
        self.observe("review.reviewer_independent", Some("pass"), json!({}))
            .await;
        self.observe("review.decision", Some(decision), json!({}))
            .await;
    }

    async fn answer_findings(&self) {
        self.observe("fix.push", Some("done"), json!({})).await;
        self.observe("fix.pushed", None, json!({})).await;
        self.observe("pull_request.head_moved", None, json!({}))
            .await;
        self.observe("findings.addressed", Some("pass"), json!({}))
            .await;
    }

    async fn pass_intake(&self) {
        self.observe("pull_request.opened", None, json!({})).await;
        self.observe("pull_request.open", Some("pass"), json!({}))
            .await;
        self.observe("pull_request.base", Some("pass"), json!({"branch": "main"}))
            .await;
    }
}

fn error_text(e: &RuntimeError) -> String {
    format!("{e} {e:?}")
}

fn tuple(state: &str, phase: &str, phase_state: &str, round: i64) -> (String, String, String, i64) {
    (state.into(), phase.into(), phase_state.into(), round)
}

#[tokio::test]
async fn a_charter_that_performs_an_unpublished_charter_is_refused() {
    let f = Fixture::new();
    let err = f
        .refused("charter.publish", json!({"source": PR_REVIEW}))
        .await;
    assert!(err.contains("unpublished_perform"), "{err}");
    f.call("charter.publish", json!({"source": PR_FIX})).await;
    let err = f
        .refused("charter.publish", json!({"source": PR_REVIEW}))
        .await;
    assert!(err.contains("perform pr_split"), "{err}");
    let listed = f.call("charter.list", json!({})).await;
    assert_eq!(listed["charters"].as_array().unwrap().len(), 1, "{listed}");
}

#[tokio::test]
async fn republishing_is_idempotent_and_a_changed_definition_needs_a_new_version() {
    let f = Fixture::new();
    f.call("charter.publish", json!({"source": PR_SPLIT})).await;
    let again = f.call("charter.publish", json!({"source": PR_SPLIT})).await;
    assert_eq!(again["already_published"], true, "{again}");
    let changed = PR_SPLIT.replace("        complete\n", "        terminate \"done\"\n");
    let err = f
        .refused("charter.publish", json!({"source": changed}))
        .await;
    assert!(err.contains("version_conflict"), "{err}");
    let v2 = changed.replace("charter pr_split v1", "charter pr_split v2");
    let out = f.call("charter.publish", json!({"source": v2})).await;
    assert_eq!(out["charter"]["version"], 2, "{out}");
}

#[tokio::test]
async fn an_await_is_released_only_by_a_matching_observation() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "intake", "waiting_trigger", 1)
    );

    f.observe("review.posted", None, json!({})).await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "intake", "waiting_trigger", 1)
    );

    f.observe("pull_request.opened", None, json!({})).await;
    assert_eq!(f.at(&run).await, tuple("open", "intake", "waiting_gate", 1));

    f.observe("pull_request.open", Some("pass"), json!({}))
        .await;
    f.observe(
        "pull_request.base",
        Some("pass"),
        json!({"branch": "develop"}),
    )
    .await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "intake", "waiting_gate", 1),
        "a requirement's arguments must match the observation's"
    );

    f.observe("pull_request.base", Some("pass"), json!({"branch": "main"}))
        .await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "review", "waiting_actor", 1),
        "the review posted during intake is at the current generation, so the first visit counts it and waits on the action"
    );
}

#[tokio::test]
async fn a_failing_hard_requirement_holds_the_phase_until_it_passes() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    f.observe("pull_request.opened", None, json!({})).await;
    f.observe("pull_request.open", Some("fail"), json!({}))
        .await;
    f.observe("pull_request.base", Some("pass"), json!({"branch": "main"}))
        .await;
    assert_eq!(f.at(&run).await, tuple("open", "intake", "waiting_gate", 1));
    let view = f.run(&run).await;
    let intake = &view["phases"][0];
    assert_eq!(
        intake["waiting_on"]["blocked"][0]["require"],
        "pull_request.open"
    );
    f.observe("pull_request.open", Some("pass"), json!({}))
        .await;
    assert_eq!(f.at(&run).await.1, "review");
}

#[tokio::test]
async fn the_waiting_view_answers_who_owes_each_open_phase() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    f.pass_intake().await;

    let rita = f.call("charter.waiting", json!({"actor": "rita"})).await;
    assert_eq!(rita["count"], 1, "{rita}");
    assert_eq!(rita["waiting"][0]["run_id"], run.as_str());
    assert_eq!(rita["waiting"][0]["phase"], "review");
    let reviewer = f.call("charter.waiting", json!({"role": "reviewer"})).await;
    assert_eq!(reviewer["count"], 1, "{reviewer}");
    let alice = f.call("charter.waiting", json!({"actor": "alice"})).await;
    assert_eq!(alice["count"], 1, "the author owes review.request: {alice}");
    assert_eq!(
        f.call("charter.waiting", json!({"role": "approver"})).await["count"],
        0
    );

    f.observe("review.request", Some("done"), json!({})).await;
    assert_eq!(
        f.call("charter.waiting", json!({"actor": "alice"})).await["count"],
        0,
        "once the author's action is recorded the author owes nothing"
    );
    assert_eq!(
        f.call("charter.waiting", json!({"actor": "rita"})).await["count"],
        1
    );
}

#[tokio::test]
async fn a_performed_charter_runs_as_a_child_and_its_outcome_routes_the_parent() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    f.pass_intake().await;
    f.decide("request_changes").await;
    assert_eq!(f.at(&run).await, tuple("open", "fix", "waiting_actor", 1));
    let children = f.child_of(&run, "pr_fix").await;
    assert_eq!(children.len(), 1, "{children:?}");
    let child = children[0]["run_id"].as_str().unwrap().to_owned();
    assert_eq!(
        f.at(&child).await,
        tuple("open", "answer", "waiting_trigger", 1)
    );

    f.answer_findings().await;
    assert_eq!(f.at(&child).await.0, "completed");
    assert_eq!(
        f.at(&run).await,
        tuple("open", "review", "waiting_trigger", 2),
        "the parent re-enters review; round one's observations no longer count"
    );

    f.decide("approve").await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "approval", "waiting_trigger", 1)
    );
}

#[tokio::test]
async fn a_when_on_a_soft_requirement_routes_instead_of_holding() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    f.pass_intake().await;
    f.decide("request_changes").await;
    let child = f.child_of(&run, "pr_fix").await[0]["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    f.observe("fix.push", Some("done"), json!({})).await;
    f.observe("fix.pushed", None, json!({})).await;
    f.observe("pull_request.head_moved", None, json!({})).await;
    f.observe("findings.addressed", Some("fail"), json!({}))
        .await;
    assert_eq!(f.at(&child).await.0, "terminated");
    let (state, phase, _, _) = f.at(&run).await;
    assert_eq!((state.as_str(), phase.as_str()), ("open", "split"));
    assert_eq!(f.child_of(&run, "pr_split").await.len(), 1);
}

#[tokio::test]
async fn cancelling_a_parent_cancels_its_open_children() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    f.pass_intake().await;
    f.decide("request_changes").await;
    let child = f.child_of(&run, "pr_fix").await[0]["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = f
        .call("charter.cancel", json!({"run_id": run, "reason": "closed"}))
        .await;
    assert_eq!(out["cancelled"], run.as_str(), "{out}");
    assert_eq!(f.at(&run).await.0, "cancelled");
    assert_eq!(f.at(&child).await.0, "cancelled");
    assert_eq!(
        f.call("charter.waiting", json!({})).await["count"],
        0,
        "nothing waits once both runs are cancelled"
    );
    let err = f.refused("charter.cancel", json!({"run_id": run})).await;
    assert!(err.contains("not_open"), "{err}");
}

#[tokio::test]
async fn a_bounded_review_loop_takes_its_then_route_and_the_run_terminates() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    f.pass_intake().await;
    for round in 1..=3 {
        assert_eq!(f.at(&run).await.1, "review");
        assert_eq!(f.at(&run).await.3, round);
        f.decide("request_changes").await;
        assert_eq!(f.at(&run).await.1, "fix");
        f.answer_findings().await;
    }
    assert_eq!(
        f.at(&run).await.1,
        "split",
        "a fourth entry to review takes max_rounds' then route"
    );
    let split = f.child_of(&run, "pr_split").await;
    assert_eq!(split.len(), 1);
    f.observe("pull_request.split", Some("done"), json!({}))
        .await;
    assert_eq!(f.at(&run).await.0, "terminated");
    let view = f.run(&run).await;
    let kinds: Vec<&str> = view["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"charter.ended"), "{kinds:?}");
}

#[tokio::test]
async fn a_new_generation_retires_observations_made_against_the_old_one() {
    let f = Fixture::new();
    f.publish_review_set().await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
              "kind": "pull_request.opened", "generation": "h1", "event_id": "opened-h1"}),
    )
    .await;
    let run = f.start_review().await;
    let at = |kind: &str, outcome: Option<&str>, args: Value, generation: &str| {
        let mut p = json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": kind, "args": args, "generation": generation});
        if let Some(o) = outcome {
            p["outcome"] = json!(o);
        }
        p
    };
    f.call(
        "charter.observe",
        at("pull_request.open", Some("pass"), json!({}), "h1"),
    )
    .await;
    f.call(
        "charter.observe",
        at(
            "pull_request.base",
            Some("pass"),
            json!({"branch": "main"}),
            "h1",
        ),
    )
    .await;
    f.call(
        "charter.observe",
        at("pull_request.opened", None, json!({}), "h1"),
    )
    .await;
    f.call(
        "charter.observe",
        at("pull_request.opened", None, json!({}), "h2"),
    )
    .await;
    assert_eq!(f.at(&run).await.0, "superseded");
    let fresh = f.start_review().await;
    f.call(
        "charter.observe",
        at("pull_request.opened", None, json!({}), "h2"),
    )
    .await;
    f.call(
        "charter.observe",
        at("pull_request.open", Some("pass"), json!({}), "h2"),
    )
    .await;
    f.call(
        "charter.observe",
        at(
            "pull_request.base",
            Some("pass"),
            json!({"branch": "main"}),
            "h2",
        ),
    )
    .await;
    assert_eq!(f.at(&fresh).await.1, "review");
}

#[tokio::test]
async fn a_repeated_event_is_recorded_once() {
    let f = Fixture::new();
    f.publish_review_set().await;
    f.start_review().await;
    let params = json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": "pull_request.opened", "event_id": "gh-delivery-1"});
    let first = f.call("charter.observe", params.clone()).await;
    let second = f.call("charter.observe", params).await;
    assert_eq!(first["recorded"][0]["already_recorded"], false, "{first}");
    assert_eq!(second["recorded"][0]["already_recorded"], true, "{second}");
    assert_eq!(
        first["recorded"][0]["sequence"],
        second["recorded"][0]["sequence"]
    );
}

#[tokio::test]
async fn observations_and_starts_are_checked_against_the_registry() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let base =
        |kind: &str| json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": kind});
    let cases = [
        (base("no.such_kind"), "unknown_kind"),
        (base("pull_request"), "wrong_entry_kind"),
        (base("review.decision"), "missing_outcome"),
        (
            {
                let mut p = base("review.decision");
                p["outcome"] = json!("maybe");
                p
            },
            "unknown_outcome",
        ),
        (
            {
                let mut p = base("pull_request.base");
                p["outcome"] = json!("pass");
                p["args"] = json!({"target": "main"});
                p
            },
            "unknown_argument",
        ),
        (
            {
                let mut p = base("pull_request.opened");
                p["producer"] = json!("charter");
                p
            },
            "reserved",
        ),
    ];
    for (params, code) in cases {
        let err = f.refused("charter.observe", params).await;
        assert!(err.contains(code), "{code}: {err}");
    }
    let start = |charter: &str, kind: &str, bindings: Value| json!({"charter": charter, "subject_kind": kind, "subject_ref": PR, "bindings": bindings});
    for (params, code) in [
        (start("nope", "pull_request", json!({})), "not_published"),
        (start("pr_review", "issue", json!({})), "subject_mismatch"),
        (
            start("pr_review", "pull_request", json!({"janitor": "joe"})),
            "unknown_role",
        ),
    ] {
        let err = f.refused("charter.start", params).await;
        assert!(err.contains(code), "{code}: {err}");
    }
    assert_eq!(
        f.call("charter.runs", json!({})).await["count"],
        0,
        "refused starts leave nothing behind"
    );
}

#[tokio::test]
async fn a_second_start_returns_the_open_run() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    let again = f
        .call(
            "charter.start",
            json!({"charter": "pr_review", "subject_kind": "pull_request", "subject_ref": PR}),
        )
        .await;
    assert_eq!(again["started"], false, "{again}");
    assert_eq!(again["run_id"], run.as_str());
}

#[tokio::test]
async fn published_vocabularies_allow_advisory_phrases_but_reject_enforcement_ceilings() {
    let f = Fixture::new();
    for (name, ceiling) in [("hard_gate", "hard"), ("soft_gate", "soft")] {
        let source = format!(
            "vocabulary {name} v1\nphrase {name}.gate \"a deployment gate\":\n    producer instrument\n    ceiling {ceiling}\n"
        );
        let err = f
            .refused("charter.vocabulary", json!({"source": source}))
            .await;
        assert!(err.contains("published_ceiling"), "{err}");
    }

    let vocab = "vocabulary acme v1\nphrase acme.deploy_gate \"the deployment gate passed\":\n    producer instrument\n    ceiling advisory\n";
    let out = f.call("charter.vocabulary", json!({"source": vocab})).await;
    assert_eq!(out["published"], true, "{out}");
    let charter = "charter ship v1\nsubject release\nroles:\n    operator\nworkflow:\n    phase gate by operator:\n        require acme.deploy_gate advisory\n        complete\n";
    let compiled = f.call("charter.compile", json!({"source": charter})).await;
    assert_eq!(compiled["valid"], true, "{compiled}");
    f.call("charter.publish", json!({"source": charter})).await;
    let started = f
        .call(
            "charter.start",
            json!({"charter": "ship", "subject_kind": "release", "subject_ref": "0.15.0", "bindings": {"operator": "olga"}}),
        )
        .await;
    let run = started["run_id"].as_str().unwrap().to_owned();
    f.call(
        "charter.observe",
        json!({"subject_kind": "release", "subject_ref": "0.15.0", "kind": "acme.deploy_gate", "outcome": "pass"}),
    )
    .await;
    assert_eq!(f.at(&run).await.0, "completed");
    let again = f.call("charter.vocabulary", json!({"source": vocab})).await;
    assert_eq!(again["already_published"], true, "{again}");
    let shadow =
        "vocabulary other v1\nphrase acme.deploy_gate \"shadow\":\n    producer instrument\n";
    let err = f
        .refused("charter.vocabulary", json!({"source": shadow}))
        .await;
    assert!(err.contains("entry_conflict"), "{err}");
}

#[tokio::test]
async fn published_vocabularies_cannot_claim_compiled_names_or_entries() {
    let f = Fixture::new();
    for name in ["core", "pr_merge"] {
        let source = format!(
            "vocabulary {name} v2\nphrase acme.shadow \"a shadow phrase\":\n    producer instrument\n"
        );
        let err = f
            .refused("charter.vocabulary", json!({"source": source}))
            .await;
        assert!(err.contains("compiled_vocabulary"), "{err}");
    }
    let shadow = "vocabulary acme v1\nphrase review.decision \"a replacement decision\":\n    producer instrument\n";
    let err = f
        .refused("charter.vocabulary", json!({"source": shadow}))
        .await;
    assert!(err.contains("compiled_entry"), "{err}");
}

#[tokio::test]
async fn compile_reports_refusals_by_line_without_writing() {
    let f = Fixture::new();
    let out = f
        .call(
            "charter.compile",
            json!({"source": "charter x v1\nsubject pull_request\nworkflow:\n    phase a:\n        require no.such hard\n        complete\n"}),
        )
        .await;
    assert_eq!(out["valid"], false);
    assert_eq!(out["diagnostics"][0]["code"], "unknown_phrase");
    assert_eq!(out["diagnostics"][0]["line"], 5);
    assert_eq!(
        f.call("charter.list", json!({})).await["charters"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn a_stop_order_terminates_the_inbox_charter() {
    let f = Fixture::new();
    f.call("charter.publish", json!({"source": INBOX_TRIAGE}))
        .await;
    let started = f
        .call(
            "charter.start",
            json!({"charter": "inbox_triage", "subject_kind": "message", "subject_ref": "inbox:agent", "bindings": {"agent": "agent-1"}}),
        )
        .await;
    let run = started["run_id"].as_str().unwrap().to_owned();
    let obs = |kind: &str, outcome: Option<&str>, args: Value| {
        let mut p = json!({"subject_kind": "message", "subject_ref": "inbox:agent", "kind": kind, "args": args});
        if let Some(o) = outcome {
            p["outcome"] = json!(o);
        }
        p
    };
    f.call("charter.observe", obs("schedule.fired", None, json!({})))
        .await;
    f.call(
        "charter.observe",
        obs("inbox.read", Some("done"), json!({"limit": 20})),
    )
    .await;
    f.call(
        "charter.observe",
        obs("tasks.list_next", Some("done"), json!({})),
    )
    .await;
    f.call(
        "charter.observe",
        obs("message.kind", Some("stop_order"), json!({})),
    )
    .await;
    assert_eq!(f.at(&run).await.1, "halt");
    f.call(
        "charter.observe",
        obs("agents.halt", Some("done"), json!({})),
    )
    .await;
    assert_eq!(f.at(&run).await.0, "terminated");
}

#[tokio::test]
async fn a_failing_requirement_holds_unless_the_matched_when_routes_on_it() {
    let f = Fixture::new();
    let gate = "charter gatecheck v1\nsubject pull_request\nroles:\n    author\nworkflow:\n    phase review:\n        require review.decision hard\n        when review.decision is request_changes: goto fix\n        max_rounds 2 then stop\n        complete\n    phase fix:\n        action fix.push\n        goto review\n    phase stop:\n        terminate \"rounds\"\n";
    f.call("charter.publish", json!({"source": gate})).await;
    let started = f
        .call(
            "charter.start",
            json!({"charter": "gatecheck", "subject_kind": "pull_request", "subject_ref": PR}),
        )
        .await;
    let run = started["run_id"].as_str().unwrap().to_owned();
    f.observe("review.decision", Some("blocked"), json!({}))
        .await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "review", "waiting_gate", 1),
        "an outcome no when routes on holds a hard requirement"
    );
    f.observe("review.decision", Some("request_changes"), json!({}))
        .await;
    assert_eq!(
        f.at(&run).await.1,
        "fix",
        "the matched when routes on the phrase"
    );
    f.observe("fix.push", Some("done"), json!({})).await;
    f.observe("review.decision", Some("approve"), json!({}))
        .await;
    assert_eq!(f.at(&run).await.0, "completed");

    f.publish_review_set().await;
    let other = "acme/widgets#8";
    let review = f
        .call(
            "charter.start",
            json!({"charter": "pr_review", "subject_kind": "pull_request", "subject_ref": other}),
        )
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let on = |kind: &str, outcome: Option<&str>, args: Value| {
        let mut p = json!({"subject_kind": "pull_request", "subject_ref": other, "kind": kind, "args": args});
        if let Some(o) = outcome {
            p["outcome"] = json!(o);
        }
        p
    };
    for (kind, outcome, args) in [
        ("pull_request.opened", None, json!({})),
        ("pull_request.open", Some("pass"), json!({})),
        ("pull_request.base", Some("pass"), json!({"branch": "main"})),
        ("review.request", Some("done"), json!({})),
        ("review.posted", None, json!({})),
        ("review.verdict_present", Some("fail"), json!({})),
        ("review.reviewer_independent", Some("pass"), json!({})),
        ("review.decision", Some("approve"), json!({})),
    ] {
        f.call("charter.observe", on(kind, outcome, args)).await;
    }
    assert_eq!(
        f.at(&review).await,
        tuple("open", "review", "waiting_gate", 1),
        "a when on review.decision does not release a failing review.verdict_present"
    );
}

#[tokio::test]
async fn a_new_generation_supersedes_a_run_during_a_phase_loop() {
    let f = Fixture::new();
    let source = "charter gate_loop v1\nsubject pull_request\nroles:\n    author\nworkflow:\n    phase gate:\n        require gate.result(name=box) hard\n        when gate.result(name=box) is fail: goto repair\n        max_rounds 3 then give_up\n        goto done\n    phase repair:\n        action fix.push\n        goto gate\n    phase done:\n        complete\n    phase give_up:\n        terminate \"three gate rounds without a pass\"\n";
    let out = f.call("charter.publish", json!({"source": source})).await;
    assert_eq!(out["published"], true, "{out}");
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
              "kind": "pull_request.opened", "generation": "h1", "event_id": "opened-h1"}),
    )
    .await;
    let run = f
        .call(
            "charter.start",
            json!({"charter": "gate_loop", "subject_kind": "pull_request", "subject_ref": PR, "generation": "h1"}),
        )
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let at = |kind: &str, outcome: &str, args: Value, generation: &str| {
        json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": kind,
               "outcome": outcome, "args": args, "generation": generation})
    };
    f.call(
        "charter.observe",
        at("gate.result", "fail", json!({"name": "box"}), "h1"),
    )
    .await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "repair", "waiting_actor", 1)
    );
    f.call(
        "charter.observe",
        at("pull_request.head_moved", "observed", json!({}), "h2"),
    )
    .await;
    assert_eq!(f.at(&run).await.0, "superseded");
    let fresh = f
        .call(
            "charter.start",
            json!({"charter": "gate_loop", "subject_kind": "pull_request",
                  "subject_ref": PR, "generation": "h2"}),
        )
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    f.call(
        "charter.observe",
        at("gate.result", "pass", json!({"name": "box"}), "h2"),
    )
    .await;
    assert_eq!(f.at(&fresh).await.0, "completed");
}

#[tokio::test]
async fn a_loop_on_an_unchanged_head_reads_its_gate_result_again_on_the_revisit() {
    // A fix that does not move the generation (a comment-only answer, say) sends the run back to review on the same
    // head. The revisit reads only observations recorded after the previous visit ended, so the gate result recorded
    // in the first visit is not read again, although nothing it describes has changed: it has to be observed again.
    let f = Fixture::new();
    let source = "charter same_head_loop v1\nsubject pull_request\nroles:\n    author\nworkflow:\n    phase review:\n        require gate.result(name=box) hard\n        when review.decision is request_changes: goto fix\n        max_rounds 3 then give_up\n        goto done\n    phase fix:\n        action fix.push\n        goto review\n    phase done:\n        complete\n    phase give_up:\n        terminate \"three review rounds without an approval\"\n";
    let out = f.call("charter.publish", json!({"source": source})).await;
    assert_eq!(out["published"], true, "{out}");
    let run = f
        .call(
            "charter.start",
            json!({"charter": "same_head_loop", "subject_kind": "pull_request", "subject_ref": PR, "generation": "h1"}),
        )
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let at = |kind: &str, outcome: &str, args: Value| {
        json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": kind,
               "outcome": outcome, "args": args, "generation": "h1"})
    };
    let pending = |run: &Value| -> Vec<String> {
        let phase = run["phases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["phase"] == run["current_phase"])
            .cloned()
            .unwrap_or(Value::Null);
        phase["waiting_on"]["pending"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|p| {
                        p["require"]
                            .as_str()
                            .or(p["when"].as_str())
                            .unwrap_or_default()
                            .to_owned()
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    f.call(
        "charter.observe",
        at("gate.result", "pass", json!({"name": "box"})),
    )
    .await;
    f.call(
        "charter.observe",
        at("review.decision", "request_changes", json!({})),
    )
    .await;
    assert_eq!(f.at(&run).await, tuple("open", "fix", "waiting_actor", 1));
    f.call("charter.observe", at("fix.push", "done", json!({})))
        .await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "review", "waiting_gate", 2),
        "the first visit's decision does not send the loop round again"
    );

    f.call(
        "charter.observe",
        at("review.decision", "approve", json!({})),
    )
    .await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "review", "waiting_gate", 2),
        "the gate result from the first visit is not read on the same head"
    );
    assert_eq!(pending(&f.run(&run).await), vec!["gate.result".to_owned()]);

    f.call(
        "charter.observe",
        at("gate.result", "pass", json!({"name": "box"})),
    )
    .await;
    assert_eq!(f.at(&run).await.0, "completed");
}

#[tokio::test]
async fn a_late_observation_for_an_older_head_never_moves_the_subject_back() {
    let f = Fixture::new();
    f.publish_review_set().await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
              "kind": "pull_request.opened", "generation": "h1", "event_id": "opened-h1"}),
    )
    .await;
    let run = f.start_review().await;
    let at = |kind: &str, outcome: &str, args: Value, generation: &str| {
        json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": kind,
               "outcome": outcome, "args": args, "generation": generation})
    };
    f.call(
        "charter.observe",
        at("pull_request.opened", "observed", json!({}), "h1"),
    )
    .await;
    f.call(
        "charter.observe",
        at("pull_request.head_moved", "observed", json!({}), "h2"),
    )
    .await;
    f.call(
        "charter.observe",
        at("pull_request.open", "pass", json!({}), "h1"),
    )
    .await;
    f.call(
        "charter.observe",
        at("pull_request.base", "pass", json!({"branch": "main"}), "h1"),
    )
    .await;
    assert_eq!(f.at(&run).await.0, "superseded");
    let fresh = f.start_review().await;
    f.call(
        "charter.observe",
        at("pull_request.opened", "observed", json!({}), "h2"),
    )
    .await;
    f.call(
        "charter.observe",
        at("pull_request.open", "pass", json!({}), "h2"),
    )
    .await;
    f.call(
        "charter.observe",
        at("pull_request.base", "pass", json!({"branch": "main"}), "h2"),
    )
    .await;
    assert_eq!(f.at(&fresh).await.1, "review");
}

#[tokio::test]
async fn a_head_that_returns_to_an_earlier_candidate_counts_none_of_its_old_results() {
    let f = Fixture::new();
    f.publish_review_set().await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
              "kind": "pull_request.opened", "generation": "h1", "event_id": "opened"}),
    )
    .await;
    let run = f.start_review().await;
    let at = |kind: &str, outcome: &str, args: Value, generation: &str, event: &str| {
        json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": kind,
               "outcome": outcome, "args": args, "generation": generation, "event_id": event})
    };
    for (kind, generation, event) in [("pull_request.head_moved", "h2", "moved-h2")] {
        f.call(
            "charter.observe",
            at(kind, "observed", json!({}), generation, event),
        )
        .await;
    }
    assert_eq!(f.at(&run).await.0, "superseded");
    let h2_run = f.start_review().await;
    f.call(
        "charter.observe",
        at(
            "pull_request.opened",
            "observed",
            json!({}),
            "h2",
            "opened-h2-again",
        ),
    )
    .await;
    let first = f
        .call(
            "charter.observe",
            at("pull_request.open", "pass", json!({}), "h2", "open"),
        )
        .await;
    assert_eq!(first["recorded"][0]["already_recorded"], false, "{first}");
    assert_eq!(f.at(&h2_run).await.0, "open");
    f.call(
        "charter.observe",
        at(
            "pull_request.head_moved",
            "observed",
            json!({}),
            "h1",
            "moved-h1",
        ),
    )
    .await;
    assert_eq!(f.at(&h2_run).await.0, "superseded");
    f.call(
        "charter.observe",
        at(
            "pull_request.head_moved",
            "observed",
            json!({}),
            "h2",
            "moved-h2-again",
        ),
    )
    .await;
    let latest = f.start_review().await;
    f.call(
        "charter.observe",
        at(
            "pull_request.opened",
            "observed",
            json!({}),
            "h2",
            "opened-h2-latest",
        ),
    )
    .await;
    f.call(
        "charter.observe",
        at(
            "pull_request.base",
            "pass",
            json!({"branch": "main"}),
            "h2",
            "base",
        ),
    )
    .await;
    assert_eq!(
        f.at(&latest).await,
        tuple("open", "intake", "waiting_gate", 1),
        "the head went h2, h1, h2: the open result from the first h2 belongs to an earlier epoch"
    );
    let pending = f.run(&latest).await["phases"][0]["waiting_on"]["pending"].to_string();
    assert!(
        pending.contains("pull_request.open") && !pending.contains("pull_request.base"),
        "{pending}"
    );
    let again = f
        .call(
            "charter.observe",
            at("pull_request.open", "pass", json!({}), "h2", "open"),
        )
        .await;
    assert_eq!(
        again["recorded"][0]["already_recorded"], false,
        "the same event id at a new epoch is a new observation: {again}"
    );
    assert_eq!(f.at(&latest).await.1, "review");
}

#[tokio::test]
async fn a_request_id_runs_a_command_once_and_refuses_a_different_request() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let start = json!({
        "charter": "pr_review", "subject_kind": "pull_request", "subject_ref": PR,
        "bindings": {"author": "alice", "reviewer": "rita", "approver": "ann", "maintainer": "max"},
        "request_id": "start-1",
    });
    let first = f.call("charter.start", start.clone()).await;
    assert_eq!(first["started"], true, "{first}");
    let again = f.call("charter.start", start.clone()).await;
    assert_eq!(again["replayed"], true, "{again}");
    assert_eq!(again["run_id"], first["run_id"]);
    let mut explicit = start;
    explicit["domain"] = json!("default");
    let same = f.call("charter.start", explicit).await;
    assert_eq!(
        same["replayed"], true,
        "naming the default domain is the same request: {same}"
    );
    let run = first["run_id"].as_str().unwrap().to_owned();

    let opened = |request_id: &str| {
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "pull_request.opened", "request_id": request_id})
    };
    let sequence = |out: &Value| out["recorded"][0]["sequence"].as_i64().unwrap();
    let a = f.call("charter.observe", opened("observe-1")).await;
    let b = f.call("charter.observe", opened("observe-1")).await;
    assert_eq!(b["replayed"], true, "{b}");
    assert_eq!(b["recorded"], a["recorded"]);
    let c = f.call("charter.observe", opened("observe-2")).await;
    assert_eq!(
        sequence(&c),
        sequence(&a) + 1,
        "the replay wrote no evidence row; a new key without an event id writes one"
    );

    let err = f
        .refused(
            "charter.observe",
            json!({"subject_kind": "pull_request", "subject_ref": PR,
                   "kind": "review.posted", "request_id": "observe-1"}),
        )
        .await;
    assert!(err.contains("request_id_reused"), "{err}");
    let err = f
        .refused(
            "charter.cancel",
            json!({"run_id": run, "request_id": "start-1"}),
        )
        .await;
    assert!(err.contains("request_id_reused"), "{err}");
    assert_eq!(
        f.at(&run).await.0,
        "open",
        "a refused request changes nothing"
    );
    let cancelled = f
        .call(
            "charter.cancel",
            json!({"run_id": run, "request_id": "cancel-1"}),
        )
        .await;
    assert_eq!(cancelled["cancelled"], run.as_str());
    let replayed = f
        .call(
            "charter.cancel",
            json!({"run_id": run, "request_id": "cancel-1"}),
        )
        .await;
    assert_eq!(
        replayed["replayed"], true,
        "a retried cancel returns its receipt instead of refusing a closed run: {replayed}"
    );
    let vocab = |name: &str| {
        json!({"source": format!("vocabulary {name} v1\nphrase {name}.gate \"a gate\":\n    producer instrument\n"),
               "request_id": "vocab-1"})
    };
    f.call("charter.vocabulary", vocab("acme")).await;
    let err = f.refused("charter.vocabulary", vocab("other")).await;
    assert!(err.contains("request_id_reused"), "{err}");
}

/// The `pr_merge/v1` definition block of ADR-193, read from the record so the comparison has no
/// second copy of the template to drift.
fn pr_merge_template() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/adr/ADR-193-charter-runs.md"
    );
    let adr = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let (_, section) = adr
        .split_once("## `pr_merge/v1`")
        .expect("ADR-193 has a pr_merge/v1 section");
    let (_, block) = section
        .split_once("```json\n")
        .expect("the section carries the definition as JSON");
    let (block, _) = block.split_once("\n```").expect("the JSON block is closed");
    serde_json::from_str(block).expect("the template block parses")
}

#[tokio::test]
async fn the_pr_merge_charter_carries_the_template_gates_floor_and_levels() {
    let f = Fixture::new();
    let out = f.call("charter.compile", json!({"source": PR_MERGE})).await;
    assert_eq!(out["valid"], true, "{out}");
    let phases = out["definition"]["phases"].as_array().unwrap();
    let template = pr_merge_template();
    let expected = template["phases"].as_array().unwrap();
    assert_eq!(
        phases.iter().map(|p| &p["id"]).collect::<Vec<_>>(),
        expected.iter().map(|p| &p["id"]).collect::<Vec<_>>()
    );
    for (i, (phase, want)) in phases.iter().zip(expected).enumerate() {
        let id = &want["id"];
        assert_eq!(phase["owner"], want["assignee"]["role"], "{id}");
        let requires = phase["requires"].as_array().unwrap();
        for r in requires {
            assert_eq!(r["level"], "hard", "{id}: {r}");
        }
        let gates: std::collections::BTreeSet<String> = requires
            .iter()
            .map(|r| r["phrase"].as_str().unwrap())
            .filter(|p| *p != "pr_merge.floor_check")
            .map(|p| format!("{}/v1", p.strip_prefix("pr_merge.").unwrap()))
            .collect();
        let want_gates: std::collections::BTreeSet<String> = want["gates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(gates, want_gates, "{id}");
        if want["await"] == "predecessor_completed" {
            assert_eq!(phase["awaits"], json!([]), "{id}");
            assert_eq!(
                phases[i - 1]["next"],
                json!({"kind": "goto", "phase": id}),
                "{id} follows its predecessor"
            );
        } else {
            let trigger = format!("pr_merge.{}", want["await"].as_str().unwrap());
            assert_eq!(phase["awaits"], json!([[trigger]]), "{id}");
        }
    }

    let policy = &template["check_policy"];
    let validate = phases[0]["requires"].as_array().unwrap();
    let floor: Vec<&Value> = validate
        .iter()
        .filter(|r| r["phrase"] == "pr_merge.floor_check")
        .map(|r| &r["args"]["name"])
        .collect();
    let want_floor: Vec<&Value> = policy["floor"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["name"])
        .collect();
    assert_eq!(floor, want_floor);
    let checks = validate
        .iter()
        .find(|r| r["phrase"] == "pr_merge.required_checks")
        .unwrap();
    assert_eq!(checks["args"]["event"], policy["allowed_event"]);
    assert_eq!(checks["args"]["tested_revision"], policy["tested_revision"]);
    let self_approval = phases[1]["requires"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["phrase"] == "pr_merge.no_self_approval")
        .unwrap();
    assert_eq!(
        self_approval["args"]["controller"],
        template["independence"]["controller"]
    );

    let merge = phases.last().unwrap();
    let action = &expected.last().unwrap()["action"];
    assert_eq!(
        merge["actions"],
        json!([{
            "name": format!("pr_merge.{}", action["kind"].as_str().unwrap()),
            "args": {"method": action["method"], "expected_head": action["expected_head"]},
            "by": template["principals"]["merge_initiator_role"],
            "passes": ["done", "skipped"],
        }])
    );
    assert_eq!(merge["next"], json!({"kind": "complete"}));
}

#[tokio::test]
async fn evidence_source_identity_rejects_changed_payloads() {
    let f = Fixture::new();
    f.publish_review_set().await;
    f.start_review().await;

    let observation = |outcome: &str, request_id: &str| {
        json!({
            "subject_kind": "pull_request",
            "subject_ref": PR,
            "kind": "pull_request.open",
            "outcome": outcome,
            "event_id": "stable-source-event",
            "request_id": request_id,
        })
    };
    f.call("charter.observe", observation("pass", "first-request"))
        .await;

    let error = f
        .refused("charter.observe", observation("fail", "changed-request"))
        .await;
    assert!(error.contains("EvidenceConflict"), "{error}");
}

#[tokio::test]
async fn old_generation_evidence_without_generation_metadata_is_not_reused() {
    let f = Fixture::new();
    f.publish_review_set().await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
              "kind": "pull_request.opened", "generation": "h1", "event_id": "opened-h1"}),
    )
    .await;
    let run = f.start_review().await;
    for (kind, outcome, args) in [
        ("pull_request.open", "pass", json!({})),
        ("pull_request.base", "pass", json!({"branch": "main"})),
    ] {
        f.call(
            "charter.observe",
            json!({"subject_kind": "pull_request", "subject_ref": PR,
                   "kind": kind, "outcome": outcome, "args": args}),
        )
        .await;
    }
    for kind in ["pull_request.head_moved", "pull_request.opened"] {
        f.call(
            "charter.observe",
            json!({"subject_kind": "pull_request", "subject_ref": PR,
                   "kind": kind, "generation": "h2"}),
        )
        .await;
    }

    assert_eq!(f.at(&run).await.0, "superseded");
    let fresh = f.start_review().await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
              "kind": "pull_request.opened", "generation": "h2"}),
    )
    .await;
    assert_eq!(
        f.at(&fresh).await,
        tuple("open", "intake", "waiting_gate", 1),
        "generation-less evidence from epoch one cannot satisfy h2 gates"
    );
    for (kind, outcome, args) in [
        ("pull_request.open", "pass", json!({})),
        ("pull_request.base", "pass", json!({"branch": "main"})),
    ] {
        f.call(
            "charter.observe",
            json!({"subject_kind": "pull_request", "subject_ref": PR,
                   "kind": kind, "outcome": outcome, "args": args, "generation": "h2"}),
        )
        .await;
    }
    assert_eq!(f.at(&fresh).await.1, "review");
}

#[tokio::test]
async fn child_terminal_observation_carries_its_generation() {
    let f = Fixture::new();
    let child = r#"charter child_flow v1
subject pull_request
roles:
    agent "the person doing the work"
workflow:
    phase work by agent:
        action work.unit by agent
        complete
"#;
    let parent = r#"charter parent_flow v1
subject pull_request
roles:
    agent "the person doing the work"
workflow:
    phase coordinate:
        perform child_flow
        await pull_request.head_moved
        complete
"#;
    f.call("charter.publish", json!({"source": child})).await;
    f.call("charter.publish", json!({"source": parent})).await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
              "kind": "pull_request.opened", "generation": "h1", "event_id": "opened-h1"}),
    )
    .await;
    let started = f
        .call(
            "charter.start",
            json!({"charter": "parent_flow", "subject_kind": "pull_request",
                   "subject_ref": PR, "generation": "h1", "bindings": {"agent": "worker"}}),
        )
        .await;
    let run = started["run_id"].as_str().unwrap().to_owned();
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "work.unit", "outcome": "done", "generation": "h1"}),
    )
    .await;
    assert_eq!(
        f.child_of(&run, "child_flow").await[0]["state"],
        "completed"
    );

    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "pull_request.head_moved", "generation": "h2"}),
    )
    .await;
    assert_eq!(f.at(&run).await.0, "superseded");
    let fresh = f
        .call(
            "charter.start",
            json!({"charter": "parent_flow", "subject_kind": "pull_request",
                  "subject_ref": PR, "generation": "h2", "bindings": {"agent": "worker"}}),
        )
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(f.at(&fresh).await.0, "open");
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
              "kind": "pull_request.head_moved", "generation": "h2", "event_id": "moved-h2-again"}),
    )
    .await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
              "kind": "work.unit", "outcome": "done", "generation": "h2", "event_id": "work-h2"}),
    )
    .await;
    assert_eq!(f.at(&fresh).await.0, "completed");
}

#[tokio::test]
async fn replayed_generation_trigger_does_not_restore_old_subject_generation() {
    let f = Fixture::new();
    f.publish_review_set().await;
    f.start_review().await;
    let trigger = |generation: &str, event_id: &str| {
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "pull_request.head_moved", "generation": generation,
               "event_id": event_id})
    };
    f.call("charter.observe", trigger("h1", "event-h1")).await;
    f.call("charter.observe", trigger("h2", "event-h2")).await;
    f.call("charter.observe", trigger("h1", "event-h1")).await;

    let next = f
        .call(
            "charter.start",
            json!({"charter": "pr_review", "subject_kind": "pull_request",
                   "subject_ref": PR, "generation": "h2"}),
        )
        .await;
    assert_eq!(next["started"], true, "{next}");
    assert_eq!(next["run"]["candidate"]["generation"], "h2");
    assert_eq!(next["run"]["subject_epoch"], 2);
    let run = next["run_id"].as_str().unwrap();
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "pull_request.open", "outcome": "pass", "generation": "h2"}),
    )
    .await;
    f.call("charter.observe", trigger("h2", "event-open-h2"))
        .await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "pull_request.base", "outcome": "pass",
               "args": {"branch": "main"}, "generation": "h2"}),
    )
    .await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "pull_request.opened", "generation": "h2"}),
    )
    .await;

    assert_eq!(f.at(run).await.1, "review");
}

#[tokio::test]
async fn starting_another_charter_cannot_change_an_existing_subject_generation() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let first = f
        .call(
            "charter.start",
            json!({"charter": "pr_review", "subject_kind": "pull_request",
                   "subject_ref": PR, "generation": "h1"}),
        )
        .await;
    let run = first["run_id"].as_str().unwrap().to_owned();
    let error = f
        .refused(
            "charter.start",
            json!({"charter": "pr_fix", "subject_kind": "pull_request",
                   "subject_ref": PR, "generation": "h2"}),
        )
        .await;
    assert!(error.contains("generation_mismatch"), "{error}");
    assert_eq!(f.run(&run).await["subject_epoch"], 0);
}

#[tokio::test]
async fn a_new_generation_supersedes_the_open_run_and_allows_a_fresh_start() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let first = f
        .call(
            "charter.start",
            json!({"charter": "pr_review", "subject_kind": "pull_request",
                   "subject_ref": PR, "generation": "h1"}),
        )
        .await;
    let old_run = first["run_id"].as_str().unwrap().to_owned();
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "pull_request.head_moved", "generation": "h2",
               "event_id": "head-h2"}),
    )
    .await;
    assert_eq!(f.run(&old_run).await["state"], "superseded");

    let next = f
        .call(
            "charter.start",
            json!({"charter": "pr_review", "subject_kind": "pull_request",
                   "subject_ref": PR, "generation": "h2"}),
        )
        .await;
    assert_eq!(next["started"], true, "{next}");
    assert_ne!(next["run_id"], old_run);
    assert_eq!(next["run"]["subject_epoch"], 1);
    assert_eq!(next["run"]["candidate"]["generation"], "h2");
}

#[tokio::test]
async fn a_perform_waits_for_the_child_started_on_its_phase_visit() {
    let f = Fixture::new();
    let child = r#"charter child_flow v1
subject pull_request
roles:
    agent "the person doing the work"
workflow:
    phase work by agent:
        action work.unit by agent
        complete
"#;
    let parent = r#"charter parent_flow v1
subject pull_request
roles:
    agent "the person doing the work"
workflow:
    phase first by agent:
        perform child_flow
        await message.arrived
        goto second
    phase second by agent:
        perform child_flow
        await prompt.received
        complete
"#;
    f.call("charter.publish", json!({"source": child})).await;
    f.call("charter.publish", json!({"source": parent})).await;
    let started = f
        .call(
            "charter.start",
            json!({"charter": "parent_flow", "subject_kind": "pull_request",
                   "subject_ref": PR, "generation": "h1", "bindings": {"agent": "worker"}}),
        )
        .await;
    let run = started["run_id"].as_str().unwrap().to_owned();
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "work.unit", "outcome": "done", "generation": "h1"}),
    )
    .await;
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "message.arrived", "generation": "h1"}),
    )
    .await;
    let children = f.child_of(&run, "child_flow").await;
    assert_eq!(children.len(), 2, "{children:?}");
    assert!(children.iter().any(|child| child["state"] == "completed"));
    assert!(children.iter().any(|child| child["state"] == "open"));
    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR,
               "kind": "prompt.received", "generation": "h1"}),
    )
    .await;

    assert_eq!(
        f.at(&run).await,
        tuple("open", "second", "waiting_actor", 1)
    );
    let children = f.child_of(&run, "child_flow").await;
    assert!(
        children.iter().any(|child| child["state"] == "open"),
        "{children:?}"
    );
}

#[tokio::test]
async fn publication_refuses_perform_cycles_across_versions() {
    let f = Fixture::new();
    let a_v1 =
        "charter a_cycle v1\nsubject pull_request\nworkflow:\n    phase done:\n        complete\n";
    let b_v1 = "charter b_cycle v1\nsubject pull_request\nworkflow:\n    phase child:\n        perform a_cycle\n        complete\n";
    let a_v2 = "charter a_cycle v2\nsubject pull_request\nworkflow:\n    phase child:\n        perform b_cycle\n        complete\n";
    f.call("charter.publish", json!({"source": a_v1})).await;
    f.call("charter.publish", json!({"source": b_v1})).await;

    let error = f.refused("charter.publish", json!({"source": a_v2})).await;
    assert!(error.contains("perform_cycle"), "{error}");
}

#[tokio::test]
async fn child_start_refuses_a_cycle_in_persisted_definitions() {
    let f = Fixture::new();
    let a =
        "charter a_cycle v1\nsubject pull_request\nworkflow:\n    phase done:\n        complete\n";
    let b = "charter b_cycle v1\nsubject pull_request\nworkflow:\n    phase child:\n        perform a_cycle\n        complete\n";
    f.call("charter.publish", json!({"source": a})).await;
    f.call("charter.publish", json!({"source": b})).await;

    let mut writer = f._runtime.sql().writer().await.unwrap();
    writer
        .execute_script("DROP TRIGGER charter_definitions_no_update".to_owned())
        .await
        .unwrap();
    let row = writer
        .query_row(khive_storage::SqlStatement::new(
            "SELECT definition_bytes FROM charter_definitions WHERE charter_id = 'a_cycle'",
            vec![],
        ))
        .await
        .unwrap()
        .unwrap();
    let bytes = match row.get("definition_bytes") {
        Some(khive_storage::SqlValue::Blob(bytes)) => bytes.clone(),
        other => panic!("expected definition bytes, got {other:?}"),
    };
    let mut definition: Value = serde_json::from_slice(&bytes).unwrap();
    definition["phases"][0]["performs"] = json!(["b_cycle"]);
    let bytes = serde_json::to_vec(&definition).unwrap();
    writer
        .execute(khive_storage::SqlStatement::new(
            "UPDATE charter_definitions SET definition_bytes = ?1 WHERE charter_id = 'a_cycle'",
            vec![khive_storage::SqlValue::Blob(bytes)],
        ))
        .await
        .unwrap();
    drop(writer);

    let error = f
        .registry
        .dispatch(
            "charter.start",
            json!({"charter": "a_cycle", "subject_kind": "pull_request", "subject_ref": PR}),
        )
        .await
        .expect_err("an ancestry cycle must stop before another child starts");
    assert!(
        format!("{error} {error:?}").contains("perform_cycle"),
        "{error:?}"
    );
}

#[tokio::test]
async fn observations_use_the_registry_pinned_by_the_target_run() {
    let f = Fixture::new();
    let first_vocabulary = "vocabulary acme v1\nphrase acme.result \"the result was recorded\":\n    outcomes pass fail\n    passes pass\n    ceiling hard\n";
    let second_vocabulary = "vocabulary acme v2\nphrase acme.result \"the result was recorded\":\n    outcomes green red\n    passes green\n    ceiling hard\n";
    let charter = "charter pinned_result v1\nsubject pull_request\nworkflow:\n    phase gate:\n        require acme.result hard\n        complete\n";
    f.call("charter.vocabulary", json!({"source": first_vocabulary}))
        .await;
    f.call("charter.publish", json!({"source": charter})).await;
    let started = f
        .call(
            "charter.start",
            json!({"charter": "pinned_result", "subject_kind": "pull_request", "subject_ref": PR}),
        )
        .await;
    let run = started["run_id"].as_str().unwrap();
    f.call("charter.vocabulary", json!({"source": second_vocabulary}))
        .await;

    f.call(
        "charter.observe",
        json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": "acme.result", "outcome": "pass"}),
    )
    .await;
    assert_eq!(f.run(run).await["state"], "completed");
}

#[tokio::test]
async fn observation_arguments_require_declared_fields_and_types() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    let missing = f
        .refused(
            "charter.observe",
            json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": "pull_request.base", "outcome": "pass", "args": {}}),
        )
        .await;
    assert!(missing.contains("missing_argument"), "{missing}");

    let wrong_type = f
        .refused(
            "charter.observe",
            json!({"subject_kind": "pull_request", "subject_ref": PR, "kind": "pull_request.base", "outcome": "pass", "args": {"branch": 123}}),
        )
        .await;
    assert!(wrong_type.contains("argument_type"), "{wrong_type}");

    let mut reader = f._runtime.sql().reader().await.unwrap();
    let count = reader
        .count(khive_storage::SqlStatement::new(
            "SELECT COUNT(*) FROM charter_evidence WHERE run_id = ?1 AND kind = 'pull_request.base'",
            vec![khive_storage::SqlValue::Text(run)],
        ))
        .await
        .unwrap();
    assert_eq!(count, 0, "invalid arguments must leave no evidence");
}
