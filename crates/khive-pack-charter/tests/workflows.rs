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
const PR_MERGE_VOCABULARY: &str = include_str!("fixtures/pr_merge.vocab");

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
        at("pull_request.opened", None, json!({}), "h2"),
    )
    .await;
    let view = f.run(&run).await;
    let pending = view["phases"][0]["waiting_on"]["pending"].to_string();
    assert!(
        pending.contains("pull_request.open") && pending.contains("pull_request.base"),
        "observations under h1 stop counting at h2: {pending}"
    );
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
    assert_eq!(f.at(&run).await.1, "review");
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
async fn a_published_vocabulary_extends_what_charters_and_observations_may_name() {
    let f = Fixture::new();
    let vocab = "vocabulary acme v1\nphrase acme.deploy_gate \"the deployment gate passed\":\n    ceiling hard\n";
    let out = f.call("charter.vocabulary", json!({"source": vocab})).await;
    assert_eq!(out["published"], true, "{out}");
    let charter = "charter ship v1\nsubject release\nroles:\n    operator\nworkflow:\n    phase gate by operator:\n        require acme.deploy_gate hard\n        complete\n";
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
    let shadow = "vocabulary other v1\nphrase acme.deploy_gate \"shadow\":\n    ceiling hard\n";
    let err = f
        .refused("charter.vocabulary", json!({"source": shadow}))
        .await;
    assert!(err.contains("entry_conflict"), "{err}");
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
            json!({"charter": "inbox_triage", "subject_kind": "message", "subject_ref": "inbox:seat", "bindings": {"seat": "seat-1"}}),
        )
        .await;
    let run = started["run_id"].as_str().unwrap().to_owned();
    let obs = |kind: &str, outcome: Option<&str>, args: Value| {
        let mut p = json!({"subject_kind": "message", "subject_ref": "inbox:seat", "kind": kind, "args": args});
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
async fn a_revisit_counts_evidence_that_arrived_after_its_previous_visit_ended() {
    let f = Fixture::new();
    let source = "charter gate_loop v1\nsubject pull_request\nroles:\n    author\nworkflow:\n    phase gate:\n        require gate.result(name=box) hard\n        when gate.result(name=box) is fail: goto repair\n        max_rounds 3 then give_up\n        goto done\n    phase repair:\n        action fix.push\n        goto gate\n    phase done:\n        complete\n    phase give_up:\n        terminate \"three gate rounds without a pass\"\n";
    let out = f.call("charter.publish", json!({"source": source})).await;
    assert_eq!(out["published"], true, "{out}");
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
        at("gate.result", "pass", json!({"name": "box"}), "h2"),
    )
    .await;
    assert_eq!(
        f.at(&run).await,
        tuple("open", "repair", "waiting_actor", 1),
        "the repair phase reads no gate result"
    );
    f.call("charter.observe", at("fix.push", "done", json!({}), "h2"))
        .await;
    assert_eq!(
        f.at(&run).await.0,
        "completed",
        "the gate's second visit counts the pass recorded while the run was in repair"
    );
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
    f.call("charter.vocabulary", json!({"source": PR_MERGE_VOCABULARY}))
        .await;
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
